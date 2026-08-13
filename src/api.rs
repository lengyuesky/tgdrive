//! REST API:认证 + 文件操作端点,供 Vue 前端调用

use crate::auth::{Auth, LoginReq};
use crate::db::Node;
use crate::error::{AppError, AppResult};
use crate::settings::{self, ApplyOutcome, ConfigPatch, Runtime};
use crate::vfs::Vfs;
use axum::extract::{Multipart, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct AppState {
    pub vfs: Vfs,
    pub auth: Arc<Auth>,
    /// 运行时配置与热更句柄
    pub rt: Arc<Runtime>,
    pub dav: dav_server::DavHandler,
}

pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .route("/auth/login", post(login))
        // 分享链接:免登录访问
        .route("/share/{token}", get(share_meta))
        .route("/share/{token}/list", get(share_list_dir))
        .route("/share/{token}/download", get(share_download));
    let protected = Router::new()
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/fs/list", get(list))
        .route("/fs/mkdir", post(mkdir))
        .route("/fs/upload", post(upload))
        .route("/fs/download", get(download))
        .route("/fs/rename", post(rename))
        .route("/fs/copy", post(copy_entry))
        .route("/fs/delete", post(delete))
        .route("/fs/search", get(search))
        .route("/fs/instant", post(instant))
        .route("/trash", get(trash_index))
        .route("/trash/restore", post(trash_restore))
        .route("/trash/purge", post(trash_purge))
        .route("/shares", get(shares_index).post(shares_create))
        .route("/shares/{id}", axum::routing::delete(shares_remove))
        .route("/info", get(info))
        .route("/admin/config", get(admin_config).post(admin_save))
        .route("/admin/config/reset", post(admin_reset))
        .route("/admin/test", post(admin_test))
        .route("/admin/status", get(admin_status))
        .route("/admin/cleanup", post(admin_cleanup))
        .route("/admin/logout-all", post(admin_logout_all))
        .route("/admin/env-export", get(admin_env_export))
        .route("/admin/backup", get(admin_backup))
        .route("/admin/backup-to-channel", post(admin_backup_to_channel))
        .route("/admin/extras", get(extras_get).post(extras_set))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware));
    Router::new().merge(public).merge(protected).with_state(state)
}

/// cookie 或 Basic 任一通过即放行
pub async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.auth.check_headers(req.headers()) {
        return next.run(req).await;
    }
    AppError::Unauthorized.into_response()
}

// ---------- 认证 ----------

async fn login(State(state): State<AppState>, Json(req): Json<LoginReq>) -> AppResult<Response> {
    req.verify(&state.auth)?;
    let token = state.auth.issue_token();
    let mut resp = Json(serde_json::json!({ "username": state.auth.username() })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        state.auth.cookie_header(&token).parse().expect("cookie 头合法"),
    );
    Ok(resp)
}

async fn logout() -> Response {
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, Auth::clear_cookie_header().parse().expect("cookie 头合法"));
    resp
}

async fn me(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "username": state.auth.username() }))
}

// ---------- 文件操作 ----------

#[derive(Serialize)]
struct Entry {
    name: String,
    is_dir: bool,
    size: i64,
    created_at: i64,
    modified_at: i64,
}

impl From<&Node> for Entry {
    fn from(n: &Node) -> Self {
        Entry {
            name: n.name.clone(),
            is_dir: n.is_dir,
            size: n.size,
            created_at: n.created_at,
            modified_at: n.modified_at,
        }
    }
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
    #[serde(default)]
    inline: Option<u8>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let nodes = state.vfs.list(&q.path).await?;
    let entries: Vec<Entry> = nodes.iter().map(Entry::from).collect();
    Ok(Json(serde_json::json!({ "path": q.path, "entries": entries })))
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

async fn mkdir(
    State(state): State<AppState>,
    Json(b): Json<PathBody>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.mkdir_p(&b.path).await?;
    Ok(Json(serde_json::json!({ "ok": true, "name": node.name })))
}

/// 单段文件名校验(multipart 里带路径分隔符一律拒绝)
fn clean_filename(raw: &str) -> AppResult<String> {
    let segs = crate::vfs::normalize(raw)?;
    match segs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(AppError::InvalidPath(format!("非法文件名: {raw}"))),
    }
}

