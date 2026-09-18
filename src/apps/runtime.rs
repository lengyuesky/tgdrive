//! 运行页面不获得登录凭据；能力调用绑定应用版本，媒体凭据只授权单个文件。
//! 控制面走 RPC；二进制数据面由应用凭票据直连本模块的媒体端点。

use super::{
    InstalledApp, cache_dir, files, installed, package,
    range_cache::{CachedRange, RANGE_CACHE_ENTRY, RangeKey},
    read_settings, save_settings, scope, storage,
};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SESSION_TTL: i64 = 6 * 60 * 60;

/// WebKit 在 CSP sandbox（不透明源）文档中不把 connect-src 的 'self' 按服务源匹配，
/// 沙箱应用的 fetch 会被直接拒绝（iOS Safari 上表现为 TypeError: Load failed）。
/// 因此把请求自身的源显式追加进 connect-src；保留 'self' 以兼容按规范实现的引擎。
fn sandbox_csp(host: Option<&HeaderValue>) -> String {
    const BASE: &str = "sandbox allow-scripts; default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self'; connect-src 'self'";
    const TAIL: &str = "; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";
    match host
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|host| {
            (1..=255).contains(&host.len())
                && !host.bytes().any(|b| {
                    b.is_ascii_whitespace()
                        || matches!(b, b'"' | b'\'' | b';' | b'%' | b'<' | b'>' | b'\\')
                })
        }) {
        Some(host) => format!("{BASE} http://{host} https://{host}{TAIL}"),
        None => format!("{BASE}{TAIL}"),
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Claims {
    purpose: String,
    app_id: String,
    revision: String,
    expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_version: Option<String>,
}

fn sign(state: &AppState, claims: &Claims) -> AppResult<String> {
    let encoded = serde_json::to_string(claims).map_err(|e| AppError::Internal(e.into()))?;
    Ok(state.auth.sign_app_claims(&encoded))
}

async fn verify(state: &AppState, token: &str) -> AppResult<(Claims, InstalledApp)> {
    let raw = state
        .auth
        .verify_app_claims(token)
        .ok_or_else(|| AppError::Forbidden("应用凭据无效".into()))?;
    let claims: Claims =
        serde_json::from_str(&raw).map_err(|_| AppError::Forbidden("应用凭据无效".into()))?;
    if claims.expires_at <= crate::db::now_ts() {
        return Err(AppError::SessionExpired);
    }
    let app = installed(state, &claims.app_id).await?;
    if !app.enabled || app.revision != claims.revision {
        return Err(AppError::AppChanged);
    }
    Ok((claims, app))
}

pub(super) async fn launch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let app = installed(&state, &id).await?;
    if !app.enabled {
        return Err(AppError::Conflict("应用已停用，请先在应用中心启用".into()));
    }
    app.manifest.validate()?;
    cache_dir(&state, &app).await?;
    let claims = Claims {
        purpose: "session".into(),
        app_id: id.clone(),
        revision: app.revision.clone(),
        expires_at: crate::db::now_ts() + SESSION_TTL,
        node_id: None,
        content_version: None,
    };
    let frame_url = format!(
        "/api/apps/runtime/{id}/{}/{}",
        app.revision, app.manifest.entry
    );
    Ok(Json(
        json!({"app": app, "session": sign(&state, &claims)?, "frame_url": frame_url, "expires_at": claims.expires_at}),
    ))
}

/// 资源 URL 按修订定址、内容按包摘要物化到不可变目录：同一 URL 的响应永不变化，
/// 因此可长期缓存；ETag 绑定包摘要，作为不支持缓存的浏览器环境的兑底再验证。
fn etag_for(digest: &str, resource: &str) -> String {
    format!("\"{digest}:{resource}\"")
}

fn if_none_match_hits(request_headers: &HeaderMap, etag: &str) -> bool {
    request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == etag || tag == "*"
            })
        })
}

/// 沙箱资源响应公共头：200 与 304 保持一致的安全与缓存校验语义。
fn sandbox_asset_headers(headers: &mut HeaderMap, request_headers: &HeaderMap, etag: &str) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&sandbox_csp(request_headers.get(header::HOST)))
            .expect("沙箱 CSP 值合法"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(etag).expect("ETag 合法"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    // 资源内容随修订轮换不可变：可安全长缓存；private 禁止共享缓存存储。
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
}

pub(super) async fn asset(
    State(state): State<AppState>,
    request_headers: HeaderMap,
    Path((id, revision, resource)): Path<(String, String, String)>,
) -> AppResult<Response> {
    if !package::valid_resource(&resource) {
        return Err(AppError::NotFound);
    }
    let app = installed(&state, &id).await?;
    if !app.enabled || app.revision != revision {
        return Err(AppError::NotFound);
    }
    let etag = etag_for(&app.digest, &resource);
    if if_none_match_hits(&request_headers, &etag) {
        let mut response = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("构造再验证响应失败");
        sandbox_asset_headers(response.headers_mut(), &request_headers, &etag);
        return Ok(response);
    }
    let directory = cache_dir(&state, &app).await?;
    let bytes = tokio::fs::read(directory.join(&resource))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::NotFound
            } else {
                error.into()
            }
        })?;
    let mime = mime_guess::from_path(&resource)
        .first_or_octet_stream()
        .to_string();
    let mut response = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
    // 即使用户直接打开资源地址，响应级 sandbox 也不会授予主站同源权限。
    sandbox_asset_headers(response.headers_mut(), &request_headers, &etag);
    Ok(response)
}

