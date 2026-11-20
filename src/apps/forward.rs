//! 三类后端入口的身份、Cookie 隔离、可信上下文与流式转发；不解析应用业务协议。
use super::backend;
use super::runtime::CallerSession;
use super::{AppPlatform, PlatformError, PlatformResult, opaque};
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tgdrive_contract::{error_code, http::*, runtime::*};

pub(crate) async fn root(platform: Arc<AppPlatform>, request: Request) -> Response {
    let result = root_inner(platform, request).await;
    secure(result.unwrap_or_else(IntoResponse::into_response))
}
async fn root_inner(platform: Arc<AppPlatform>, request: Request) -> PlatformResult<Response> {
    validate_path(request.uri().path())?;
    if crate::web::is_admin_path(request.uri().path()) {
        return Err(PlatformError::new(error_code::NOT_FOUND, "宿主接口不存在"));
    }
    let selection = platform.root_selection().await?;
    let Some(id) = selection.app_id else {
        return Ok(Response::builder()
            .status(if matches!(*request.method(), Method::GET | Method::HEAD) {
                StatusCode::FOUND
            } else {
                StatusCode::SEE_OTHER
            })
            .header(header::LOCATION, "/admin/")
            .header("x-tgdrive-request-id", opaque())
            .body(Body::empty())
            .expect("固定重定向有效"));
    };
    forward(platform, None, id, ForwardEntry::Root, request).await
}
pub(crate) async fn page(
    platform: Arc<AppPlatform>,
    caller: Arc<CallerSession>,
    request: Request,
) -> PlatformResult<Response> {
    forward(
        platform,
        Some(caller.clone()),
        caller.app_id.clone(),
        ForwardEntry::Page,
        request,
    )
    .await
}
pub(crate) async fn prefix(
    platform: Arc<AppPlatform>,
    id: String,
    request: Request,
) -> PlatformResult<Response> {
    forward(platform, None, id, ForwardEntry::Prefix, request)
        .await
        .map(secure)
}
async fn forward(
    platform: Arc<AppPlatform>,
    caller: Option<Arc<CallerSession>>,
    id: String,
    entry: ForwardEntry,
    request: Request,
) -> PlatformResult<Response> {
    // 首响应预算包含按需启动和请求上传，不给每个步骤重新开始一个 60 秒窗口。
    let page_cancel = caller
        .as_ref()
        .map(|caller| caller.cancelled.clone())
        .unwrap_or_default();
    let owner = caller.is_some();
    let mut revocations = platform.host.auth.revocation_signal();
    tokio::select! {
        _ = page_cancel.cancelled(), if owner => Err(PlatformError::new(error_code::SESSION_EXPIRED, "应用页面已关闭")),
        _ = revocations.changed(), if owner => Err(PlatformError::new(error_code::SESSION_EXPIRED, "主人登录已撤销")),
        result = tokio::time::timeout(Duration::from_secs(PROXY_FIRST_RESPONSE_SECONDS), forward_inner(platform, caller, id, entry, request)) => result.map_err(|_| PlatformError::new(error_code::REQUEST_TIMEOUT, "后端首响应超过时限"))?,
    }
}
async fn forward_inner(
    platform: Arc<AppPlatform>,
    caller: Option<Arc<CallerSession>>,
    id: String,
    entry: ForwardEntry,
    request: Request,
) -> PlatformResult<Response> {
    let method = request.method().clone();
    if !matches!(
        method.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    ) || request.headers().contains_key(header::UPGRADE)
    {
        return Err(PlatformError::new(
            error_code::METHOD_NOT_ALLOWED,
            "不支持此转发方法或协议升级",
        ));
    }
    validate_path(request.uri().path())?;
    if request.uri().to_string().len() as u64 > MAX_URI_BYTES
        || header_bytes(request.headers()) > MAX_HEADER_BYTES
    {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "转发地址或头超过上限",
        ));
    }
    if platform.host.restore.maintenance() || platform.host.restore.shutdown.is_cancelled() {
        return Err(PlatformError::new(
            error_code::HOST_MAINTENANCE,
            "宿主正在维护",
        ));
    }
    let installed = platform.installed(&id).await.map_err(|_| unavailable())?;
    let backend = installed.manifest.backend.ok_or_else(unavailable)?;
    if !installed.enabled || (entry != ForwardEntry::Page && !backend.external_service) {
        return Err(unavailable());
    }
    if let Some(caller) = &caller {
        if !caller.is_page() {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "页面入口只接受页面会话",
            ));
        }
        if caller.generation != installed.generation || caller.cancelled.is_cancelled() {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "应用页面身份已失效",
            ));
        }
    }
    let public_origin = super::http::public_origin(&platform, &request)?;
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|peer| {
            platform
                .host
                .attempts
                .client_ip(peer.0.ip(), request.headers())
                .to_string()
        });
    let prefix = match entry {
        ForwardEntry::Page => format!("{SELF_API_BASE}/backend"),
        ForwardEntry::Prefix => format!("{SERVICE_BASE}/{id}"),
        ForwardEntry::Root => String::new(),
    };
    let raw = request.uri().path();
    let path = if entry == ForwardEntry::Root {
        raw
    } else {
        raw.strip_prefix(&prefix)
            .filter(|path| path.starts_with('/') || path.is_empty())
            .ok_or_else(|| PlatformError::new(error_code::INVALID_PATH, "转发前缀无效"))?
    };
    let path = if path.is_empty() { "/" } else { path }.to_owned();
    let upstream = request
        .uri()
        .query()
        .map_or_else(|| path.clone(), |query| format!("{path}?{query}"));
    let request_id = opaque();
    let public_base = format!("{public_origin}{prefix}");
    let mut headers = request.headers().clone();
    strip_request(&platform, &mut headers, entry);
    headers.insert(header::HOST, HeaderValue::from_static("tgdrive"));
    let (parts, body) = request.into_parts();
    let page_slot = caller
        .as_ref()
        .map(|caller| caller.control_slot())
        .transpose()?;
    let run = platform
        .backends
        .ensure(&platform, &id)
        .await
        .map_err(|_| unavailable())?;
    let activity = run.activity();
    let proxy_slot =
        run.proxy_slots.clone().try_acquire_owned().map_err(|_| {
            PlatformError::limited(error_code::RATE_LIMITED, "后端代理并发已满", 50)
        })?;
    let forward = ForwardContext {
        contract: *run.negotiated.read().unwrap(),
        app_id: id,
        caller: if entry == ForwardEntry::Page {
            ForwardCaller::Owner
        } else {
            ForwardCaller::External
        },
        entry,
        request_id: request_id.clone(),
        original_uri: parts
            .uri
            .path_and_query()
            .map_or("/", |uri| uri.as_str())
            .to_owned(),
        public_origin: public_origin.clone(),
        public_base_url: public_base.clone(),
        client_ip,
    };
    let value =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forward)?);
    headers.insert(
        FORWARD_CONTEXT_HEADER,
        HeaderValue::from_str(&value)
            .map_err(|_| PlatformError::new(error_code::PAYLOAD_TOO_LARGE, "转发上下文超过上限"))?,
    );
    if header_bytes(&headers) > MAX_HEADER_BYTES {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "转发上下文与请求头超过上限",
        ));
    }
    let overflow = Arc::new(AtomicBool::new(false));
    let body = limited_request(body, &headers, overflow.clone())?;
    let mut upstream_request = hyper::Request::builder()
        .method(method.clone())
        .uri(&upstream)
        .body(body)
        .map_err(|_| PlatformError::new(error_code::INVALID_PATH, "转发 URI 无效"))?;
    *upstream_request.headers_mut() = headers;
    let response = match backend::request(&run, upstream_request).await {
        Ok(response) => response,
        Err(_) if overflow.load(Ordering::Acquire) => {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "代理实际正文超过上限",
            ));
        }
        Err(error) => return Err(error),
    };
    if overflow.load(Ordering::Acquire) {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "代理实际正文超过上限",
        ));
    }
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        || header_bytes(response.headers()) > MAX_HEADER_BYTES
    {
        return Err(invalid_backend());
    }
    if response.headers().contains_key(STREAM_DIRECTIVE_HEADER) {
        let directive = decode_directive(&method, &response)?;
        let mut extra = response.headers().clone();
        strip_response(&mut extra, entry);
        let bytes = axum::body::to_bytes(response.into_body(), 1)
            .await
            .map_err(|_| invalid_backend())?;
        if !bytes.is_empty() {
            return Err(invalid_backend());
        }
        let mut result = platform
            .directive_response(
                run.caller.clone(),
                directive,
                &parts.headers,
                &method,
                request_id,
            )
            .await?;
        for name in [
            "content-length",
            "content-range",
            "content-encoding",
            "accept-ranges",
            "etag",
            "content-disposition",
            "content-type",
            "cache-control",
            "content-security-policy",
            "x-frame-options",
            "x-content-type-options",
        ] {
            extra.remove(name);
        }
        for (name, value) in extra.iter() {
            result.headers_mut().append(name.clone(), value.clone());
        }
        drop((activity, proxy_slot, page_slot));
        return Ok(if entry == ForwardEntry::Page {
            result
        } else {
            secure(result)
        });
    }
    let (mut response_parts, body) = response.into_parts();
    let locations = response_parts
        .headers
        .get_all(header::LOCATION)
        .iter()
        .collect::<Vec<_>>();
    if locations.len() > 1 {
        return Err(invalid_backend());
    }
    if let Some(location) = locations.first() {
        validate_location(
            location.to_str().map_err(|_| invalid_backend())?,
            &public_base,
            &public_origin,
            &forward.app_id,
            entry,
            &path,
        )?;
    }
    strip_response(&mut response_parts.headers, entry);
    response_parts.headers.insert(
        "x-tgdrive-request-id",
        HeaderValue::from_str(&request_id).expect("随机 ID 为 ASCII"),
    );
    let cancelled = run.caller.cancelled.clone();
    let page_cancel = caller
        .as_ref()
        .map(|caller| caller.cancelled.clone())
        .unwrap_or_default();
    let revocations = platform.host.auth.revocation_signal();
    let stream = futures_util::stream::try_unfold(
        (
            Box::pin(body.into_data_stream()),
            activity,
            proxy_slot,
            page_slot,
            cancelled,
            page_cancel,
            caller,
            revocations,
        ),
        |(
            mut stream,
            activity,
            proxy_slot,
            page_slot,
            cancelled,
            page_cancel,
            caller,
            mut revocations,
        )| async move {
            let expiry = caller
                .as_ref()
                .map(|caller| caller.expires_at.load(Ordering::Acquire))
                .unwrap_or(i64::MAX);
            let deadline =
                Duration::from_millis(expiry.saturating_sub(super::now().0).max(0) as u64);
            let next = tokio::select! {
                _ = cancelled.cancelled() => return Err(std::io::Error::other("后端启动轮次已撤销")),
                _ = page_cancel.cancelled() => return Err(std::io::Error::other("应用页面已关闭")),
                _ = revocations.changed(), if caller.is_some() => return Err(std::io::Error::other("主人登录已撤销")),
                _ = tokio::time::sleep(deadline), if caller.is_some() => return Err(std::io::Error::other("页面会话已过期")),
                next = tokio::time::timeout(Duration::from_secs(STREAM_IDLE_TIMEOUT_SECONDS), stream.next()) => next.map_err(|_| std::io::Error::other("后端流空闲超时"))?,
            };
            match next {
                Some(Ok(bytes)) => Ok(Some((
                    bytes,
                    (
                        stream,
                        activity,
                        proxy_slot,
                        page_slot,
                        cancelled,
                        page_cancel,
                        caller,
                        revocations,
                    ),
                ))),
                Some(Err(_)) => Err(std::io::Error::other("后端字节流中断")),
                None => Ok(None),
            }
        },
    );
    let response = Response::from_parts(response_parts, Body::from_stream(stream));
    Ok(if entry == ForwardEntry::Page {
        response
    } else {
        secure(response)
    })
}
fn limited_request(
    body: Body,
    headers: &HeaderMap,
    overflow: Arc<AtomicBool>,
) -> PlatformResult<Body> {
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_PROXY_BODY_BYTES)
    {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "普通代理正文超过上限",
        ));
    }
    let stream = futures_util::stream::try_unfold(
        (Box::pin(body.into_data_stream()), 0u64, overflow),
        |(mut stream, used, overflow)| async move {
            match tokio::time::timeout(
                Duration::from_secs(STREAM_IDLE_TIMEOUT_SECONDS),
                stream.next(),
            )
            .await
            .map_err(|_| std::io::Error::other("代理请求正文空闲超时"))?
            {
                Some(Ok(bytes)) => {
                    let used = used.saturating_add(bytes.len() as u64);
                    if used > MAX_PROXY_BODY_BYTES {
                        overflow.store(true, Ordering::Release);
                        return Err(std::io::Error::other("代理正文超过上限"));
                    }
                    Ok(Some((bytes, (stream, used, overflow))))
                }
                Some(Err(_)) => Err(std::io::Error::other("代理正文中断")),
                None => Ok(None),
            }
        },
    );
    Ok(Body::from_stream(stream))
}
fn unavailable() -> PlatformError {
    PlatformError::new(error_code::BACKEND_UNAVAILABLE, "应用后端暂时不可用")
}
fn invalid_backend() -> PlatformError {
    PlatformError::new(
        error_code::INVALID_BACKEND_RESPONSE,
        "后端控制响应不符合契约",
    )
}
fn decode_directive(
    method: &Method,
    response: &hyper::Response<Body>,
) -> PlatformResult<tgdrive_contract::files::StreamDirective> {
    if response.status() != StatusCode::NO_CONTENT || !matches!(method.as_str(), "GET" | "HEAD") {
        return Err(invalid_backend());
    }
    if response.headers().contains_key(header::TRANSFER_ENCODING)
        || response
            .headers()
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .any(|value| value != "0")
    {
        return Err(invalid_backend());
    }
    let mut values = response.headers().get_all(STREAM_DIRECTIVE_HEADER).iter();
    let value = values.next().ok_or_else(invalid_backend)?;
    if values.next().is_some() {
        return Err(invalid_backend());
    }
    let value = value
        .to_str()
        .map_err(|_| invalid_backend())?
        .strip_prefix(STREAM_DIRECTIVE_PREFIX)
        .ok_or_else(invalid_backend)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid_backend())?;
    if bytes.len() as u64 > MAX_STREAM_DIRECTIVE_BYTES
        || base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes) != value
    {
        return Err(invalid_backend());
    }
    let raw =
        tgdrive_contract::validation::parse_unique_json(&bytes).map_err(|_| invalid_backend())?;
    if !raw.is_object()
        || !raw.get("file").is_some_and(serde_json::Value::is_object)
        || raw
            .get("window")
            .is_some_and(|window| !window.is_null() && !window.is_object())
    {
        return Err(invalid_backend());
    }
    let directive = serde_json::from_value(raw).map_err(|_| invalid_backend())?;
    tgdrive_contract::validation::validate_stream_directive(&directive, u64::MAX)?;
    Ok(directive)
}
fn header_bytes(headers: &HeaderMap) -> u64 {
    headers
        .iter()
        .map(|(name, value)| (name.as_str().len() + value.as_bytes().len() + 4) as u64)
        .sum()
}
pub(crate) fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(EXTERNAL_CSP),
    );
    headers.remove("content-security-policy-report-only");
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
    headers
        .entry("x-tgdrive-request-id")
        .or_insert_with(|| HeaderValue::from_str(&opaque()).expect("随机 ID 为 ASCII"));
    response
}
fn strip_hop(headers: &mut HeaderMap) {
    let names = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| {
            value
                .split(',')
                .map(|token| token.trim().to_ascii_lowercase())
        })
        .collect::<Vec<_>>();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