async fn upload(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
    mut mp: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    let dir = state.vfs.resolve(&q.path).await?;
    let mut saved = Vec::new();
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart 解析失败: {e}")))?
    {
        let Some(fname) = field.file_name().map(str::to_string) else { continue };
        let name = clean_filename(&fname)?;
        let handle = state.vfs.start_upload(&dir, &name).await?;
        loop {
            match field.chunk().await {
                Ok(Some(bytes)) => handle.feed(bytes).await?,
                Ok(None) => break,
                Err(e) => {
                    // handle 随作用域丢弃 → 管线自动回滚
                    return Err(AppError::BadRequest(format!("读取上传流失败: {e}")));
                }
            }
        }
        let node = handle.finish().await?;
        saved.push(Entry::from(&node));
    }
    if saved.is_empty() {
        return Err(AppError::BadRequest("请求中没有文件".into()));
    }
    Ok(Json(serde_json::json!({ "saved": saved })))
}

/// 解析单个 Range;None=无 Range,Err=不可满足
fn parse_range(headers: &HeaderMap, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let Some(spec) = h.strip_prefix("bytes=") else { return Ok(None) };
    let first = spec.split(',').next().unwrap_or("").trim();
    let Some((a, b)) = first.split_once('-') else { return Err(()) };
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            // 后缀范围:最后 n 字节
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 || total == 0 {
                return Err(());
            }
            (total.saturating_sub(n), total - 1)
        }
        (s, "") => (s.parse().map_err(|_| ())?, total.saturating_sub(1)),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse().map_err(|_| ())?),
    };
    if total == 0 || start >= total || start > end {
        return Err(());
    }
    Ok(Some((start, end.min(total - 1))))
}

async fn download(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let node = state.vfs.resolve(&q.path).await?;
    stream_node(&state, &node, &headers, q.inline == Some(1)).await
}

/// 流式返回一个文件节点(带 Range 支持);REST 下载与分享下载共用
async fn stream_node(
    state: &AppState,
    node: &Node,
    headers: &HeaderMap,
    inline: bool,
) -> AppResult<Response> {
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let mime = mime_guess::from_path(&node.name).first_or_octet_stream();
    let disposition = {
        let kind = if inline { "inline" } else { "attachment" };
        let enc = utf8_percent_encode(&node.name, NON_ALPHANUMERIC);
        format!("{kind}; filename*=UTF-8''{enc}")
    };

    let range = parse_range(headers, node.size as u64);
    match range {
        Err(()) => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{}", node.size))],
        )
            .into_response()),
        Ok(None) => {
            let reader = state.vfs.open_reader(node, 0).await?;
            let total = reader.total_size();
            let body = axum::body::Body::from_stream(ReaderStream::new(reader));
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CONTENT_LENGTH, total)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
        Ok(Some((start, end))) => {
            let len = end - start + 1;
            let reader = state.vfs.open_reader(node, start).await?;
            let body = axum::body::Body::from_stream(ReaderStream::new(reader.take(len)));
            Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CONTENT_LENGTH, len)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", node.size))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
    }
}

#[derive(Deserialize)]
struct FromTo {
    from: String,
    to: String,
}

