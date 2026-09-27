//! 登录、登出与会话状态。

use super::*;

pub(super) fn peer_key(
    state: &AppState,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: &HeaderMap,
) -> String {
    state.attempts.client_ip(addr.ip(), headers).to_string()
}

pub(super) async fn login(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> AppResult<Response> {
    const LIMIT: usize = 5;
    const WINDOW: i64 = 15 * 60;
    let key = format!("login:{}", peer_key(&state, peer, &headers));
    if let Some(wait) = state.attempts.retry_after(&key, LIMIT, WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    // argon2 校验要几十毫秒 CPU，放进阻塞线程
    let auth = state.auth.clone();
    let (username, password) = (req.username.clone(), req.password.clone());
    let password_ok =
        tokio::task::spawn_blocking(move || auth.verify_login_password(&username, &password))
            .await
            .map_err(|error| AppError::Internal(anyhow::anyhow!("登录校验任务失败：{error}")))?;
    if !password_ok {
        state.attempts.record_failure(&key, WINDOW);
        return Err(AppError::Unauthorized);
    }
    if state.auth.totp_enabled() {
        let code = req.totp.as_deref().map(str::trim).unwrap_or_default();
        // 密码对了才会走到这里；没填验证码不算失败，只提示输入
        if code.is_empty() {
            return Err(AppError::TotpRequired);
        }
        if !state.auth.check_second_factor(code, &state.vfs.db).await? {
            state.attempts.record_failure(&key, WINDOW);
            return Err(AppError::TotpInvalid);
        }
    }
    state.attempts.clear(&key);
    let token = state.auth.issue_token();
    let mut resp = Json(serde_json::json!({ "username": state.auth.username() })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        state
            .auth
            .cookie_header(&token)
            .parse()
            .expect("cookie 头合法"),
    );
    Ok(resp)
}

pub(super) async fn logout(State(state): State<AppState>) -> Response {
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        state
            .auth
            .clear_cookie_header()
            .parse()
            .expect("cookie 头合法"),
    );
    resp
}

pub(super) async fn me(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "username": state.auth.username() }))
}
