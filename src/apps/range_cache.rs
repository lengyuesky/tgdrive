//! 应用媒体端点的共享小范围字节缓存：跨应用、跨会话命中重复段。
//! 只存内存、按总预算 LRU 逐出；键含内容版本，票据失效时键随之失效。

use bytes::Bytes;
use std::collections::{HashMap, VecDeque};

/// 单 Range 超过该长度不入缓存，直接流式透传。
pub const RANGE_CACHE_ENTRY: u64 = 2 * 1024 * 1024;
/// 总预算；重启即清，不落盘。
pub const RANGE_CACHE_BUDGET: usize = 64 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RangeKey {
    pub node: i64,
    pub version: String,
    pub start: u64,
    pub len: u64,
}

pub struct CachedRange {
    pub mime: String,
    pub disposition: String,
    pub bytes: Bytes,
}

#[derive(Default)]
pub struct RangeCache {
    map: HashMap<RangeKey, CachedRange>,
    order: VecDeque<RangeKey>,
    budget: usize,
}

impl RangeCache {
    pub fn get(&mut self, key: &RangeKey) -> Option<&CachedRange> {
        if !self.map.contains_key(key) {
            return None;
        }
        if let Some(position) = self.order.iter().position(|k| k == key) {
            self.order.remove(position);
            self.order.push_back(key.clone());
        }
        self.map.get(key)
    }

    pub fn put(&mut self, key: RangeKey, value: CachedRange) {
        let size = value.bytes.len();
        if size > RANGE_CACHE_BUDGET {
            return;
        }
        if let Some(old) = self.map.insert(key.clone(), value) {
            self.budget -= old.bytes.len();
            if let Some(position) = self.order.iter().position(|k| *k == key) {
                self.order.remove(position);
            }
        }
        self.order.push_back(key);
        self.budget += size;
        while self.budget > RANGE_CACHE_BUDGET {
            match self.order.pop_front() {
                Some(oldest) => {
                    if let Some(old) = self.map.remove(&oldest) {
                        self.budget -= old.bytes.len();
                    }
                }
                None => break,
            }
        }
    }
}
