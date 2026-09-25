//! HTTP 安全辅助：认证失败限流等不依赖业务数据的进程内状态。

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const MAX_BUCKETS: usize = 4096;
/// 桶表写满后新键共用的兜底桶：攻击者轮换大量来源地址也不能绕过限流。
const OVERFLOW_KEY: &str = "\0overflow";

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
    trusted_proxies: Arc<Vec<IpCidr>>,
}

impl AttemptLimiter {
    pub fn with_trusted_proxies(trusted_proxies: Vec<IpCidr>) -> Self {
        Self {
            buckets: Arc::default(),
            trusted_proxies: Arc::new(trusted_proxies),
        }
    }

    /// 限流用的客户端地址。直连地址属于可信反代时，从 `X-Forwarded-For`
    /// 自右向左取第一个非可信地址；否则一律用直连地址，防止伪造头绕过限流。
    pub fn client_ip(&self, peer: IpAddr, headers: &axum::http::HeaderMap) -> IpAddr {
        let trusted = |ip: &IpAddr| self.trusted_proxies.iter().any(|net| net.contains(ip));
        if !trusted(&peer) {
            return peer;
        }
        let mut client = peer;
        for value in headers.get_all("x-forwarded-for").iter().rev() {
            let Ok(value) = value.to_str() else {
                return client;
            };
            for hop in value.rsplit(',') {
                let Ok(ip) = hop.trim().parse::<IpAddr>() else {
                    return client;
                };
                client = ip;
                if !trusted(&ip) {
                    return ip;
                }
            }
        }
        client
    }

    /// 已达到上限时返回还需等待的秒数。
    pub fn retry_after(&self, key: &str, limit: usize, window_secs: i64) -> Option<u64> {
        let now = crate::db::now_ts();
        let mut buckets = self.buckets.lock().unwrap();
        let key = if buckets.contains_key(key) {
            key
        } else {
            OVERFLOW_KEY
        };
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
        // 新键的失败全部记入共用兜底桶，既有热点键继续按各自的桶受限。
        let mut key = key;
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(key) {
            buckets.retain(|_, bucket| bucket.last_seen >= now - 3600);
            if buckets.len() >= MAX_BUCKETS {
                key = OVERFLOW_KEY;
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

/// 单个 IP 或 CIDR 网段，用于配置可信反向代理。
#[derive(Clone, Debug, PartialEq)]
pub struct IpCidr {
    addr: IpAddr,
    prefix: u8,
}

impl IpCidr {
    pub fn contains(&self, ip: &IpAddr) -> bool {
        let ip = match (self.addr, ip) {
            (IpAddr::V4(_), IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => return false,
            },
            _ => *ip,
        };
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for IpCidr {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (addr, prefix) = match value.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (value, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| format!("无效地址: {value}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("无效前缀: {value}"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
}

/// 解析逗号分隔的可信代理列表。
pub fn parse_trusted_proxies(value: &str) -> Result<Vec<IpCidr>, String> {
    value
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .map(str::parse)
        .collect()
}

/// 跨站写请求拦截：Cookie 会被浏览器自动附带，只靠 SameSite=Lax 不够。
/// 浏览器会标注 `Sec-Fetch-Site`，缺失时（旧浏览器）退回比较 `Origin` 与 Host；
/// 两者都没有的请求来自命令行等非浏览器客户端，不受 CSRF 影响。
pub async fn csrf_guard(request: Request, next: Next) -> Response {
    if !request.method().is_safe() && is_cross_site(request.headers()) {
        return crate::error::AppError::Forbidden("拒绝跨站请求".into()).into_response();
    }
    next.run(request).await
}

fn is_cross_site(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return !matches!(site.to_str(), Ok("same-origin" | "none"));
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(origin_host) = origin.split_once("://").map(|(_, rest)| rest) else {
        return true;
    };
    let host_matches = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|host| host.trim().eq_ignore_ascii_case(origin_host))
    };
    !(host_matches("host") || host_matches("x-forwarded-host"))
}

const SLOW_REQUEST: std::time::Duration = std::time::Duration::from_secs(2);

/// 进程内 HTTP 指标（Prometheus 文本格式由 /api/metrics 输出）。
/// 按状态码类别计数，耗时只累计总和，足够算平均延迟又不引入依赖。
static HTTP_BY_CLASS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];
static HTTP_DURATION_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HTTP_SLOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 追加 HTTP 相关指标。
pub fn write_http_metrics(out: &mut String) {
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering::Relaxed;
    out.push_str("# HELP tgdrive_http_requests_total 已完成的 HTTP 请求数（按状态码类别）。\n# TYPE tgdrive_http_requests_total counter\n");
    for (index, counter) in HTTP_BY_CLASS.iter().enumerate() {
        let _ = writeln!(
            out,
            "tgdrive_http_requests_total{{class=\"{}xx\"}} {}",
            index + 1,
            counter.load(Relaxed)
        );
    }
    let _ = writeln!(
        out,
        "# HELP tgdrive_http_request_duration_seconds_sum 请求到响应头的累计耗时。\n# TYPE tgdrive_http_request_duration_seconds_sum counter\ntgdrive_http_request_duration_seconds_sum {:.3}",
        HTTP_DURATION_MS.load(Relaxed) as f64 / 1000.0
    );
    let _ = writeln!(
        out,
        "# HELP tgdrive_http_slow_requests_total 超过 2 秒的请求数。\n# TYPE tgdrive_http_slow_requests_total counter\ntgdrive_http_slow_requests_total {}",
        HTTP_SLOW.load(Relaxed)
    );
}
static REQUEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 访问日志与请求 ID。错误响应与慢请求（到响应头为止）按 INFO/WARN 输出，
/// 其余只在 DEBUG 输出，避免媒体 Range 与事件流刷屏。路径不含查询串，
/// 分享令牌与媒体票据被遮蔽，日志里不留凭据。
pub async fn access_log(mut request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{:x}-{:x}",
                crate::db::now_ts(),
                REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        });
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert("x-request-id", value);
    }
    let method = request.method().clone();
    let path = redact_path(request.uri().path());
    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    let ms = elapsed.as_millis() as u64;
    {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(counter) = HTTP_BY_CLASS.get(usize::from(status / 100).wrapping_sub(1)) {
            counter.fetch_add(1, Relaxed);
        }
        HTTP_DURATION_MS.fetch_add(ms, Relaxed);
        if elapsed >= SLOW_REQUEST {
            HTTP_SLOW.fetch_add(1, Relaxed);
        }
    }
    if status >= 500 {
        tracing::warn!(%request_id, %method, %path, status, ms, "请求失败");
    } else if status >= 400 || elapsed >= SLOW_REQUEST {
        tracing::info!(%request_id, %method, %path, status, ms, "请求");
    } else {
        tracing::debug!(%request_id, %method, %path, status, ms, "请求");
    }
    response
}

/// 遮蔽路径中充当凭据的段：`/api/share/{token}`、`/api/apps/media/{ticket}`。
fn redact_path(path: &str) -> String {
    let mut out = Vec::new();
    let mut redact_next = false;
    for (index, segment) in path.split('/').enumerate() {
        if redact_next && !segment.is_empty() {
            out.push("***");
            redact_next = false;
            continue;
        }
        redact_next = index > 0 && matches!(segment, "share" | "media");
        out.push(segment);
    }
    out.join("/")
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

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            map.append(*k, v.parse().unwrap());
        }
        map
    }