fn host_cookie(name: &str) -> bool {
    name.starts_with(RESERVED_COOKIE_PREFIX)
}
fn strip_request(platform: &AppPlatform, headers: &mut HeaderMap, entry: ForwardEntry) {
    strip_hop(headers);
    let remove = headers
        .keys()
        .filter(|name| {
            name.as_str().starts_with("x-tgdrive-")
                || name.as_str() == "forwarded"
                || name.as_str().starts_with("x-forwarded-")
        })
        .cloned()
        .collect::<Vec<_>>();
    for name in remove {
        headers.remove(name);
    }
    if entry == ForwardEntry::Page {
        headers.remove(header::AUTHORIZATION);
        headers.remove(header::COOKIE);
        return;
    }
    let host_bearer = headers.get_all(header::AUTHORIZATION).iter().filter_map(|value| value.to_str().ok()).any(|value| {
        let mut words = value.split_ascii_whitespace();
        matches!((words.next(), words.next(), words.next()), (Some(scheme), Some(token), None) if scheme.eq_ignore_ascii_case("Bearer") && (platform.host.auth.owns_native_bearer(token) || platform.sessions.owns(&platform.cursors, token)))
    });
    if host_bearer {
        headers.remove(header::AUTHORIZATION);
    }
    if entry == ForwardEntry::Prefix {
        headers.remove(header::COOKIE);
    } else {
        let value = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| {
                let pair = pair.trim();
                let (name, _) = pair.split_once('=')?;
                (!host_cookie(name.trim())).then_some(pair.to_owned())
            })
            .collect::<Vec<_>>()
            .join("; ");
        headers.remove(header::COOKIE);
        if !value.is_empty()
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert(header::COOKIE, value);
        }
    }
}
fn strip_response(headers: &mut HeaderMap, entry: ForwardEntry) {
    strip_hop(headers);
    let remove = headers
        .keys()
        .filter(|name| {
            name.as_str().starts_with("x-tgdrive-")
                || matches!(
                    name.as_str(),
                    "content-security-policy-report-only"
                        | "report-to"
                        | "reporting-endpoints"
                        | "nel"
                        | "refresh"
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    for name in remove {
        headers.remove(name);
    }
    let cookies = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    headers.remove(header::SET_COOKIE);
    if entry != ForwardEntry::Root {
        return;
    }
    for cookie in cookies {
        let Ok(value) = cookie.to_str() else {
            continue;
        };
        let mut fields = value.split(';');
        let Some(first) = fields.next() else {
            continue;
        };
        let Some((name, _)) = first.trim().split_once('=') else {
            continue;
        };
        if host_cookie(name.trim())
            || name.trim().is_empty()
            || name
                .trim()
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && !b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            continue;
        }
        let mut value = first.trim().to_owned();
        for field in fields {
            let field = field.trim();
            let key = field.split_once('=').map_or(field, |(key, _)| key).trim();
            if key.eq_ignore_ascii_case("path") || key.eq_ignore_ascii_case("domain") {
                continue;
            }
            value.push_str("; ");
            value.push_str(field);
        }
        value.push_str("; Path=/");
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.append(header::SET_COOKIE, value);
        }
    }
}
pub(crate) fn validate_path(path: &str) -> PlatformResult<()> {
    let invalid = || PlatformError::new(error_code::INVALID_PATH, "请求路由不是安全的规范化路径");
    if !path.starts_with('/') || path.contains("//") {
        return Err(invalid());
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'%' {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            let value = bytes
                .get(position + 1)
                .and_then(|byte| digit(*byte))
                .zip(bytes.get(position + 2).and_then(|byte| digit(*byte)))
                .map(|(a, b)| a * 16 + b)
                .ok_or_else(invalid)?;
            if matches!(value, b'/' | b'\\' | 0..=31 | 127) {
                return Err(invalid());
            }
            decoded.push(value);
            position += 3;
        } else {
            if matches!(byte, b'\\' | 0..=31 | 127) {
                return Err(invalid());
            }
            decoded.push(byte);
            position += 1;
        }
    }
    std::str::from_utf8(&decoded).map_err(|_| invalid())?;
    if decoded
        .split(|byte| *byte == b'/')
        .any(|part| part == b"." || part == b"..")
    {
        return Err(invalid());
    }
    let first = decoded.split(|byte| *byte == b'/').nth(1);
    if first == Some(b"admin".as_slice()) && !crate::web::is_admin_path(path) {
        return Err(invalid());
    }
    Ok(())
}
fn validate_location(
    location: &str,
    base: &str,
    origin: &str,
    id: &str,
    entry: ForwardEntry,
    path: &str,
) -> PlatformResult<()> {
    if location.len() as u64 > MAX_URI_BYTES
        || location
            .chars()
            .any(|character| character.is_control() || character == '\\')
    {
        return Err(invalid_backend());
    }
    let source = reqwest::Url::parse(&format!("{base}{path}")).map_err(|_| invalid_backend())?;
    let target = source.join(location).map_err(|_| invalid_backend())?;
    if !target.username().is_empty()
        || target.password().is_some()
        || !matches!(target.scheme(), "http" | "https")
    {
        return Err(invalid_backend());
    }
    let same = target.origin().ascii_serialization() == origin;
    if !same {
        return if entry != ForwardEntry::Page && target.scheme() == "https" {
            Ok(())
        } else {
            Err(invalid_backend())
        };
    }
    validate_path(target.path()).map_err(|_| invalid_backend())?;
    let page = format!("{SELF_API_BASE}/backend");
    let service = format!("{SERVICE_BASE}/{id}");
    let inside =
        |prefix: &str| target.path() == prefix || target.path().starts_with(&format!("{prefix}/"));
    let allowed = match entry {
        ForwardEntry::Page => inside(&page),
        ForwardEntry::Prefix => inside(&service),
        ForwardEntry::Root => !crate::web::is_admin_path(target.path()) || inside(&service),
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid_backend())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_boundaries_and_redirects_do_not_escape_entries() {
        for path in [
            "/admin/../file",
            "/x/%2e%2e/admin",
            "/x%2fadmin",
            "/x%5cadmin",
            "/%61dmin/api",
            "/x%zz",
            "//admin",
            "/x/%7f",
        ] {
            assert!(validate_path(path).is_err());
        }
        assert!(validate_path("/administrator/file").is_ok());
        assert!(validate_path("/admin/api/apps/v1/services/example/file").is_ok());
        assert!(
            validate_location(
                "/admin/settings",
                "https://drive.invalid",
                "https://drive.invalid",
                "example",
                ForwardEntry::Root,
                "/file"
            )
            .is_err()
        );
        assert!(
            validate_location(
                "https://external.invalid/file",
                "https://drive.invalid/admin/api/apps/v1/self/backend",
                "https://drive.invalid",
                "example",
                ForwardEntry::Page,
                "/owner"
            )
            .is_err()
        );
    }
    #[test]
    fn response_cookie_rules_apply_to_every_cookie_and_ignore_host_paths() {
        let mut headers = HeaderMap::new();
        for cookie in [
            "tgdrive_token=attack; Path=/other",
            "tgdrive_other=attack; Domain=drive.invalid",
            "app_session=good; Path=/admin; Domain=drive.invalid; HttpOnly",
        ] {
            headers.append(header::SET_COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        for entry in [ForwardEntry::Page, ForwardEntry::Prefix] {
            let mut copy = headers.clone();
            strip_response(&mut copy, entry);
            assert!(!copy.contains_key(header::SET_COOKIE));
        }
        strip_response(&mut headers, ForwardEntry::Root);
        let values = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "app_session=good; HttpOnly; Path=/");
    }
}
