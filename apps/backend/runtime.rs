//! 运行页面不获得登录凭据；能力调用绑定应用版本，媒体凭据只授权单个文件。

use super::{
    InstalledApp, cache_dir, files, installed, package, read_settings, save_settings, storage,
};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SESSION_TTL: i64 = 6 * 60 * 60;
const SANDBOX_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self'; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

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
        return Err(AppError::Forbidden("应用会话已过期，请重新打开".into()));
    }
    let app = installed(state, &claims.app_id).await?;
    if !app.enabled || app.revision != claims.revision {
        return Err(AppError::Conflict(
            "应用已停用、卸载或更新，请重新打开".into(),
        ));
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

pub(super) async fn asset(
    State(state): State<AppState>,
    Path((id, revision, resource)): Path<(String, String, String)>,
) -> AppResult<Response> {
    if !package::valid_resource(&resource) {
        return Err(AppError::NotFound);
    }
    let app = installed(&state, &id).await?;
    if !app.enabled || app.revision != revision {
        return Err(AppError::NotFound);
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
    let headers = response.headers_mut();
    // 即使用户直接打开资源地址，响应级 sandbox 也不会授予主站同源权限。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX_CSP),
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
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
            files::invoke(&state, &request.method, request.params).await?
        }
        "storage.get" | "storage.set" | "storage.list" | "storage.delete" => {
            storage::invoke(&state, &app, &request.method, request.params).await?
        }
        "app.ping" => json!({"ok": true, "revision": app.revision}),
        "settings.get" => read_settings(&state, &app).await?,
        "settings.patch" => save_settings(&state, &app, request.params).await?,
        "files.search" => {
            app.manifest.require("files.read")?;
            let query = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("搜索参数无效".into()))?;
            crate::api::search(State(state.clone()), Query(query))
                .await?
                .0
        }
        "favorites.set" => {
            app.manifest.require("favorites.write")?;
            let body = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("收藏参数无效".into()))?;
            crate::api::favorite_set(State(state.clone()), Json(body))
                .await?
                .0
        }
        "media.url" => {
            app.manifest.require("media.read")?;
            let media: MediaRequest = serde_json::from_value(request.params)
                .map_err(|_| AppError::BadRequest("媒体参数无效".into()))?;
            if !matches!(media.kind.as_str(), "preview" | "thumbnail" | "download") {
                return Err(AppError::BadRequest("媒体用途无效".into()));
            }
            if media.id.is_some()
                && (app.manifest.api_version < 2 || media.content_version.is_none())
            {
                return Err(AppError::BadRequest(
                    "节点媒体引用需要 API v2 和内容版本".into(),
                ));
            }
            let (node, _) = files::resolve(
                &state,
                &files::Reference {
                    id: media.id,
                    path: media.path,
                    content_version: media.content_version,
                },
            )
            .await?;
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
            json!({"url": format!("/api/apps/media/{ticket}")})
        }
        _ => return Err(AppError::BadRequest("不支持的应用能力".into())),
    };
    Ok(Json(result))
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
    if state.vfs.db.full_path_if_visible(id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    let node = state.vfs.db.node(id).await?;
    if claims
        .content_version
        .as_ref()
        .is_some_and(|v| *v != files::version(&node))
    {
        return Err(AppError::Conflict("文件内容已改变，请重新打开".into()));
    }
    let mut response = match claims.purpose.as_str() {
        "thumbnail" => crate::api::thumbnail_for_node(&state, &node, &headers).await?,
        "preview" => crate::api::stream_node(&state, &node, &headers, true).await?,
        "download" => crate::api::stream_node(&state, &node, &headers, false).await?,
        _ => return Err(AppError::Forbidden("媒体凭据用途不匹配".into())),
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Range, Accept-Ranges, Content-Length"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
