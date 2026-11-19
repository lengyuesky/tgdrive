//! 契约接口注册表直接生成路由；页面数据面是 Bearer fetch，不是宿主消息转发桥。

use super::runtime::{PageSession, Sessions};
use super::store;
use super::{AppPlatform, PlatformError, PlatformResult, opaque};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, FromRequestParts, MatchedPath, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tgdrive_contract::{error_code, files::*, http::*, runtime::*, validation::parse_unique_json};
use tokio::io::AsyncWriteExt;

// 主人专用内部接口不属于应用契约，但复用契约 DTO 与同一套 Cookie/CSRF 防护。
const INTERNAL_ENDPOINTS: &[EndpointSpec] = &[
    EndpointSpec {
        name: "center.nodes.stat",
        methods: &["POST"],
        path: "/admin/api/app-center/v1/nodes/stat",
        access: EndpointAccess::OwnerOnly,
        capability: None,
    },
    EndpointSpec {
        name: "center.app.usage",
        methods: &["GET"],
        path: "/admin/api/app-center/v1/apps/{app_id}/data/usage",
        access: EndpointAccess::OwnerOnly,
        capability: None,
    },
];

pub(crate) fn router(host: crate::api::AppState) -> Router {
    let platform = Arc::new(AppPlatform::new(host));
    router_platform(platform)
}
pub(super) fn router_platform(platform: Arc<AppPlatform>) -> Router {
    super::sse::start(&platform);
    let mut routes = Router::new();
    let mut paths = BTreeSet::new();
    for endpoint in ENDPOINTS.iter().chain(INTERNAL_ENDPOINTS) {
        if matches!(endpoint.access, EndpointAccess::DeviceKv) || endpoint.name == "local.ticket" {
            continue;
        }
        let path = endpoint.path.replace("{path}", "{*path}");
        if paths.insert(path.clone()) {
            routes = routes.route(&path, axum::routing::any(dispatch));
        }
    }
    routes
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(platform.clone(), envelope))
        .with_state(platform)
}

pub(crate) async fn boundary_guard(request: Request, next: Next) -> Response {
    if request.uri().path() == LOCAL_MEDIA_BASE
        || request
            .uri()
            .path()
            .starts_with(&format!("{LOCAL_MEDIA_BASE}/"))
    {
        let mut response =
            PlatformError::new(error_code::NOT_FOUND, "公网不存在本机读取入口").into_response();
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(EXTERNAL_CSP),
        );
        response.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        return response;
    }
    next.run(request).await
}
pub(crate) fn maintenance_response(path: &str, headers: &HeaderMap) -> Response {
    let mut response =
        PlatformError::new(error_code::HOST_MAINTENANCE, "宿主处于恢复或维护状态").into_response();
    let public = path.starts_with(ASSET_BASE)
        || path.starts_with(&format!("{APP_API_PREFIX}/media/"))
        || path.starts_with(&format!("{APP_API_PREFIX}/cache-media/"));
    if public {
        response
            .headers_mut()
            .insert("access-control-allow-origin", HeaderValue::from_static("*"));
    } else if !path.starts_with(APP_CENTER_BASE)
        && headers
            .get(header::ORIGIN)
            .is_some_and(|origin| origin == "null")
    {
        response.headers_mut().insert(
            "access-control-allow-origin",
            HeaderValue::from_static("null"),
        );
    }
    response.headers_mut().insert(
        "access-control-expose-headers",
        HeaderValue::from_static(CORS_EXPOSED_HEADERS),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if path.starts_with(ASSET_BASE) {
            PAGE_CSP
        } else {
            EXTERNAL_CSP
        }),
    );
    response
}

