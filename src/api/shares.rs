//! 分享：管理与公开访问。

use super::*;

// ---------- 分享:管理(需登录) ----------

#[derive(Deserialize)]
pub(super) struct ShareCreateBody {
    pub(super) path: String,
    pub(super) password: Option<String>,
    /// 有效小时数;不传/0 = 永久
    pub(super) expires_hours: Option<i64>,
    /// 最多下载次数;不传/0 = 不限
    pub(super) max_downloads: Option<i64>,
}

pub(super) async fn shares_create(
    State(state): State<AppState>,
    Json(b): Json<ShareCreateBody>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&b.path).await?;
    let pwd = b
        .password
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let exp = b
        .expires_hours
        .filter(|h| *h > 0)
        .map(|h| crate::db::now_ts() + h * 3600);
    let maxd = b.max_downloads.filter(|n| *n > 0);
    let share = state.vfs.db.share_create(node.id, pwd, exp, maxd).await?;
    Ok(Json(
        serde_json::json!({ "share": share, "name": node.name, "is_dir": node.is_dir }),
    ))
}

pub(super) async fn shares_index(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let mut items = Vec::new();
    for s in state.vfs.db.shares_list().await? {
        let node = state.vfs.db.node(s.node_id).await?;
        let path = state
            .vfs
            .db
            .full_path_if_visible(s.node_id)
            .await?
            .unwrap_or_else(|| "(不可见)".into());
        items.push(serde_json::json!({
            "id": s.id,
            "token": s.token,
            "path": path,
            "name": node.name,
            "is_dir": node.is_dir,
            "password": s.password,
            "expires_at": s.expires_at,
            "max_downloads": s.max_downloads,
            "downloads": s.downloads,
            "created_at": s.created_at,
        }));
    }
    Ok(Json(serde_json::json!({ "items": items })))
}

pub(super) async fn shares_remove(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.db.share_delete(id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------- 分享:公开访问(免登录) ----------

#[derive(Deserialize)]
pub(super) struct ShareQuery {
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) inline: Option<u8>,
}

#[derive(Deserialize, Default)]
pub(super) struct ShareAuthorizeBody {
    #[serde(default)]
    pub(super) password: Option<String>,
}

/// token → (share, 分享根节点);过期/已删除在此统一拦下
pub(super) async fn share_access(
    state: &AppState,
    token: &str,
) -> AppResult<(crate::db::Share, Node)> {
    let s = state
        .vfs
        .db
        .share_by_token(token)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(exp) = s.expires_at
        && exp < crate::db::now_ts()
    {
        return Err(AppError::BadRequest("分享已过期".into()));
    }
    let node = state.vfs.db.node(s.node_id).await?;
    if node.status != "ready" {
        return Err(AppError::NotFound);
    }
    Ok((s, node))
}

pub(super) fn share_check_pwd(s: &crate::db::Share, pwd: Option<&str>) -> AppResult<()> {
    if let Some(want) = &s.password
        && !pwd.is_some_and(|given| Auth::secrets_equal(given, want))
    {
        return Err(AppError::Forbidden("提取码错误".into()));
    }
    Ok(())
}

/// 分享内相对路径 → 节点;normalize 拒绝 `..`,不会越出分享根
pub(super) async fn share_resolve(state: &AppState, root: &Node, rel: &str) -> AppResult<Node> {
    let segs = crate::vfs::normalize(rel)?;
    let mut cur = root.clone();
    for seg in &segs {
        cur = state
            .vfs
            .db
            .child(cur.id, seg)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    Ok(cur)
}

pub(super) async fn share_meta(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let (s, node) = share_access(&state, &token).await?;
    Ok(Json(serde_json::json!({
        "name": node.name,
        "is_dir": node.is_dir,
        "size": node.size,
        "need_password": s.password.is_some(),
        "expires_at": s.expires_at,
        "downloads": s.downloads,
        "max_downloads": s.max_downloads,
    })))
}

/// 提取码只通过 POST 请求体提交；成功后签发短期、分享范围内的 HttpOnly 票据。
pub(super) async fn share_authorize(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ShareAuthorizeBody>,
) -> AppResult<Response> {
    const LIMIT: usize = 10;
    const WINDOW: i64 = 10 * 60;

    let (share, _) = share_access(&state, &token).await?;
    let key = format!("share:{token}:{}", peer_key(&state, peer, &headers));
    if let Some(wait) = state.attempts.retry_after(&key, LIMIT, WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    if let Err(error) = share_check_pwd(&share, body.password.as_deref()) {
        state.attempts.record_failure(&key, WINDOW);
        return Err(error);
    }
    state.attempts.clear(&key);

    let (raw, ticket) = state.auth.issue_share_ticket(share.id);
    let mut response = Json(serde_json::json!({
        "expires_at": ticket.expires_at,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        state
            .auth
            .share_cookie_header(&raw, &token)
            .parse()
            .expect("分享 cookie 头合法"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

pub(super) fn require_share_ticket(
    state: &AppState,
    headers: &HeaderMap,
    share_id: i64,
) -> AppResult<crate::auth::ShareTicket> {
    let ticket = state
        .auth
        .share_ticket_from_headers(headers)
        .ok_or_else(|| AppError::Forbidden("分享票据无效或已过期，请重新输入提取码".into()))?;
    if ticket.share_id != share_id {
        return Err(AppError::Forbidden("分享票据与当前链接不匹配".into()));
    }
    Ok(ticket)
}

pub(super) async fn share_list_dir(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    Query(q): Query<ShareQuery>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let (s, root) = share_access(&state, &token).await?;
    require_share_ticket(&state, &headers, s.id)?;
    let target = share_resolve(&state, &root, q.path.as_deref().unwrap_or("")).await?;
    if !target.is_dir {
        return Err(AppError::NotDirectory);
    }
    let entries: Vec<Entry> = state
        .vfs
        .db
        .list_children(target.id)
        .await?
        .iter()
        .map(Entry::from)
        .collect();
    Ok(Json(serde_json::json!({ "entries": entries })))
}

pub(super) async fn share_download(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    Query(q): Query<ShareQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (s, root) = share_access(&state, &token).await?;
    let ticket = require_share_ticket(&state, &headers, s.id)?;
    let target = if root.is_dir {
        share_resolve(&state, &root, q.path.as_deref().unwrap_or("")).await?
    } else {
        root
    };
    if target.is_dir {
        return Err(AppError::IsDirectory);
    }
    // 无效 Range 不应消耗一次下载额度。
    if parse_range(&headers, target.size as u64).is_err() {
        return Ok(Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", target.size))
            .body(axum::body::Body::empty())
            .expect("响应构造失败"));
    }
    if !state
        .vfs
        .db
        .share_claim_download(s.id, &ticket.ticket_id, target.id, ticket.expires_at)
        .await?
    {
        return Err(AppError::BadRequest("下载次数已用完".into()));
    }
    stream_node(&state, &target, &headers, q.inline == Some(1)).await
}