#[derive(Deserialize)]
pub(super) struct RpcRequest {
    session: String,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaRequest {
    path: Option<String>,
    id: Option<i64>,
    content_version: Option<String>,
    #[serde(default = "preview_kind")]
    kind: String,
}
fn preview_kind() -> String {
    "preview".into()
}

pub(super) async fn rpc(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<RpcRequest>,
) -> AppResult<Json<Value>> {
    let (claims, app) = verify(&state, &request.session).await?;
    if claims.purpose != "session" || claims.app_id != id || claims.node_id.is_some() {
        return Err(AppError::Forbidden("应用凭据用途不匹配".into()));
    }
    if !matches!(
        request.method.as_str(),
        "app.ping"
            | "settings.get"
            | "settings.patch"
            | "files.search"
            | "favorites.set"
            | "media.url"
    ) && app.manifest.api_version < 2
    {
        return Err(AppError::BadRequest("此能力需要应用 API v2".into()));
    }
    let result = match request.method.as_str() {
        "files.list" | "files.stat" | "files.searchPage" => {
            app.manifest.require("files.read")?;
            files::invoke(&state, &app, &request.method, request.params).await?
        }
        "storage.get" | "storage.set" | "storage.list" | "storage.delete" => {
            storage::invoke(&state, &app, &request.method, request.params).await?
        }
        "app.ping" => json!({"ok": true, "revision": app.revision}),
        "settings.get" => read_settings(&state, &app).await?,
        "settings.patch" => save_settings(&state, &app, request.params).await?,
        "files.search" => {
            app.manifest.require("files.read")?;
            let query: crate::api::SearchQuery = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("搜索参数无效".into()))?;
            if !app.scope.is_empty() {
                let base = query.under.clone().unwrap_or_else(|| "/".into());
                let normalized = format!("/{}", crate::vfs::normalize(&base)?.join("/"));
                scope::check(&state, &app, &normalized).await?;
            }
            crate::api::search(State(state.clone()), Query(query))
                .await?
                .0
        }
        "favorites.set" => {
            app.manifest.require("favorites.write")?;
            let body: crate::api::FavoriteBody = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("收藏参数无效".into()))?;
            let normalized = format!("/{}", crate::vfs::normalize(&body.path)?.join("/"));
            scope::check(&state, &app, &normalized).await?;
            crate::api::favorite_set(State(state.clone()), Json(body))
                .await?
                .0
        }
        "media.url" => {
            app.manifest.require("media.read")?;
            let media: MediaRequest = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("媒体参数无效".into()))?;
            if !matches!(
                media.kind.as_str(),
                "preview" | "thumbnail" | "download" | "bytes"
            ) {
                return Err(AppError::BadRequest("媒体用途无效".into()));
            }
            if media.id.is_some()
                && (app.manifest.api_version < 2 || media.content_version.is_none())
            {
                return Err(AppError::BadRequest(
                    "节点媒体引用需要 API v2 和内容版本".into(),
                ));
            }
            let (node, path) = files::resolve(
                &state,
                &files::Reference {
                    id: media.id,
                    path: media.path,
                    content_version: media.content_version,
                },
            )
            .await?;
            scope::check(&state, &app, &path).await?;
            if node.is_dir {
                return Err(AppError::IsDirectory);
            }
            let ticket = sign(
                &state,
                &Claims {
                    purpose: media.kind,
                    node_id: Some(node.id),
                    content_version: Some(files::version(&node)),
                    ..claims
                },
            )?;
            json!({"url": format!("/api/apps/media/{ticket}"), "expires_at": claims.expires_at})
        }
        _ => return Err(AppError::BadRequest("不支持的应用能力".into())),
    };
    Ok(Json(result))
}

/// CORS 预检兑底：Range 已属安全头，现代浏览器不会预检，仍应答 OPTIONS 保险。
pub(super) async fn media_preflight() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Range, If-None-Match")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("构造预检响应失败")
}

fn cors_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Range, Accept-Ranges, Content-Length, ETag"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
}