fn specifications(path: &str) -> Vec<&'static EndpointSpec> {
    ENDPOINTS
        .iter()
        .chain(INTERNAL_ENDPOINTS)
        .filter(|endpoint| endpoint.path.replace("{path}", "{*path}") == path)
        .collect()
}
fn methods(specs: &[&EndpointSpec]) -> String {
    let mut methods = BTreeSet::new();
    for spec in specs {
        for method in spec.methods {
            if *method == "*" {
                methods.extend(["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]);
            } else {
                methods.insert(*method);
            }
        }
    }
    methods.into_iter().collect::<Vec<_>>().join(", ")
}

async fn envelope(
    State(platform): State<Arc<AppPlatform>>,
    request: Request,
    next: Next,
) -> Response {
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str())
        .unwrap_or("");
    let specs = specifications(matched);
    let access = specs.first().map(|s| s.access);
    let allowed = methods(&specs);
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let response = if request.uri().to_string().len() > MAX_URI_BYTES as usize
        || request
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
            .sum::<usize>()
            > MAX_HEADER_BYTES as usize
    {
        PlatformError::new(error_code::PAYLOAD_TOO_LARGE, "请求地址或请求头超过上限")
            .into_response()
    } else if request.method() == Method::OPTIONS {
        if access == Some(EndpointAccess::OwnerOnly)
            && (origin.as_deref() == Some("null")
                || !platform.host.auth.session_cookie_valid(request.headers()))
        {
            PlatformError::new(error_code::UNAUTHORIZED, "应用中心需要主人网页登录").into_response()
        } else {
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                "access-control-allow-methods",
                HeaderValue::from_str(&allowed).expect("方法为 ASCII"),
            );
            response.headers_mut().insert(
                "access-control-allow-headers",
                HeaderValue::from_static(CORS_ALLOWED_HEADERS),
            );
            response
        }
    } else {
        next.run(request).await
    };
    let mut response = response;
    let headers = response.headers_mut();
    if !headers.contains_key("x-tgdrive-request-id") {
        headers.insert(
            "x-tgdrive-request-id",
            HeaderValue::from_str(&opaque()).expect("随机标识为 ASCII"),
        );
    }
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if access != Some(EndpointAccess::OwnerOnly) {
        headers.insert(
            "access-control-expose-headers",
            HeaderValue::from_static(CORS_EXPOSED_HEADERS),
        );
        if matches!(
            access,
            Some(EndpointAccess::Public | EndpointAccess::Ticket)
        ) {
            headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        } else if origin.as_deref() == Some("null") {
            headers.insert(
                "access-control-allow-origin",
                HeaderValue::from_static("null"),
            );
            headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
    if access == Some(EndpointAccess::Public) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_CSP),
        );
        headers.insert(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        );
        headers.insert(
            "cross-origin-resource-policy",
            HeaderValue::from_static("cross-origin"),
        );
    } else if access != Some(EndpointAccess::OwnerOnly) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(EXTERNAL_CSP),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            "cross-origin-resource-policy",
            HeaderValue::from_static("cross-origin"),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.remove("access-control-allow-credentials");
    if platform.host.restore.maintenance() {
        platform.sessions.revoke_all();
    }
    response
}

async fn dispatch(State(platform): State<Arc<AppPlatform>>, request: Request) -> Response {
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str())
        .unwrap_or("");
    let names = specifications(matched);
    let seconds = if names.iter().any(|spec| {
        matches!(
            spec.name,
            "center.package.upload" | "center.install.preview" | "center.install"
        )
    }) {
        PACKAGE_DOWNLOAD_SECONDS
    } else if names
        .iter()
        .any(|spec| matches!(spec.name, "files.bytes" | "media.ticket" | "cache.ticket"))
    {
        STREAM_IDLE_TIMEOUT_SECONDS
    } else {
        CONTROL_TIMEOUT_SECONDS
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(seconds),
        dispatch_inner(platform, request),
    )
    .await
    .unwrap_or_else(|_| {
        Err(PlatformError::new(
            error_code::REQUEST_TIMEOUT,
            "应用请求超过时限",
        ))
    });
    match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}