async fn rename(
    State(state): State<AppState>,
    Json(b): Json<FromTo>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.rename(&b.from, &b.to).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn copy_entry(
    State(state): State<AppState>,
    Json(b): Json<FromTo>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.copy(&b.from, &b.to).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeleteBody {
    paths: Vec<String>,
}

async fn delete(
    State(state): State<AppState>,
    Json(b): Json<DeleteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut failed = Vec::new();
    let mut deleted = 0;
    for p in &b.paths {
        match state.vfs.delete(p).await {
            Ok(()) => deleted += 1,
            Err(e) => failed.push(serde_json::json!({ "path": p, "error": e.to_string() })),
        }
    }
    Ok(Json(serde_json::json!({ "deleted": deleted, "failed": failed })))
}

async fn info(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let (files, dirs, total) = state.vfs.db.stats().await?;
    let c = state.rt.current();
    Ok(Json(serde_json::json!({
        "username": state.auth.username(),
        "api_base": c.api_base,
        "official_api": c.is_official_api(),
        "chunk_size": c.chunk_size,
        "configured": c.is_configured(),
        "files": files,
        "dirs": dirs,
        "total_size": total,
    })))
}

// ---------- 后台管理 ----------

/// 密码一律以固定长度占位返回,连长度都不泄露
const PASSWORD_PLACEHOLDER: &str = "••••••••";

async fn admin_config(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let c = state.rt.current();
    let sources = state.rt.sources().await?;
    let readonly: HashMap<&str, String> = state.rt.readonly_items().into_iter().collect();
    Ok(Json(serde_json::json!({
        "values": {
            "api_base": c.api_base,
            "bot_token": settings::mask(&c.bot_token),
            "chat_id": c.chat_id,
            "chunk_size": c.chunk_size,
            "upload_concurrency": c.upload_concurrency,
            "tmp_dir": c.tmp_dir.display().to_string(),
            "username": c.username,
            "password": PASSWORD_PLACEHOLDER,
            "telegram_api_id": c.telegram_api_id,
            "telegram_api_hash": settings::mask(&c.telegram_api_hash),
        },
        "sources": sources,
        "configured": c.is_configured(),
        "official_api": c.is_official_api(),
        "official_api_base": crate::config::OFFICIAL_API_BASE,
        "official_chunk_max": crate::config::OFFICIAL_CHUNK_MAX,
        "official_chunk_default": crate::config::OFFICIAL_CHUNK_DEFAULT,
        "local_chunk_default": crate::config::LOCAL_CHUNK_DEFAULT,
        "readonly": readonly,
    })))
}

async fn admin_save(
    State(state): State<AppState>,
    Json(mut patch): Json<ConfigPatch>,
) -> AppResult<Json<ApplyOutcome>> {
    // 前端把脱敏占位原样回传时视为「未修改」,避免把 *** 存成真值
    let c = state.rt.current();
    if patch.bot_token.as_deref() == Some(settings::mask(&c.bot_token).as_str()) {
        patch.bot_token = None;
    }
    if patch.telegram_api_hash.as_deref() == Some(settings::mask(&c.telegram_api_hash).as_str()) {
        patch.telegram_api_hash = None;
    }
    if patch.password.as_deref() == Some(PASSWORD_PLACEHOLDER) {
        patch.password = None;
    }
    let out = state.rt.apply(patch).await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ResetBody {
    keys: Vec<String>,
}

async fn admin_reset(
    State(state): State<AppState>,
    Json(b): Json<ResetBody>,
) -> AppResult<Json<ApplyOutcome>> {
    Ok(Json(state.rt.reset(&b.keys).await?))
}

#[derive(Deserialize, Default)]
struct TestBody {
    api_base: Option<String>,
    bot_token: Option<String>,
    chat_id: Option<String>,
}

/// 试连当前或页面上正在编辑的配置,不保存
async fn admin_test(
    State(state): State<AppState>,
    body: Option<Json<TestBody>>,
) -> AppResult<Json<serde_json::Value>> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let c = state.rt.current();
    let pick = |given: Option<String>, cur: &str| -> String {
        match given {
            Some(v) if !v.trim().is_empty() && v != settings::mask(cur) => v.trim().to_string(),
            _ => cur.to_string(),
        }
    };
    let api_base = pick(b.api_base, &c.api_base).trim_end_matches('/').to_string();
    let token = pick(b.bot_token, &c.bot_token);
    let chat_id = pick(b.chat_id, &c.chat_id);
    if token.trim().is_empty() || chat_id.trim().is_empty() {
        return Err(AppError::BadRequest("请先填写 Bot Token 与存储频道".into()));
    }
    let bot = Runtime::probe(&api_base, &token, &chat_id).await?;
    Ok(Json(serde_json::json!({ "ok": true, "bot": bot, "api_base": api_base })))
}

/// 数据库占用(含 WAL/SHM 边车文件)
fn db_file_size(path: &str) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|sfx| std::fs::metadata(format!("{path}{sfx}")).ok())
        .map(|m| m.len())
        .sum()
}

async fn admin_status(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let (files, dirs, total) = state.vfs.db.stats().await?;
    let c = state.rt.current();
    let db_path = state.rt.env.db_path.clone();
    Ok(Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "bot": state.rt.bot_name().await,
        "configured": c.is_configured(),
        "api_base": c.api_base,
        "official_api": c.is_official_api(),
        "chunk_size": c.chunk_size,
        "upload_concurrency": c.upload_concurrency,
        "files": files,
        "dirs": dirs,
        "total_size": total,
        "db_size": db_file_size(&db_path),
        "db_path": db_path,
        "listen": state.rt.env.listen.to_string(),
        "started_at": state.rt.started_at,
        "now": crate::db::now_ts(),
    })))
}

async fn admin_cleanup(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let freed = state.vfs.db.cleanup_stale_uploads().await?;
    let n = freed.len();
    state.vfs.uploader.delete_messages_best_effort(freed);
    Ok(Json(serde_json::json!({ "cleaned": n })))
}

/// 轮换签名密钥:所有设备上的登录态(含本会话)立即失效
async fn admin_logout_all(State(state): State<AppState>) -> AppResult<Response> {
    state.auth.rotate_secret(&state.vfs.db).await?;
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, Auth::clear_cookie_header().parse().expect("cookie 头合法"));
    Ok(resp)
}

