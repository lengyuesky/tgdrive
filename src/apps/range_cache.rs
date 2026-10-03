//! 应用媒体端点的共享小范围字节缓存：跨应用、跨会话命中重复段。
//! 只存内存、按总预算 LRU 逐出；键含内容版本，票据失效时键随之失效。

#[cfg(test)]
#[path = "range_cache_tests.rs"]
mod tests;

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
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl RangeCache {
    /// 只导出汇总计数，不泄露节点、版本或媒体票据。
    pub fn write_metrics(&self, out: &mut String) {
        use std::fmt::Write as _;
        for (name, kind, help, value) in [
            (
                "tgdrive_range_cache_hits_total",
                "counter",
                "小范围字节缓存命中次数",
                self.hits,
            ),
            (
                "tgdrive_range_cache_misses_total",
                "counter",
                "小范围字节缓存未命中次数",
                self.misses,
            ),
            (
                "tgdrive_range_cache_evictions_total",
                "counter",
                "小范围字节缓存逐出次数",
                self.evictions,
            ),
            (
                "tgdrive_range_cache_entries",
                "gauge",
                "小范围字节缓存条目数",
                self.map.len() as u64,
            ),
            (
                "tgdrive_range_cache_bytes",
                "gauge",
                "小范围字节缓存正文大小，不含元数据",
                self.budget as u64,
            ),
            (
                "tgdrive_range_cache_budget_bytes",
                "gauge",
                "小范围字节缓存正文预算",
                RANGE_CACHE_BUDGET as u64,
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP {name} {help}。\n# TYPE {name} {kind}\n{name} {value}"
            );
        }
    }

    pub fn get(&mut self, key: &RangeKey) -> Option<&CachedRange> {
        if !self.map.contains_key(key) {
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        self.hits = self.hits.saturating_add(1);
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
                        self.evictions = self.evictions.saturating_add(1);
                    }
                }
                None => break,
            }
        }
    }
}
