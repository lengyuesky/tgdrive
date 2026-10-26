//! 移动客户端兼容接口：设备认证、能力声明、稳定文件引用及受限媒体票据。
use super::*;
use crate::auth::native::{DEVICE_ACCESS_TTL, DEVICE_REFRESH_TTL, NativeClaims, refresh_hash};
use crate::db::devices::DeviceSession;

pub(super) fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/auth/device/login", post(device_login))
        .route("/auth/device/refresh", post(device_refresh))
        .route("/media/{ticket}", get(device_media))
}
pub(super) fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/client/capabilities", get(capabilities))
        .route("/auth/device/logout", post(device_logout))
        .route("/auth/devices", get(devices_index))
        .route("/auth/devices/{id}", axum::routing::delete(device_revoke))
        .route("/fs/stat", get(file_stat))
        .route("/fs/media-ticket", post(media_ticket))
}
fn private_json(value: serde_json::Value) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceLogin {
    username: String,
    password: String,
    totp: Option<String>,
    device_name: String,
}
async fn device_login(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<DeviceLogin>,
) -> AppResult<Response> {
    let name = body.device_name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "设备名称需为 1～80 个非控制字符".into(),
        ));
    }
    let epoch = state.auth.device_epoch();
    authenticate_login(
        &state,
        peer,
        &headers,
        &LoginReq {
            username: body.username,
            password: body.password,
            totp: body.totp,
        },
    )
    .await?;
    if epoch != state.auth.device_epoch() {
        return Err(AppError::Unauthorized);
    }
    let now = crate::db::now_ts();
    let session = DeviceSession {
        id: crate::auth::random_secret(),
        name: name.to_string(),
        auth_epoch: epoch,
        created_at: now,
        last_used_at: now,
        expires_at: now + DEVICE_REFRESH_TTL,
        revoked_at: None,
    };
    let refresh = crate::auth::random_secret();
    state
        .vfs
        .db
        .create_device_session(&session, &refresh_hash(&refresh))
        .await?;
    token_response(&state, &session, refresh).await
}
async fn token_response(
    state: &AppState,
    session: &DeviceSession,
    refresh: String,
) -> AppResult<Response> {
    let expires_at = (crate::db::now_ts() + DEVICE_ACCESS_TTL).min(session.expires_at);
    let access = state.auth.sign_native_claims(&NativeClaims {
        purpose: "access".into(),
        session_id: session.id.clone(),
        auth_epoch: session.auth_epoch.clone(),
        expires_at,
        node_id: None,
        content_version: None,
    })?;
    Ok(private_json(serde_json::json!({
        "token_type":"Bearer", "access_token":access, "access_expires_at":expires_at,
        "refresh_token":refresh, "refresh_expires_at":session.expires_at,
        "session_id":session.id, "username":state.auth.username(), "server_id":state.vfs.db.client_server_id().await?,
    })))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshBody {
    refresh_token: String,
}
async fn device_refresh(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RefreshBody>,
) -> AppResult<Response> {
    let key = format!("refresh:{}", peer_key(&state, peer, &headers));
    const WINDOW: i64 = 15 * 60;
    if let Some(wait) = state.attempts.retry_after(&key, 10, WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    if body.refresh_token.len() != 43
        || !body
            .refresh_token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        state.attempts.record_failure(&key, WINDOW);
        return Err(AppError::Unauthorized);
    }
    let refresh = crate::auth::random_secret();
    let session = match state
        .vfs
        .db
        .rotate_device_refresh(
            &refresh_hash(&body.refresh_token),
            &refresh_hash(&refresh),
            &state.auth.device_epoch(),
        )
        .await
    {
        Ok(session) => session,
        Err(error) => {
            if matches!(error, AppError::Unauthorized) {
                state.attempts.record_failure(&key, WINDOW);
            }
            return Err(error);
        }
    };
    token_response(&state, &session, refresh).await
}
async fn require_device(state: &AppState, headers: &HeaderMap) -> AppResult<DeviceSession> {
    state
        .auth
        .device_from_headers(headers, &state.vfs.db)
        .await?
        .ok_or(AppError::Unauthorized)
}
async fn device_logout(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = require_device(&state, &headers).await?;
    state.vfs.db.revoke_device_session(&session.id).await?;
    Ok(private_json(serde_json::json!({"ok":true})))
}
async fn require_strong(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    if !state
        .auth
        .strong_session_valid(headers, &state.vfs.db)
        .await?
    {
        return Err(AppError::Forbidden(
            "请使用主账号及两步验证登录后管理设备，不接受 Basic 或 WebDAV 应用密码".into(),
        ));
    }
    Ok(())
}
async fn devices_index(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    require_strong(&state, &headers).await?;
    let current = state
        .auth
        .device_from_headers(&headers, &state.vfs.db)
        .await?
        .map(|s| s.id);
    Ok(private_json(serde_json::json!({
        "items":state.vfs.db.device_sessions(&state.auth.device_epoch()).await?, "current_session_id":current,
    })))
}
async fn device_revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    require_strong(&state, &headers).await?;
    state.vfs.db.revoke_device_session(&id).await?;
    Ok(private_json(serde_json::json!({"ok":true})))
}
async fn capabilities(State(state): State<AppState>) -> AppResult<Response> {
    Ok(private_json(serde_json::json!({
        "contract_version":1, "host_version":env!("CARGO_PKG_VERSION"),
        "server_id":state.vfs.db.client_server_id().await?, "minimum_ios":17,
        "capabilities":["auth.device.v1","files.entries.v1","files.reference.v1","media.ticket.v1","download.etag.v1","uploads.receipts.v1","uploads.ticket.v1","uploads.conflict-policy.v1","apps.rpc.v2","events.sse.v1"],
        "limits":{"directory_page":500,"device_sessions":32,"access_ttl_seconds":DEVICE_ACCESS_TTL,"refresh_ttl_seconds":DEVICE_REFRESH_TTL},
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReference {
    id: Option<i64>,
    path: Option<String>,
    content_version: Option<String>,
}
async fn resolve_reference(
    state: &AppState,
    reference: &FileReference,
) -> AppResult<(Node, String)> {
    let (node, path) = match (reference.id, reference.path.as_deref()) {
        (Some(id), None) if id > 0 => {
            let path = state
                .vfs
                .db
                .full_path_if_visible(id)
                .await?
                .ok_or(AppError::NotFound)?;
            (state.vfs.db.node(id).await?, path)
        }
        (None, Some(path)) if path.len() <= 4096 => {
            let path = crate::vfs::canonical(path)?;
            (state.vfs.resolve(&path).await?, path)
        }
        _ => {
            return Err(AppError::BadRequest(
                "需提供唯一的正整数节点 ID 或路径".into(),
            ));
        }
    };
    if reference
        .content_version
        .as_ref()
        .is_some_and(|v| *v != node.content_version())
    {
        return Err(AppError::Conflict("文件内容已改变，请重新打开".into()));
    }
    Ok((node, path))
}
async fn file_stat(
    State(state): State<AppState>,
    Query(reference): Query<FileReference>,
) -> AppResult<Response> {
    let (node, path) = resolve_reference(&state, &reference).await?;
    let has_thumbnail = if node.is_dir {
        false
    } else {
        state
            .vfs
            .db
            .first_chunk(node.id)
            .await?
            .is_some_and(|c| c.tg_thumbnail_file_id.is_some())
    };
    Ok(private_json(serde_json::json!({
        "id":node.id,"path":path,"name":node.name,"is_dir":node.is_dir,"size":node.size,
        "created_at":node.created_at,"modified_at":node.modified_at,"sha256":node.sha256,
        "favorite":node.favorite,"content_version":node.content_version(),"has_thumbnail":has_thumbnail,
    })))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaRequest {
    id: i64,
    content_version: String,
    kind: String,
}
async fn media_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MediaRequest>,
) -> AppResult<Response> {
    let session = require_device(&state, &headers).await?;
    let ttl = match body.kind.as_str() {
        "preview" => 2 * 3600,
        "download" => 6 * 3600,
        "bytes" | "thumbnail" => 10 * 60,
        _ => return Err(AppError::BadRequest("媒体用途无效".into())),
    };
    let (node, _) = resolve_reference(
        &state,
        &FileReference {
            id: Some(body.id),
            path: None,
            content_version: Some(body.content_version),
        },
    )
    .await?;
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let expires_at = (crate::db::now_ts() + ttl).min(session.expires_at);
    let ticket = state.auth.sign_native_claims(&NativeClaims {
        purpose: body.kind,
        session_id: session.id,
        auth_epoch: session.auth_epoch,
        expires_at,
        node_id: Some(node.id),
        content_version: Some(node.content_version()),
    })?;
    Ok(private_json(
        serde_json::json!({"url":format!("/api/media/{ticket}"),"expires_at":expires_at}),
    ))
}
async fn device_media(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let claims = state
        .auth
        .verify_native_claims(&ticket)
        .ok_or(AppError::Unauthorized)?;
    if !["preview", "download", "bytes", "thumbnail"].contains(&claims.purpose.as_str())
        || state
            .vfs
            .db
            .active_device_session(&claims.session_id, &claims.auth_epoch)
            .await?
            .is_none()
    {
        return Err(AppError::Unauthorized);
    }
    let version = claims.content_version.ok_or(AppError::Unauthorized)?;
    let (node, _) = resolve_reference(
        &state,
        &FileReference {
            id: claims.node_id,
            path: None,
            content_version: Some(version),
        },
    )
    .await?;
    match claims.purpose.as_str() {
        "thumbnail" => thumbnail_for_node(&state, &node, &headers).await,
        "preview" => stream_node(&state, &node, &headers, true).await,
        _ => stream_node(&state, &node, &headers, false).await,
    }
}

#[cfg(test)]
mod tests;