    #[test]
    fn access_log_masks_credential_segments() {
        assert_eq!(redact_path("/api/share/abc123/list"), "/api/share/***/list");
        assert_eq!(redact_path("/api/apps/media/tkt"), "/api/apps/media/***");
        assert_eq!(redact_path("/api/fs/list"), "/api/fs/list");
    }

    #[test]
    fn cross_site_detection() {
        assert!(!is_cross_site(&headers(&[])));
        assert!(!is_cross_site(&headers(&[(
            "sec-fetch-site",
            "same-origin"
        )])));
        assert!(is_cross_site(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(is_cross_site(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(!is_cross_site(&headers(&[
            ("origin", "https://pan.example"),
            ("host", "pan.example")
        ])));
        assert!(!is_cross_site(&headers(&[
            ("origin", "https://pan.example"),
            ("host", "127.0.0.1:8080"),
            ("x-forwarded-host", "pan.example"),
        ])));
        assert!(is_cross_site(&headers(&[
            ("origin", "https://evil.example"),
            ("host", "pan.example")
        ])));
        assert!(is_cross_site(&headers(&[
            ("origin", "null"),
            ("host", "pan.example")
        ])));
    }

    #[test]
    fn client_ip_only_trusts_configured_proxies() {
        let peer: IpAddr = "10.0.0.2".parse().unwrap();
        let xff = headers(&[("x-forwarded-for", "6.6.6.6, 1.2.3.4, 10.0.0.9")]);
        assert_eq!(AttemptLimiter::default().client_ip(peer, &xff), peer);
        let limiter =
            AttemptLimiter::with_trusted_proxies(parse_trusted_proxies("10.0.0.0/8").unwrap());
        // 最左侧可由客户端伪造，取最右侧第一个非可信地址。
        assert_eq!(
            limiter.client_ip(peer, &xff),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(limiter.client_ip(peer, &headers(&[])), peer);
        let outside: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(limiter.client_ip(outside, &xff), outside);
    }

    #[test]
    fn cidr_parsing_and_matching() {
        let net: IpCidr = "172.16.0.0/12".parse().unwrap();
        assert!(net.contains(&"172.20.1.1".parse().unwrap()));
        assert!(!net.contains(&"172.32.0.1".parse().unwrap()));
        assert!(net.contains(&"::ffff:172.20.1.1".parse().unwrap()));
        let v6: IpCidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains(&"fd12::1".parse().unwrap()));
        assert!("1.2.3.4/33".parse::<IpCidr>().is_err());
        assert!(parse_trusted_proxies(" ").unwrap().is_empty());
    }

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

    #[test]
    fn full_table_limits_new_keys_through_overflow_bucket() {
        let limiter = AttemptLimiter::default();
        for index in 0..MAX_BUCKETS {
            limiter.record_failure(&format!("seed:{index}"), 60);
        }
        // 表已满且没有过期桶：轮换新键也会累积到兜底桶并被拦下。
        for index in 0..3 {
            let key = format!("fresh:{index}");
            assert!(limiter.retry_after(&key, 3, 60).is_none());
            limiter.record_failure(&key, 60);
        }
        assert!(limiter.retry_after("fresh:another", 3, 60).is_some());
        // 已有桶的键仍按自己的计数判断。
        assert!(limiter.retry_after("seed:0", 3, 60).is_none());
    }
}