async fn dispatch_inner(platform: Arc<AppPlatform>, request: Request) -> PlatformResult<Response> {
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str())
        .unwrap_or("");
    let specs = specifications(matched);
    let allowed = methods(&specs);
    let Some(spec) = specs.into_iter().find(|spec| {
        spec.methods.contains(&request.method().as_str())
            || (spec.methods.contains(&"*")
                && ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
                    .contains(&request.method().as_str()))
    }) else {
        let mut response =
            PlatformError::new(error_code::METHOD_NOT_ALLOWED, "目标不接受此方法").into_response();
        response.headers_mut().insert(
            header::ALLOW,
            HeaderValue::from_str(&allowed).expect("方法为 ASCII"),
        );
        return Ok(response);
    };
    let headers = request.headers().clone();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let origin = public_origin(&platform, &request)?;
    let session = if spec.access == EndpointAccess::OwnerOnly {
        if headers.get(header::ORIGIN).is_some_and(|v| v == "null")
            || !platform.host.auth.session_cookie_valid(&headers)
        {
            return Err(PlatformError::new(
                error_code::UNAUTHORIZED,
                "应用中心需要主人网页登录",
            ));
        }
        if !method.is_safe()
            && ((headers
                .get("sec-fetch-site")
                .is_some_and(|site| site != "same-origin" && site != "none"))
                || headers
                    .get(header::ORIGIN)
                    .is_some_and(|value| value.to_str().map_or(true, |value| value != origin)))
        {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "拒绝跨源应用中心操作",
            ));
        }
        None
    } else if matches!(
        spec.access,
        EndpointAccess::Public | EndpointAccess::Ticket | EndpointAccess::External
    ) {
        None
    } else {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|v| !v.is_empty() && !v.bytes().any(|b| b.is_ascii_whitespace()))
            .ok_or_else(|| PlatformError::new(error_code::UNAUTHORIZED, "缺少应用页面会话"))?;
        let session = platform.sessions.authenticate(
            &platform.cursors,
            bearer,
            &platform.host.auth.device_epoch(),
        )?;
        let mut connection = platform.host.vfs.db.pool.acquire().await?;
        let app = store::installation(&mut connection, &session.app_id).await?;
        app.check(&session.generation)?;
        session.rate_limit()?;
        Some(session)
    };
    if platform.host.restore.maintenance() || platform.host.vfs.db.background_blocked() {
        return Err(PlatformError::new(
            error_code::HOST_MAINTENANCE,
            "宿主处于恢复或维护状态",
        ));
    }
    let _slot = if let Some(session) = &session {
        if matches!(spec.name, "events" | "files.bytes") {
            None
        } else {
            Some(session.control_slot()?)
        }
    } else {
        None
    };
    let (mut parts, body) = request.into_parts();
    let Path(parameters) = Path::<BTreeMap<String, String>>::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| PlatformError::new(error_code::INVALID_PATH, "路径参数无效"))?;
    let id = parameters.get("app_id").map(String::as_str).unwrap_or("");
    let key = parameters.get("key").map(String::as_str).unwrap_or("");
    let body = if matches!(
        spec.name,
        "session.refresh"
            | "session.close"
            | "center.app.restart"
            | "center.app.cache.clear"
            | "read_leases.revoke"
    ) {
        let bytes = to_bytes(body, MAX_JSON_REQUEST_BYTES as usize)
            .await
            .map_err(|_| {
                PlatformError::new(error_code::PAYLOAD_TOO_LARGE, "空正文接口收到过长正文")
            })?;
        if !bytes.is_empty() {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "此接口不接受正文",
            ));
        }
        Body::empty()
    } else {
        body
    };
    let mut revocations = platform.host.auth.revocation_signal();
    if session
        .as_ref()
        .is_some_and(|session| session.owner_epoch != platform.host.auth.device_epoch())
    {
        return Err(PlatformError::new(
            error_code::SESSION_EXPIRED,
            "主人登录已撤销",
        ));
    }
    let operation = async {
        let response = match spec.name {
            "center.apps" => owner_json(platform.installed_apps().await?),
            "center.app" => owner_json(platform.installed(id).await?),
            "center.nodes.stat" => {
                let reference: NodeReference = strict_json(body).await?;
                let mut tx = platform.host.vfs.db.pool.begin().await?;
                owner_json(store::owner_reference(&mut tx, &reference).await?)
            }
            "center.app.usage" => owner_json(platform.owner_or_page_usage(id, None).await?),
            "center.catalog" => owner_json(platform.catalog().await?.current().await),
            "center.catalog.refresh" => {
                let request: CatalogRefreshRequest = strict_json(body).await?;
                owner_json(platform.catalog().await?.refresh(request.force).await?)
            }
            "center.package.upload" => {
                if headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    != Some("application/zip")
                {
                    return Err(PlatformError::new(
                        error_code::INVALID_REQUEST,
                        "包上传需要 application/zip",
                    ));
                }
                let staging = upload_body(&platform, body, &headers).await?;
                let receipt = platform
                    .upload_package(&staging.path().join("package.tgapp"))
                    .await?;
                (StatusCode::CREATED, axum::Json(receipt)).into_response()
            }
            "center.install.preview" => {
                owner_json(platform.preview(strict_json(body).await?).await?)
            }
            "center.install" => owner_json(platform.install(strict_json(body).await?).await?),
            "center.app.state" => {
                owner_json(platform.state_patch(id, strict_json(body).await?).await?)
            }
            "center.app.scope" => {
                let result =
                    super::store::update_scope(&platform.host.vfs.db, id, strict_json(body).await?)
                        .await?;
                platform.events.emit(
                    id,
                    tgdrive_contract::events::event_name::SCOPE_CHANGED,
                    &tgdrive_contract::events::ScopeChangedEvent {
                        revision: result.revision.clone(),
                    },
                )?;
                owner_json(result)
            }
            "center.app.uninstall" => {
                let state_revision = headers
                    .get(header::IF_MATCH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix('"'))
                    .and_then(|v| v.strip_suffix('"'))
                    .filter(|v| tgdrive_contract::validation::valid_opaque_id(v))
                    .ok_or_else(|| {
                        PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "卸载需要加双引号的 If-Match 安装修订",
                        )
                    })?;
                let query = queries(&uri)?;
                let purge = match query.get("purge_data").map(String::as_str) {
                    None | Some("false") => false,
                    Some("true") => true,
                    _ => {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "purge_data 无效",
                        ));
                    }
                };
                platform.uninstall(id, state_revision, purge).await?;
                StatusCode::NO_CONTENT.into_response()
            }
            "center.app.cache.clear" => {
                platform.installed(id).await?;
                platform.clear_app_cache(id).await?;
                StatusCode::NO_CONTENT.into_response()
            }
            "center.app.launch" => owner_json(
                platform
                    .launch(id, strict_json(body).await?, origin)
                    .await?,
            ),
            "center.app.restart" | "center.root.read" | "center.root.update" => {
                return Err(super::runtime::backend_unavailable());
            }
            "assets" => {
                return assets(&platform, &parameters, &headers, method == Method::HEAD).await;
            }
            "events" => {
                let session = caller(&session)?;
                super::sse::response(platform.clone(), session.clone(), &headers).await?
            }
            "runtime.info" => {
                let session = caller(&session)?;
                let mut tx = platform.host.vfs.db.pool.begin().await?;
                let app = store::installation(&mut tx, &session.app_id).await?;
                let scope = store::scope_info(&mut tx, &app).await?;
                json(super::runtime::info(&app, scope)?)
            }
            "session.refresh" => {
                let session = caller(&session)?;
                let expiry = super::now().0 + PAGE_SESSION_SECONDS as i64 * 1000;
                session.expires_at.store(expiry, Ordering::Release);
                json(SessionRefreshResponse {
                    session_token: Sessions::token(&platform.cursors, session)?,
                    expires_at: tgdrive_contract::Timestamp(expiry),
                })
            }
            "session.close" => {
                let session = caller(&session)?;
                platform.sessions.revoke(&session.id);
                StatusCode::NO_CONTENT.into_response()
            }
            "scope.read" => {
                let session = caller(&session)?;
                json(platform.app_scope(&session.app_id).await?)
            }
            "files.stat" => {
                let session = caller(&session)?;
                let reference: NodeReference = strict_json(body).await?;
                let mut tx = platform.host.vfs.db.pool.begin().await?;
                let app = store::installation(&mut tx, &session.app_id).await?;
                app.check(&session.generation)?;
                json(store::reference(&mut tx, &app, &reference).await?)
            }
            "files.list" => {
                let session = caller(&session)?;
                json(
                    platform
                        .list_directory(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "files.query" => {
                let session = caller(&session)?;
                json(
                    platform
                        .file_query(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "files.directories_with_files" => {
                let session = caller(&session)?;
                json(
                    platform
                        .directories_with_files(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "files.random" => {
                let session = caller(&session)?;
                json(
                    platform
                        .random_files(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "files.changes" => {
                let session = caller(&session)?;
                json(
                    platform
                        .file_changes(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "kv.get" => {
                let session = caller(&session)?;
                json(
                    platform
                        .kv_get(&session.app_id, &session.generation, key)
                        .await?,
                )
            }
            "kv.put" => {
                let session = caller(&session)?;
                json(
                    platform
                        .kv_put(
                            &session.app_id,
                            &session.generation,
                            key,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "kv.delete" => {
                let session = caller(&session)?;
                platform
                    .kv_delete(
                        &session.app_id,
                        &session.generation,
                        key,
                        strict_json(body).await?,
                    )
                    .await?;
                StatusCode::NO_CONTENT.into_response()
            }
            "kv.list" => {
                let session = caller(&session)?;
                json(
                    platform
                        .kv_list(
                            &session.app_id,
                            &session.generation,
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "files.bytes" => {
                let session = caller(&session)?;
                let query = queries(&uri)?;
                let version = query
                    .get("content_version")
                    .filter(|version| tgdrive_contract::validation::valid_opaque_id(version))
                    .cloned()
                    .ok_or_else(|| {
                        PlatformError::new(error_code::INVALID_REQUEST, "读取必须提供文件内容版本")
                    })?;
                let file = ContentRef {
                    id: parameters.get("id").cloned().unwrap_or_default(),
                    content_version: version,
                };
                platform
                    .direct_bytes(session, file, &headers, method == Method::HEAD)
                    .await?
            }
            "files.media_url" => {
                let session = caller(&session)?;
                json(
                    platform
                        .media_url(session, strict_json(body).await?, &origin)
                        .await?,
                )
            }
            "media.ticket" => {
                platform
                    .ticket_bytes(
                        parameters.get("ticket").map(String::as_str).unwrap_or(""),
                        false,
                        &headers,
                        method == Method::HEAD,
                    )
                    .await?
            }
            "cache.ticket" => {
                platform
                    .ticket_bytes(
                        parameters.get("ticket").map(String::as_str).unwrap_or(""),
                        true,
                        &headers,
                        method == Method::HEAD,
                    )
                    .await?
            }
            "read_leases.create" => {
                let session = caller(&session)?;
                (
                    StatusCode::CREATED,
                    axum::Json(
                        platform
                            .create_lease(session, strict_json(body).await?)
                            .await?,
                    ),
                )
                    .into_response()
            }
            "read_leases.renew" => {
                let session = caller(&session)?;
                json(
                    platform
                        .renew_lease(
                            session,
                            parameters.get("id").map(String::as_str).unwrap_or(""),
                            strict_json(body).await?,
                        )
                        .await?,
                )
            }
            "read_leases.revoke" => {
                let session = caller(&session)?;
                platform.revoke_lease(
                    session,
                    parameters.get("id").map(String::as_str).unwrap_or(""),
                )?;
                StatusCode::NO_CONTENT.into_response()
            }
            "cache.put" => {
                let session = caller(&session)?;
                let content_type = headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let metadata = if let Some(value) = headers.get(CACHE_METADATA_HEADER) {
                    let encoded = value.to_str().map_err(|_| {
                        PlatformError::new(error_code::INVALID_REQUEST, "缓存元数据头不是 ASCII")
                    })?;
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(encoded)
                        .map_err(|_| {
                            PlatformError::new(
                                error_code::INVALID_REQUEST,
                                "缓存元数据必须是无 padding 的 base64url",
                            )
                        })?;
                    let value = parse_unique_json(&bytes)?;
                    if !value.is_object() {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "缓存元数据必须是对象",
                        ));
                    }
                    check_objects(&value)?;
                    serde_json::from_value(value)?
                } else {
                    tgdrive_contract::data::CachePutMetadata::default()
                };
                let bytes = to_bytes(body, MAX_CACHE_OBJECT_BYTES as usize)
                    .await
                    .map_err(|_| {
                        PlatformError::new(error_code::PAYLOAD_TOO_LARGE, "缓存对象超过上限")
                    })?;
                json(
                    platform
                        .cache_put(session, key, content_type, metadata, bytes)
                        .await?,
                )
            }
            "cache.get" => {
                let session = caller(&session)?;
                platform
                    .cache_response(
                        session.clone(),
                        key,
                        None,
                        None,
                        &headers,
                        method == Method::HEAD,
                    )
                    .await?
            }
            "cache.delete" => {
                let session = caller(&session)?;
                platform.cache_delete(session, key).await?;
                StatusCode::NO_CONTENT.into_response()
            }
            "cache.lookup" => {
                let session = caller(&session)?;
                json(
                    platform
                        .cache_lookup(session, strict_json(body).await?, &origin)
                        .await?,
                )
            }
            "data.usage" => {
                let session = caller(&session)?;
                json(
                    platform
                        .data_usage(&session.app_id, &session.generation)
                        .await?,
                )
            }
            _ => return Err(super::runtime::backend_unavailable()),
        };
        Ok(response)
    };
    if let Some(session) = &session
        && spec.name != "session.close"
    {
        return tokio::select! { _=revocations.changed()=>Err(PlatformError::new(error_code::SESSION_EXPIRED,"主人登录已撤销")), _=session.cancelled.cancelled()=>Err(PlatformError::new(error_code::SESSION_EXPIRED,"页面会话已撤销")), result=operation=>result };
    }
    operation.await
}
fn caller(session: &Option<Arc<PageSession>>) -> PlatformResult<&Arc<PageSession>> {
    session
        .as_ref()
        .ok_or_else(|| PlatformError::new(error_code::UNAUTHORIZED, "缺少页面身份"))
}
fn owner_json<T: serde::Serialize>(value: T) -> Response {
    axum::Json(value).into_response()
}
fn json<T: serde::Serialize>(value: T) -> Response {
    match tgdrive_contract::validation::validate_json_response(&value) {
        Ok(()) => axum::Json(value).into_response(),
        Err(error) => PlatformError::from(error).into_response(),
    }
}

pub(crate) async fn strict_json<T: DeserializeOwned>(body: Body) -> PlatformResult<T> {
    let bytes = to_bytes(body, MAX_JSON_REQUEST_BYTES as usize)
        .await
        .map_err(|_| PlatformError::new(error_code::PAYLOAD_TOO_LARGE, "JSON 正文超过上限"))?;
    let value = parse_unique_json(&bytes).map_err(|_| {
        PlatformError::new(
            error_code::INVALID_REQUEST,
            "JSON 含重复键、非法数值或语法错误",
        )
    })?;
    if !value.is_object() {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "请求必须是 JSON 对象",
        ));
    }
    check_objects(&value)?;
    Ok(serde_json::from_value(value)?)
}
fn check_objects(value: &serde_json::Value) -> PlatformResult<()> {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if matches!(key.as_str(), "value" | "metadata") {
                continue;
            }
            if matches!(
                key.as_str(),
                "condition"
                    | "scope"
                    | "file"
                    | "directory"
                    | "source"
                    | "filter"
                    | "file_filter"
                    | "size"
                    | "created_at"
                    | "modified_at"
            ) && !value.is_null()
                && !value.is_object()
            {
                return Err(PlatformError::new(
                    error_code::INVALID_REQUEST,
                    "结构字段必须使用 JSON 对象",
                )
                .field(key));
            }
            if let Some(array) = value.as_array() {
                if matches!(key.as_str(), "directories" | "sources")
                    && array.iter().any(|value| !value.is_object())
                {
                    return Err(PlatformError::new(
                        error_code::INVALID_REQUEST,
                        "结构数组的每项必须是 JSON 对象",
                    )
                    .field(key));
                }
                for value in array {
                    check_objects(value)?;
                }
            } else {
                check_objects(value)?;
            }
        }
    }
    Ok(())
}
async fn upload_body(
    platform: &AppPlatform,
    body: Body,
    headers: &HeaderMap,
) -> PlatformResult<tempfile::TempDir> {
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_PACKAGE_BYTES)
    {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "压缩包长度超过上限",
        ));
    }
    let root = platform.root.join("staging");
    tokio::fs::create_dir_all(&root).await?;
    let staging = tempfile::Builder::new()
        .prefix("upload-")
        .tempdir_in(root)?;
    let mut file = tokio::fs::File::create(staging.path().join("package.tgapp")).await?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(bytes) = stream.next().await {
        let bytes =
            bytes.map_err(|_| PlatformError::new(error_code::INVALID_PACKAGE, "包上传中断"))?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_PACKAGE_BYTES {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "包上传实际长度超过上限",
            ));
        }
        file.write_all(&bytes).await?;
    }
    file.sync_all().await?;
    Ok(staging)
}

pub(crate) fn public_origin(platform: &AppPlatform, request: &Request) -> PlatformResult<String> {
    let trusted = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| {
            platform
                .host
                .rt
                .env
                .trusted_proxies
                .iter()
                .any(|net| net.contains(&peer.0.ip()))
        });
    let forwarded = |name: &str| {
        trusted
            .then(|| {
                request
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.split(',').next())
                    .map(str::trim)
            })
            .flatten()
    };
    let authority = forwarded("x-forwarded-host")
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
        })
        .or_else(|| request.uri().authority().map(|a| a.as_str()))
        .unwrap_or("localhost");
    let scheme = forwarded("x-forwarded-proto").unwrap_or(if platform.host.rt.env.cookie_secure {
        "https"
    } else {
        "http"
    });
    if !matches!(scheme, "http" | "https") {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "公开地址协议无效",
        ));
    }
    let url = reqwest::Url::parse(&format!("{scheme}://{authority}"))
        .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "公开地址格式无效"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "公开地址格式无效",
        ));
    }
    Ok(url.origin().ascii_serialization())
}
fn queries(uri: &axum::http::Uri) -> PlatformResult<BTreeMap<String, String>> {
    let url = reqwest::Url::parse(&format!("http://localhost{uri}"))
        .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "查询格式无效"))?;
    let mut output = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if output
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "查询参数不能重复",
            ));
        }
    }
    Ok(output)
}
pub(crate) fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag
            })
        })
}

