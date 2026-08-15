//! HTTP 安全辅助：认证失败限流等不依赖业务数据的进程内状态。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

const MAX_BUCKETS: usize = 4096;

#[derive(Default)]
struct Bucket {
    failures: VecDeque<i64>,
    last_seen: i64,
}

/// 小型滑动窗口失败限流器。
///
/// 只记录失败，不会因为正常浏览消耗额度；状态无需持久化，服务重启后自然清空。
#[derive(Clone, Default)]
pub struct AttemptLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
}

impl AttemptLimiter {
    /// 已达到上限时返回还需等待的秒数。
    pub fn retry_after(&self, key: &str, limit: usize, window_secs: i64) -> Option<u64> {
        let now = crate::db::now_ts();
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.get_mut(key)?;
        prune(bucket, now, window_secs);
        if bucket.failures.len() < limit {
            return None;
        }
        let first = *bucket.failures.front().unwrap_or(&now);
        Some((first + window_secs - now).max(1) as u64)
    }

    pub fn record_failure(&self, key: &str, window_secs: i64) {
        let now = crate::db::now_ts();
        let mut buckets = self.buckets.lock().unwrap();

        // 防止大量一次性键无限占用内存。优先清掉一小时未活动的桶；仍满时
        // 不再为新键建桶，但既有热点键继续受限。
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(key) {
            buckets.retain(|_, bucket| bucket.last_seen >= now - 3600);
            if buckets.len() >= MAX_BUCKETS {
                return;
            }
        }

        let bucket = buckets.entry(key.to_string()).or_default();
        prune(bucket, now, window_secs);
        bucket.failures.push_back(now);
        bucket.last_seen = now;
    }

    pub fn clear(&self, key: &str) {
        self.buckets.lock().unwrap().remove(key);
    }
}

fn prune(bucket: &mut Bucket, now: i64, window_secs: i64) {
    let cutoff = now - window_secs;
    while bucket.failures.front().is_some_and(|at| *at <= cutoff) {
        bucket.failures.pop_front();
    }
    bucket.last_seen = now;
}

/// 为 API、WebDAV 与静态前端统一补齐浏览器安全响应头。
pub async fn response_headers(State(hsts): State<bool>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    insert_if_absent(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert_if_absent(headers, header::X_FRAME_OPTIONS, "DENY");
    insert_if_absent(headers, header::REFERRER_POLICY, "no-referrer");
    insert_if_absent(
        headers,
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=()",
    );
    insert_if_absent(
        headers,
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    );
    insert_if_absent(
        headers,
        HeaderName::from_static("cross-origin-resource-policy"),
        "same-origin",
    );
    insert_if_absent(
        headers,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'none'; form-action 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; worker-src 'self' blob:",
    );
    if hsts {
        insert_if_absent(
            headers,
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000",
        );
    }
    response
}

fn insert_if_absent(headers: &mut axum::http::HeaderMap, name: HeaderName, value: &'static str) {
    if !headers.contains_key(&name) {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_reach_limit_and_success_can_clear() {
        let limiter = AttemptLimiter::default();
        for _ in 0..3 {
            assert!(limiter.retry_after("a", 3, 60).is_none());
            limiter.record_failure("a", 60);
        }
        assert!(limiter.retry_after("a", 3, 60).is_some());
        limiter.clear("a");
        assert!(limiter.retry_after("a", 3, 60).is_none());
    }
}