async fn admin_env_export(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.rt.env_export(),
    )
        .into_response()
}

// ---------- 搜索 / 秒传 ----------

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let needle = q.q.trim();
    if needle.is_empty() {
        return Err(AppError::BadRequest("搜索词不能为空".into()));
    }
    let hits = state.vfs.db.search(needle, 200).await?;
    let results: Vec<serde_json::Value> = hits
        .iter()
        .map(|(n, path)| {
            serde_json::json!({
                "path": path,
                "name": n.name,
                "is_dir": n.is_dir,
                "size": n.size,
                "modified_at": n.modified_at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": results })))
}

#[derive(Deserialize)]
struct InstantBody {
    dir: String,
    name: String,
    sha256: String,
    size: i64,
}

/// 秒传探测:库里已有同内容文件则零流量落位,否则让前端走真实上传
async fn instant(
    State(state): State<AppState>,
    Json(b): Json<InstantBody>,
) -> AppResult<Json<serde_json::Value>> {
    let name = clean_filename(&b.name)?;
    if b.sha256.len() != 64 || !b.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest("sha256 格式不对".into()));
    }
    if b.size < 0 {
        return Err(AppError::BadRequest("大小无效".into()));
    }
    let dir = state.vfs.resolve(&b.dir).await?;
    if !dir.is_dir {
        return Err(AppError::NotDirectory);
    }
    match state.vfs.db.instant_link(&b.sha256.to_lowercase(), b.size, dir.id, &name).await? {
        Some((id, freed)) => {
            state.vfs.uploader.delete_messages_best_effort(freed);
            let node = state.vfs.db.node(id).await?;
            Ok(Json(serde_json::json!({ "linked": true, "entry": Entry::from(&node) })))
        }
        None => Ok(Json(serde_json::json!({ "linked": false }))),
    }
}

// ---------- 回收站 ----------

async fn trash_index(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let list = state.vfs.db.trash_list().await?;
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|(n, orig, at)| {
            serde_json::json!({
                "id": n.id,
                "name": n.name,
                "is_dir": n.is_dir,
                "size": n.size,
                "orig_path": orig,
                "trashed_at": at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct TrashBody {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    all: bool,
}

/// 只接受确实在回收站里的 id,防止借这个口子删任意节点
async fn trash_targets(state: &AppState, b: &TrashBody) -> AppResult<Vec<i64>> {
    let in_trash: Vec<i64> = state.vfs.db.trash_list().await?.iter().map(|(n, _, _)| n.id).collect();
    if b.all {
        return Ok(in_trash);
    }
    Ok(b.ids.iter().copied().filter(|id| in_trash.contains(id)).collect())
}

async fn trash_restore(
    State(state): State<AppState>,
    Json(b): Json<TrashBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for id in trash_targets(&state, &b).await? {
        match state.vfs.db.trash_restore(id).await {
            Ok(path) => restored.push(serde_json::json!({ "id": id, "path": path })),
            Err(e) => failed.push(serde_json::json!({ "id": id, "error": e.to_string() })),
        }
    }
    Ok(Json(serde_json::json!({ "restored": restored, "failed": failed })))
}

async fn trash_purge(
    State(state): State<AppState>,
    Json(b): Json<TrashBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut purged = 0;
    for id in trash_targets(&state, &b).await? {
        let freed = state.vfs.db.delete_node(id).await?;
        state.vfs.uploader.delete_messages_best_effort(freed);
        purged += 1;
    }
    Ok(Json(serde_json::json!({ "purged": purged })))
}

// ---------- 分享:管理(需登录) ----------

#[derive(Deserialize)]
struct ShareCreateBody {
    path: String,
    password: Option<String>,
    /// 有效小时数;不传/0 = 永久
    expires_hours: Option<i64>,
    /// 最多下载次数;不传/0 = 不限
    max_downloads: Option<i64>,
}

async fn shares_create(
    State(state): State<AppState>,
    Json(b): Json<ShareCreateBody>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&b.path).await?;
    let pwd = b.password.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let exp = b.expires_hours.filter(|h| *h > 0).map(|h| crate::db::now_ts() + h * 3600);
    let maxd = b.max_downloads.filter(|n| *n > 0);
    let share = state.vfs.db.share_create(node.id, pwd, exp, maxd).await?;
    Ok(Json(serde_json::json!({ "share": share, "name": node.name, "is_dir": node.is_dir })))
}

async fn shares_index(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
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

async fn shares_remove(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.db.share_delete(id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------- 分享:公开访问(免登录) ----------

#[derive(Deserialize)]
struct ShareQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    pwd: Option<String>,
    #[serde(default)]
    inline: Option<u8>,
}

/// token → (share, 分享根节点);过期/已删除在此统一拦下
async fn share_access(state: &AppState, token: &str) -> AppResult<(crate::db::Share, Node)> {
    let s = state.vfs.db.share_by_token(token).await?.ok_or(AppError::NotFound)?;
    if let Some(exp) = s.expires_at {
        if exp < crate::db::now_ts() {
            return Err(AppError::BadRequest("分享已过期".into()));
        }
    }
    let node = state.vfs.db.node(s.node_id).await?;
    if node.status != "ready" {
        return Err(AppError::NotFound);
    }
    Ok((s, node))
}

fn share_check_pwd(s: &crate::db::Share, pwd: Option<&str>) -> AppResult<()> {
    if let Some(want) = &s.password {
        if pwd != Some(want.as_str()) {
            return Err(AppError::Forbidden("提取码错误".into()));
        }
    }
    Ok(())
}

/// 分享内相对路径 → 节点;normalize 拒绝 `..`,不会越出分享根
async fn share_resolve(state: &AppState, root: &Node, rel: &str) -> AppResult<Node> {
    let segs = crate::vfs::normalize(rel)?;
    let mut cur = root.clone();
    for seg in &segs {
        cur = state.vfs.db.child(cur.id, seg).await?.ok_or(AppError::NotFound)?;
    }
    Ok(cur)
}

async fn share_meta(
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

async fn share_list_dir(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    Query(q): Query<ShareQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let (s, root) = share_access(&state, &token).await?;
    share_check_pwd(&s, q.pwd.as_deref())?;
    let target = share_resolve(&state, &root, q.path.as_deref().unwrap_or("")).await?;
    if !target.is_dir {
        return Err(AppError::NotDirectory);
    }
    let entries: Vec<Entry> =
        state.vfs.db.list_children(target.id).await?.iter().map(Entry::from).collect();
    Ok(Json(serde_json::json!({ "entries": entries })))
}

async fn share_download(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    Query(q): Query<ShareQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (s, root) = share_access(&state, &token).await?;
    share_check_pwd(&s, q.pwd.as_deref())?;
    let target = if root.is_dir {
        share_resolve(&state, &root, q.path.as_deref().unwrap_or("")).await?
    } else {
        root
    };
    if target.is_dir {
        return Err(AppError::IsDirectory);
    }
    if let Some(max) = s.max_downloads {
        if s.downloads >= max {
            return Err(AppError::BadRequest("下载次数已用完".into()));
        }
    }
    state.vfs.db.share_bump(s.id).await?;
    stream_node(&state, &target, &headers, q.inline == Some(1)).await
}

// ---------- 备份 / 维护配置 ----------

/// 下载数据库一致性快照(VACUUM INTO 临时文件后流式返回)
async fn admin_backup(State(state): State<AppState>) -> AppResult<Response> {
    let ts = crate::db::now_ts();
    let tmp = state.rt.current().tmp_dir.join(format!("tgdrive-backup-{ts}.db"));
    let tmp_str = tmp.to_str().ok_or_else(|| AppError::BadRequest("临时目录路径无效".into()))?;
    state.vfs.db.vacuum_into(tmp_str).await?;
    let file = tokio::fs::File::open(&tmp).await?;
    let len = file.metadata().await?.len();
    // Linux 下先删路径,fd 继续有效到流结束,不留残件
    let _ = std::fs::remove_file(&tmp);
    let body = axum::body::Body::from_stream(ReaderStream::new(file));
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"tgdrive-backup-{ts}.db\""),
        )
        .body(body)
        .expect("响应构造失败"))
}

/// 立即把数据库快照发到存储频道
async fn admin_backup_to_channel(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let name = crate::housekeeping::backup_to_channel(&state.vfs, &state.rt).await?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

async fn extras_get(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let ex = crate::extras::load(&state.vfs.db).await?;
    let last_backup = state
        .vfs
        .db
        .setting_get("extra.backup_last_ts")
        .await?
        .and_then(|v| v.parse::<i64>().ok());
    let trash_count = state.vfs.db.trash_list().await?.len();
    Ok(Json(serde_json::json!({
        "extras": ex,
        "backup_last_ts": last_backup,
        "trash_count": trash_count,
    })))
}

async fn extras_set(
    State(state): State<AppState>,
    Json(p): Json<crate::extras::ExtrasPatch>,
) -> AppResult<Json<serde_json::Value>> {
    crate::extras::save(&state.vfs.db, &p).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