async fn assets(
    platform: &AppPlatform,
    parameters: &BTreeMap<String, String>,
    headers: &HeaderMap,
    head: bool,
) -> PlatformResult<Response> {
    use sha2::{Digest, Sha256};
    let id = parameters.get("app_id").map(String::as_str).unwrap_or("");
    let sha = parameters.get("sha256").map(String::as_str).unwrap_or("");
    let path = parameters.get("path").map(String::as_str).unwrap_or("");
    if !tgdrive_contract::validation::valid_app_id(id)
        || !tgdrive_contract::validation::valid_sha256(sha)
        || !tgdrive_contract::validation::valid_resource_path(path)
        || !(path.starts_with("page/") || path.starts_with("assets/"))
    {
        return Err(PlatformError::new(error_code::NOT_FOUND, "包资源不存在"));
    }
    let app = platform.installed(id).await?;
    if !app.enabled || app.package.sha256 != sha {
        return Err(PlatformError::new(
            error_code::NOT_FOUND,
            "包资源不属于当前已启用版本",
        ));
    }
    let mut directory =
        super::safe_fs::Directory::open(&platform.package_root, false)?.child(sha, false)?;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_some() {
            directory = directory.child(segment, false)?;
        } else {
            let file = directory.read(segment)?;
            let size = file.metadata()?.len();
            let etag = format!(
                "\"p{sha}:{}:identity\"",
                crate::engine::upload::to_hex(&Sha256::digest(path.as_bytes()))
            );
            let not_modified = etag_matches(headers, &etag);
            let body = if head || not_modified {
                Body::empty()
            } else {
                Body::from_stream(tokio_util::io::ReaderStream::new(
                    tokio::fs::File::from_std(file),
                ))
            };
            let mut response = Response::builder()
                .status(if not_modified {
                    StatusCode::NOT_MODIFIED
                } else {
                    StatusCode::OK
                })
                .header(header::ETAG, etag)
                .header(
                    header::CONTENT_TYPE,
                    mime_guess::from_path(path).first_or_octet_stream().as_ref(),
                )
                .header(
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                )
                .body(body)
                .expect("资源头已验证");
            if !not_modified {
                response.headers_mut().insert(
                    header::CONTENT_LENGTH,
                    HeaderValue::from_str(&size.to_string()).expect("字节数为 ASCII"),
                );
            }
            return Ok(response);
        }
    }
    Err(PlatformError::new(error_code::NOT_FOUND, "包资源不存在"))
}
