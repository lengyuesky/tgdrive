//! 有界路由指标：只接收 Axum 注册模板，不记录 URL、令牌、文件或用户标识。
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::body::{Body, Bytes, HttpBody};
use http_body::{Frame, SizeHint};

const BOUNDS_MS: [u64; 12] = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000];
const MAX_ROUTES: usize = 128;
static ROUTES: LazyLock<Mutex<BTreeMap<String, Arc<RouteMetrics>>>> = LazyLock::new(Mutex::default);

#[derive(Default)]
struct Histogram {
    buckets: [u64; 12],
    count: u64,
    micros: u64,
}
impl Histogram {
    fn observe(&mut self, duration: Duration) {
        self.count += 1;
        self.micros = self
            .micros
            .saturating_add(duration.as_micros().min(u64::MAX as u128) as u64);
        for (bucket, bound) in self.buckets.iter_mut().zip(BOUNDS_MS) {
            if duration <= Duration::from_millis(bound) {
                *bucket += 1;
            }
        }
    }
    fn write(&self, out: &mut String, name: &str, route: &str) {
        self.write_label(out, name, "route", route);
    }
    fn write_label(&self, out: &mut String, name: &str, label: &str, route: &str) {
        for (count, bound) in self.buckets.iter().zip(BOUNDS_MS) {
            let _ = writeln!(
                out,
                "{name}_bucket{{{label}=\"{route}\",le=\"{}\"}} {count}",
                bound as f64 / 1000.0
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{label}=\"{route}\",le=\"+Inf\"}} {}",
            self.count
        );
        let _ = writeln!(out, "{name}_count{{{label}=\"{route}\"}} {}", self.count);
        let _ = writeln!(
            out,
            "{name}_sum{{{label}=\"{route}\"}} {}",
            self.micros as f64 / 1_000_000.0
        );
    }
}

#[derive(Default)]
pub(crate) struct RouteMetrics {
    headers: Mutex<Histogram>,
    body_duration: Mutex<Histogram>,
    active: AtomicU64,
    bytes: AtomicU64,
    outcomes: [AtomicU64; 3],
}

pub(crate) fn route(template: Option<&str>) -> Arc<RouteMetrics> {
    route_in(&mut ROUTES.lock().unwrap(), template)
}

fn route_in(
    routes: &mut BTreeMap<String, Arc<RouteMetrics>>,
    template: Option<&str>,
) -> Arc<RouteMetrics> {
    let template = template
        .filter(|value| value.len() <= 256)
        .unwrap_or("unmatched");
    // 预留一个溢出桶；即使未来增加很多接口，标签集合仍然有上限。
    let key = if routes.contains_key(template) || routes.len() < MAX_ROUTES - 1 {
        template
    } else {
        "other"
    };
    routes.entry(key.to_owned()).or_default().clone()
}

pub(crate) fn observe_headers(metrics: &RouteMetrics, duration: Duration) {
    metrics.headers.lock().unwrap().observe(duration);
}

pub(crate) fn observe_body(body: Body, metrics: Arc<RouteMetrics>) -> Body {
    metrics.active.fetch_add(1, Relaxed);
    let mut body = MeteredBody {
        inner: body,
        metrics,
        started: Instant::now(),
        finished: false,
    };
    if body.inner.is_end_stream() {
        body.finish(0);
    }
    Body::new(body)
}

struct MeteredBody {
    inner: Body,
    metrics: Arc<RouteMetrics>,
    started: Instant,
    finished: bool,
}
impl MeteredBody {
    fn finish(&mut self, outcome: usize) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.metrics.active.fetch_sub(1, Relaxed);
        self.metrics.outcomes[outcome].fetch_add(1, Relaxed);
        self.metrics
            .body_duration
            .lock()
            .unwrap()
            .observe(self.started.elapsed());
    }
}
impl HttpBody for MeteredBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.metrics.bytes.fetch_add(data.len() as u64, Relaxed);
                }
                if self.inner.is_end_stream() {
                    self.finish(0);
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish(1),
            Poll::Ready(None) => self.finish(0),
            Poll::Pending => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
impl Drop for MeteredBody {
    fn drop(&mut self) {
        self.finish(2);
    }
}

#[derive(Default)]
struct TelegramMetrics {
    duration: Mutex<Histogram>,
    errors: AtomicU64,
    floods: AtomicU64,
    retries: AtomicU64,
}
static TELEGRAM: LazyLock<[TelegramMetrics; 2]> = LazyLock::new(Default::default);
static UPLOAD_WAIT: LazyLock<Mutex<Histogram>> = LazyLock::new(Mutex::default);

/// 只计 Bot API 幂等调用和媒体上传的已完成尝试，不含 MTProto 或取消中的请求。
pub(crate) fn telegram_attempt(
    upload: bool,
    duration: Duration,
    failed: bool,
    flood: bool,
    retried: bool,
) {
    let metrics = &TELEGRAM[usize::from(upload)];
    metrics.duration.lock().unwrap().observe(duration);
    if failed {
        metrics.errors.fetch_add(1, Relaxed);
    }
    if flood {
        metrics.floods.fetch_add(1, Relaxed);
    }
    if retried {
        metrics.retries.fetch_add(1, Relaxed);
    }
}

pub(crate) struct UploadWait(Instant);
pub(crate) fn upload_wait() -> UploadWait {
    UploadWait(Instant::now())
}
impl Drop for UploadWait {
    fn drop(&mut self) {
        UPLOAD_WAIT.lock().unwrap().observe(self.0.elapsed());
    }
}

pub(crate) fn write(out: &mut String) {
    out.push_str("# HELP tgdrive_telegram_attempt_seconds Bot API 已完成尝试耗时，不含排队或退避。\n# TYPE tgdrive_telegram_attempt_seconds histogram\n");
    out.push_str("# HELP tgdrive_telegram_errors_total Bot API 已完成的失败尝试数。\n# TYPE tgdrive_telegram_errors_total counter\n");
    out.push_str("# HELP tgdrive_telegram_floods_total Bot API 限流失败尝试数。\n# TYPE tgdrive_telegram_floods_total counter\n");
    out.push_str("# HELP tgdrive_telegram_retries_total Bot API 已完成的非首次尝试数。\n# TYPE tgdrive_telegram_retries_total counter\n");
    out.push_str("# HELP tgdrive_upload_wait_seconds 媒体上传获取发送时隙的实际等待时间，包含取消。\n# TYPE tgdrive_upload_wait_seconds histogram\n");
    UPLOAD_WAIT
        .lock()
        .unwrap()
        .write_label(out, "tgdrive_upload_wait_seconds", "kind", "bot_api");
    for (kind, metrics) in ["api", "upload"].iter().zip(TELEGRAM.iter()) {
        metrics.duration.lock().unwrap().write_label(
            out,
            "tgdrive_telegram_attempt_seconds",
            "kind",
            kind,
        );
        for (name, counter) in [
            ("errors", &metrics.errors),
            ("floods", &metrics.floods),
            ("retries", &metrics.retries),
        ] {
            let _ = writeln!(
                out,
                "tgdrive_telegram_{name}_total{{kind=\"{kind}\"}} {}",
                counter.load(Relaxed)
            );
        }
    }
    out.push_str("# HELP tgdrive_http_headers_seconds 按路由模板统计的请求到响应头耗时。\n# TYPE tgdrive_http_headers_seconds histogram\n");
    out.push_str("# HELP tgdrive_http_body_seconds 从响应头到正文结束、错误或丢弃的耗时。\n# TYPE tgdrive_http_body_seconds histogram\n");
    out.push_str("# HELP tgdrive_http_body_active 尚未结束的响应正文数。\n# TYPE tgdrive_http_body_active gauge\n");
    out.push_str("# HELP tgdrive_http_body_bytes_total 响应正文交给 HTTP 层的累计字节，不代表客户端已收到。\n# TYPE tgdrive_http_body_bytes_total counter\n");
    out.push_str("# HELP tgdrive_http_body_finished_total 正文结束原因：complete、error 或 cancelled（未读完被丢弃）。\n# TYPE tgdrive_http_body_finished_total counter\n");
    let routes: Vec<_> = ROUTES
        .lock()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (route, metrics) in routes {
        let route = route
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        metrics
            .headers
            .lock()
            .unwrap()
            .write(out, "tgdrive_http_headers_seconds", &route);
        metrics
            .body_duration
            .lock()
            .unwrap()
            .write(out, "tgdrive_http_body_seconds", &route);
        let _ = writeln!(
            out,
            "tgdrive_http_body_active{{route=\"{route}\"}} {}",
            metrics.active.load(Relaxed)
        );
        let _ = writeln!(
            out,
            "tgdrive_http_body_bytes_total{{route=\"{route}\"}} {}",
            metrics.bytes.load(Relaxed)
        );
        for (index, outcome) in ["complete", "error", "cancelled"].iter().enumerate() {
            let _ = writeln!(
                out,
                "tgdrive_http_body_finished_total{{route=\"{route}\",outcome=\"{outcome}\"}} {}",
                metrics.outcomes[index].load(Relaxed)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, middleware, routing::get};
    use http_body_util::{BodyExt, StreamBody};
    use tower::ServiceExt;

    #[test]
    fn route_cardinality_is_bounded_and_existing_routes_are_reused() {
        let mut routes = BTreeMap::new();
        let first = route_in(&mut routes, Some("/first/{id}"));
        for index in 0..200 {
            route_in(&mut routes, Some(&format!("/route-{index}")));
        }
        assert_eq!(routes.len(), MAX_ROUTES);
        assert!(Arc::ptr_eq(
            &first,
            &route_in(&mut routes, Some("/first/{id}"))
        ));
        assert!(Arc::ptr_eq(
            &routes["other"].clone(),
            &route_in(&mut routes, None)
        ));
    }

    #[test]
    fn histogram_boundaries_are_cumulative_and_export_seconds() {
        let mut histogram = Histogram::default();
        for duration in [
            Duration::ZERO,
            Duration::from_millis(5),
            Duration::from_micros(5001),
            Duration::from_secs(31),
        ] {
            histogram.observe(duration);
        }
        assert_eq!(histogram.buckets[0], 2);
        assert_eq!(histogram.buckets[1], 3);
        assert_eq!(histogram.count, 4);
        let mut text = String::new();
        histogram.write(&mut text, "latency", "/test");
        assert!(text.contains("latency_bucket{route=\"/test\",le=\"0.005\"} 2"));
        assert!(text.contains("latency_bucket{route=\"/test\",le=\"+Inf\"} 4"));
        assert!(text.contains("latency_sum{route=\"/test\"} 31.010001"));
    }

    #[tokio::test]
    async fn body_keeps_bytes_trailers_and_counts_completion_once() {
        let metrics = Arc::new(RouteMetrics::default());
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-finish", http::HeaderValue::from_static("yes"));
        let stream = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(b"abc"))),
            Ok(Frame::trailers(trailers.clone())),
        ]);
        let body = observe_body(Body::new(StreamBody::new(stream)), metrics.clone());
        assert_eq!(metrics.active.load(Relaxed), 1);
        let collected = body.collect().await.unwrap();
        assert_eq!(collected.trailers(), Some(&trailers));
        assert_eq!(collected.to_bytes(), "abc");
        assert_eq!(metrics.active.load(Relaxed), 0);
        assert_eq!(metrics.bytes.load(Relaxed), 3);
        assert_eq!(metrics.outcomes[0].load(Relaxed), 1);
        assert_eq!(metrics.outcomes[2].load(Relaxed), 0);
    }

    #[tokio::test]
    async fn body_drop_error_empty_and_size_hint_are_distinct() {
        let cancelled = Arc::new(RouteMetrics::default());
        let body = observe_body(Body::from("abc"), cancelled.clone());
        assert_eq!(body.size_hint().exact(), Some(3));
        drop(body);
        assert_eq!(cancelled.outcomes[2].load(Relaxed), 1);
        assert_eq!(cancelled.active.load(Relaxed), 0);
        let failed = Arc::new(RouteMetrics::default());
        let body = Body::from_stream(futures_util::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("读取失败"))
        }));
        assert!(observe_body(body, failed.clone()).collect().await.is_err());
        assert_eq!(failed.outcomes[1].load(Relaxed), 1);
        assert_eq!(failed.outcomes[2].load(Relaxed), 0);
        let empty = Arc::new(RouteMetrics::default());
        drop(observe_body(Body::empty(), empty.clone()));
        assert_eq!(empty.outcomes[0].load(Relaxed), 1);
        assert_eq!(empty.active.load(Relaxed), 0);
    }

    #[tokio::test]
    async fn middleware_uses_template_not_media_ticket_or_query() {
        let app = Router::new()
            .route("/metrics-test/{ticket}", get(|| async { "ok" }))
            .layer(middleware::from_fn(crate::security::access_log));
        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/metrics-test/private-ticket?secret=private-query")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "ok"
        );
        let mut text = String::new();
        write(&mut text);
        assert!(text.contains("route=\"/metrics-test/{ticket}\""));
        assert!(!text.contains("private-ticket"));
        assert!(!text.contains("private-query"));
    }
}
