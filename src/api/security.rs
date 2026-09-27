//! 后台「账号安全」：两步验证与 WebDAV 应用密码。

use super::*;

/// 验证码输错的额度：会话被盗时也不能靠这里无限次猜验证码。
const CODE_LIMIT: usize = 5;
const CODE_WINDOW: i64 = 15 * 60;
const CODE_KEY: &str = "security:totp";

/// 安全设置只接受网页登录会话，WebDAV 应用密码不能反过来关闭两步验证。
fn require_web_session(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    if state.auth.session_cookie_valid(headers) {
        Ok(())
    } else {
        Err(AppError::Forbidden("请在网页登录后修改安全设置".into()))
    }
}

/// 校验当前验证码或恢复码；错误计入限流。用 400 而不是 401，免得前端当成登录失效。
async fn require_second_factor(state: &AppState, code: &str) -> AppResult<()> {
    if let Some(wait) = state
        .attempts
        .retry_after(CODE_KEY, CODE_LIMIT, CODE_WINDOW)
    {
        return Err(AppError::RateLimited(wait));
    }
    if state
        .auth
        .check_second_factor(code.trim(), &state.vfs.db)
        .await?
    {
        state.attempts.clear(CODE_KEY);
        Ok(())
    } else {
        state.attempts.record_failure(CODE_KEY, CODE_WINDOW);
        Err(AppError::BadRequest("验证码错误或已使用".into()))
    }
}

#[derive(Deserialize)]
pub(super) struct CodeBody {
    code: String,
}

pub(super) async fn security_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let status = state.auth.security_status();
    Json(serde_json::json!({
        "totp_enabled": status.totp_enabled,
        "recovery_remaining": status.recovery_remaining,
        "webdav_password_set": status.webdav_password_set,
        // 来自 .env 的明文密码无法在这里改写，提示用户换成哈希
        "password_hashed": crate::password::is_hash(&state.rt.current().password),
    }))
}

pub(super) async fn totp_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<crate::auth::TotpSetup>> {
    require_web_session(&state, &headers)?;
    if state.auth.totp_enabled() {
        return Err(AppError::BadRequest(
            "两步验证已启用；如需更换验证器，请先关闭再重新绑定".into(),
        ));
    }
    Ok(Json(state.auth.begin_totp_setup()))
}

/// 绑定成功后轮换会话密钥：其他设备需要重新登录（并输入验证码），当前页面换发新 Cookie。
pub(super) async fn totp_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> AppResult<Response> {
    require_web_session(&state, &headers)?;
    let codes = state
        .auth
        .enable_totp(body.code.trim(), &state.vfs.db)
        .await?;
    state.auth.rotate_secret(&state.vfs.db).await?;
    let token = state.auth.issue_token();
    let mut resp = Json(serde_json::json!({ "recovery_codes": codes })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        state
            .auth
            .cookie_header(&token)
            .parse()
            .expect("cookie 头合法"),
    );
    record_security_log(&state, "已启用两步验证，其他设备的登录已失效").await;
    Ok(resp)
}

pub(super) async fn totp_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> AppResult<Json<serde_json::Value>> {
    require_web_session(&state, &headers)?;
    require_second_factor(&state, &body.code).await?;
    state.auth.disable_totp(&state.vfs.db).await?;
    record_security_log(&state, "已关闭两步验证").await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(super) async fn totp_recovery_regenerate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> AppResult<Json<serde_json::Value>> {
    require_web_session(&state, &headers)?;
    require_second_factor(&state, &body.code).await?;
    let codes = state.auth.regenerate_recovery_codes(&state.vfs.db).await?;
    record_security_log(&state, "已重新生成两步验证恢复码，旧恢复码作废").await;
    Ok(Json(serde_json::json!({ "recovery_codes": codes })))
}

pub(super) async fn webdav_password_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    require_web_session(&state, &headers)?;
    let password = state.auth.generate_webdav_password(&state.vfs.db).await?;
    record_security_log(&state, "已生成新的 WebDAV 应用密码，旧应用密码作废").await;
    Ok(Json(serde_json::json!({ "password": password })))
}

pub(super) async fn webdav_password_revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    require_web_session(&state, &headers)?;
    state.auth.revoke_webdav_password(&state.vfs.db).await?;
    record_security_log(&state, "已撤销 WebDAV 应用密码").await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn record_security_log(state: &AppState, message: &str) {
    if let Err(error) = state
        .vfs
        .db
        .append_backend_log("info", "账号安全", message)
        .await
    {
        tracing::warn!("记录安全日志失败: {error}");
    }
}