fn not_modified(etag: &str) -> Response {
    let mut out = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::empty())
        .expect("构造再验证响应失败");
    cors_headers(out.headers_mut());
    out
}

/// 小 Range 缓存命中/回填：票据已绑定内容版本，缓存键含版本天然防陈旧。
#[allow(clippy::too_many_arguments)]
async fn cached_range(
    state: &AppState,
    node: &crate::db::Node,
    version: &str,
    start: u64,
    len: u64,
    request_headers: &HeaderMap,
    inline: bool,
    etag: &str,
) -> AppResult<Response> {
    let key = RangeKey {
        node: node.id,
        version: version.to_string(),
        start,
        len,
    };
    let cache = &state.range_cache;
    if let Ok(mut guard) = cache.lock()
        && let Some(hit) = guard.get(&key)
    {
        let mut out = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, hit.mime.clone())
            .header(header::CONTENT_DISPOSITION, hit.disposition.clone())
            .header(header::CONTENT_LENGTH, len)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{}", start + len - 1, node.size),
            )
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, "private, no-cache")
            .body(Body::from(hit.bytes.clone()))
            .expect("构造范围响应失败");
        cors_headers(out.headers_mut());
        return Ok(out);
    }
    let mut response = crate::api::stream_node(state, node, request_headers, inline).await?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        // 不可满足范围等异常透传，不做缓存。
        cors_headers(response.headers_mut());
        return Ok(response);
    }
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("attachment")
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), RANGE_CACHE_ENTRY as usize + 1)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if bytes.len() != len as usize {
        return Err(AppError::Internal(anyhow::anyhow!(
            "文件范围响应长度与请求不一致，已停止读取"
        )));
    }
    if let Ok(mut guard) = cache.lock() {
        guard.put(
            key,
            CachedRange {
                mime: mime.clone(),
                disposition: disposition.clone(),
                bytes: bytes.clone(),
            },
        );
    }
    let mut out = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from(bytes))
        .expect("构造范围响应失败");
    cors_headers(out.headers_mut());
    let headers = out.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {start}-{}/{}", start + len - 1, node.size))
            .expect("Content-Range 合法"),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(etag).expect("ETag 合法"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(out)
}

pub(super) async fn media(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (claims, app) = verify(&state, &ticket).await?;
    app.manifest.require("media.read")?;
    let id = claims
        .node_id
        .ok_or_else(|| AppError::Forbidden("媒体凭据无效".into()))?;
    let path = state
        .vfs
        .db
        .full_path_if_visible(id)
        .await?
        .ok_or(AppError::NotFound)?;
    scope::check(&state, &app, &path).await?;
    let node = state.vfs.db.node(id).await?;
    let version = files::version(&node);
    if claims
        .content_version
        .as_ref()
        .is_some_and(|v| *v != version)
    {
        return Err(AppError::Conflict("文件内容已改变，请重新打开".into()));
    }
    let purpose = claims.purpose.as_str();
    // 字节与预览响应可经浏览器私有缓存复用，但每次使用都需 ETag 再验证（304）。
    if matches!(purpose, "preview" | "bytes") {
        let etag = format!("\"{version}\"");
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            == Some(etag.as_str())
        {
            return Ok(not_modified(&etag));
        }
        // 共享字节缓存只服务程序化小段读取（bytes）：预览面向 <video>/<img> 元素，
        // 必须流式渐进播放，不能为进缓存而整段缓冲到首字节可发。
        if purpose == "bytes" {
            match crate::api::parse_range(&headers, node.size as u64) {
                Ok(Some((start, end))) if end.saturating_sub(start) < RANGE_CACHE_ENTRY => {
                    return cached_range(
                        &state,
                        &node,
                        &version,
                        start,
                        end - start + 1,
                        &headers,
                        false,
                        &etag,
                    )
                    .await;
                }
                _ => {}
            }
        }
    }
    let mut response = match purpose {
        "thumbnail" => crate::api::thumbnail_for_node(&state, &node, &headers).await?,
        "preview" => crate::api::stream_node(&state, &node, &headers, true).await?,
        "bytes" => crate::api::stream_node(&state, &node, &headers, false).await?,
        "download" => crate::api::stream_node(&state, &node, &headers, false).await?,
        _ => return Err(AppError::Forbidden("媒体凭据用途不匹配".into())),
    };
    let out = response.headers_mut();
    cors_headers(out);
    match purpose {
        // 缩略图内部已带 private, no-cache + ETag/304，这里不再覆盖。
        "thumbnail" => {}
        "preview" | "bytes" => {
            out.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-cache"),
            );
            out.insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"{version}\"")).expect("ETag 合法"),
            );
        }
        _ => {
            out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
    }
    Ok(response)
}
