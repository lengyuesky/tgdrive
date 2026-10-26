//! 后台上传票据仅可操作已绑定的一个上传会话，不接受路径、节点或其他会话变更。
use super::*;
use crate::auth::native::NativeClaims;

pub(super) fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/transfer/{ticket}/status", get(status))
        .route(
            "/transfer/{ticket}/parts/{idx}",
            put(part).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/transfer/{ticket}/complete", post(complete))
}
pub(super) fn protected_routes() -> Router<AppState> {
    Router::new().route("/uploads/{id}/ticket", post(issue))
}
async fn issue(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let device = state
        .auth
        .device_from_headers(&headers, &state.vfs.db)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let upload = state.vfs.db.upload_session_info(&id).await?;
    let expires_at = (crate::db::now_ts() + 24 * 3600).min(device.expires_at);
    let claims = NativeClaims {
        purpose: format!("upload:{id}"),
        session_id: device.id,
        auth_epoch: device.auth_epoch,
        expires_at,
        node_id: Some(upload.session.node_id),
        content_version: Some(upload.session.sha256),
    };
    let ticket = state.auth.sign_native_claims(&claims)?;
    Ok(Json(
        serde_json::json!({"url":format!("/api/transfer/{ticket}"),"expires_at":expires_at}),
    ))
}
async fn verify(state: &AppState, ticket: &str) -> AppResult<String> {
    let claims = state
        .auth
        .verify_native_claims(ticket)
        .ok_or(AppError::Unauthorized)?;
    let id = claims
        .purpose
        .strip_prefix("upload:")
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or(AppError::Unauthorized)?;
    if state
        .vfs
        .db
        .active_device_session(&claims.session_id, &claims.auth_epoch)
        .await?
        .is_none()
    {
        return Err(AppError::Unauthorized);
    }
    let matches = match state.vfs.db.upload_session_info(id).await {
        Ok(upload) => {
            claims.node_id == Some(upload.session.node_id)
                && claims.content_version.as_deref() == Some(upload.session.sha256.as_str())
        }
        Err(AppError::NotFound) => {
            let receipt = state
                .vfs
                .db
                .upload_receipt(id)
                .await?
                .ok_or(AppError::NotFound)?;
            claims.content_version.as_deref() == Some(receipt.sha256.as_str())
        }
        Err(error) => return Err(error),
    };
    if !matches {
        return Err(AppError::Forbidden("上传票据与会话内容不匹配".into()));
    }
    Ok(id.to_string())
}
async fn status(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let id = verify(&state, &ticket).await?;
    uploads::upload_result(State(state), Path(id)).await
}
async fn part(
    State(state): State<AppState>,
    Path((ticket, idx)): Path<(String, i64)>,
    request: Request,
) -> AppResult<Json<serde_json::Value>> {
    let id = verify(&state, &ticket).await?;
    uploads::upload_part(State(state), Path((id, idx)), request).await
}
async fn complete(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let id = verify(&state, &ticket).await?;
    uploads::upload_complete(State(state), Path(id)).await
}

#[cfg(test)]
mod tests;
