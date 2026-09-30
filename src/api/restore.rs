//! 恢复 API 仅接受网页 Cookie；Basic/WebDAV 应用密码不能授权。
use super::*;
use crate::restore::{PrepareRequest, RestoreStatus};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/restore/history", get(history))
        .route("/admin/restore/discover", post(discover))
        .route("/admin/restore/prepare", post(prepare))
        .route("/admin/restore/status", get(status))
        .route("/admin/restore/cancel", post(cancel))
        .route("/admin/restore/confirm", post(confirm))
        .route("/admin/restore/resume", post(resume))
}
fn web_session(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    if !state.auth.session_cookie_valid(headers) {
        return Err(AppError::Forbidden(
            "恢复操作必须先网页登录，不接受 Basic 或应用密码".into(),
        ));
    }
    Ok(())
}
async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    web_session(&state, &headers)?;
    Ok(Json(
        serde_json::json!({ "backups": crate::restore::history(&state).await? }),
    ))
}
async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<RestoreStatus>> {
    web_session(&state, &headers)?;
    Ok(Json(state.restore.status().await))
}
#[derive(Deserialize)]
struct DiscoverBody {
    #[serde(default)]
    reset: bool,
}
async fn discover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DiscoverBody>,
) -> AppResult<Json<crate::restore::DiscoveryResult>> {
    web_session(&state, &headers)?;
    Ok(Json(state.restore.discover(&state, body.reset).await?))
}
async fn prepare(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PrepareRequest>,
) -> AppResult<Json<RestoreStatus>> {
    web_session(&state, &headers)?;
    let manager = state.restore.clone();
    Ok(Json(manager.prepare(state, body).await?))
}
#[derive(Deserialize)]
struct IdBody {
    id: String,
}
async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IdBody>,
) -> AppResult<Json<RestoreStatus>> {
    web_session(&state, &headers)?;
    Ok(Json(state.restore.cancel(&body.id).await?))
}
#[derive(Deserialize)]
struct ConfirmBody {
    id: String,
    password: String,
    totp: Option<String>,
    confirmation: String,
}
#[derive(Deserialize)]
struct ResumeBody {
    password: String,
    totp: Option<String>,
}
async fn reauthenticate(state: &AppState, password: &str, totp: Option<&str>) -> AppResult<()> {
    const KEY: &str = "restore:confirm";
    const WINDOW: i64 = 15 * 60;
    if let Some(wait) = state.attempts.retry_after(KEY, 5, WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    if !state
        .auth
        .verify_login_password(&state.auth.username(), password)
    {
        state.attempts.record_failure(KEY, WINDOW);
        return Err(AppError::BadRequest("当前登录密码不正确".into()));
    }
    if state.auth.totp_enabled()
        && !state
            .auth
            .check_second_factor(totp.unwrap_or("").trim(), &state.vfs.db)
            .await?
    {
        state.attempts.record_failure(KEY, WINDOW);
        return Err(AppError::BadRequest("两步验证码错误、缺失或已使用".into()));
    }
    // 成功不清零，避免其他有效请求为猜密码者重置额度。
    Ok(())
}
async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> AppResult<(StatusCode, Json<RestoreStatus>)> {
    web_session(&state, &headers)?;
    if body.confirmation != "恢复" {
        return Err(AppError::BadRequest("请准确输入“恢复”".into()));
    }
    let _exclusive = state.restore.write_gate.write().await;
    web_session(&state, &headers)?;
    reauthenticate(&state, &body.password, body.totp.as_deref()).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(state.restore.confirm(&state, &body.id).await?),
    ))
}
async fn resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResumeBody>,
) -> AppResult<(StatusCode, Json<RestoreStatus>)> {
    web_session(&state, &headers)?;
    let _exclusive = state.restore.write_gate.write().await;
    web_session(&state, &headers)?;
    reauthenticate(&state, &body.password, body.totp.as_deref()).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(state.restore.resume(&state).await?),
    ))
}

/// 外层覆盖 API、WebDAV 及应用网关，安全模式不能绕过管理员端点触发历史任务。
pub(crate) async fn write_guard(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let restore = path.starts_with("/api/admin/restore/");
    let auth = path.starts_with("/api/auth/");
    let read = matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) || req.method().as_str() == "PROPFIND";
    let _guard = if !read && !restore {
        Some(state.restore.write_gate.read().await)
    } else {
        None
    };
    if state.restore.maintenance() && !matches!(path, "/healthz" | "/api/admin/restore/status") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"恢复维护中，应用正在退出重启"})),
        )
            .into_response();
    }
    // 安全模式允许修复网络/账号配置，但所有数据写入与后台执行入口保持关闭。
    let config = matches!(
        path,
        "/api/admin/config" | "/api/admin/config/reset" | "/api/admin/test"
    ) || path.starts_with("/api/admin/security/");
    if state.vfs.db.background_blocked() && !read && !restore && !auth && !config {
        return AppError::Forbidden(
            "恢复安全模式：数据写入和后台任务已暂停，请先核对历史任务，再到备份页确认恢复后台任务"
                .into(),
        )
        .into_response();
    }
    next.run(req).await
}
