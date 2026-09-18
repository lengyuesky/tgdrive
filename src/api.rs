//! REST API:认证 + 文件操作端点,供 Vue 前端调用

use crate::auth::{Auth, LoginReq};
use crate::db::{BackendLogFilter, Node, SearchFilter, SearchSort, UploadSessionInfo};
use crate::engine::upload::QueueTrigger;
use crate::error::{AppError, AppResult};
use crate::settings::{self, ApplyOutcome, ConfigPatch, Runtime};
use crate::vfs::Vfs;
use axum::extract::{ConnectInfo, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt as _;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio::sync::broadcast;
use tokio_util::io::ReaderStream;

mod entries;

#[derive(Clone)]
pub struct AppState {
    pub vfs: Vfs,
    pub auth: Arc<Auth>,
    /// 运行时配置与热更句柄
    pub rt: Arc<Runtime>,
    pub dav: dav_server::DavHandler,
    /// 登录与分享提取码的失败限流。
    pub attempts: crate::security::AttemptLimiter,
    pub tasks: crate::tasks::TaskManager,
    pub migration: crate::migration::StorageMigrationManager,
    pub apps_catalog: Arc<crate::apps::CatalogManager>,
    /// 进程内事件总线；SSE 向浏览器转发应用与文件树粗粒度变更。
    pub events: crate::events::EventBus,
    /// 应用媒体端点的共享小范围字节缓存（仅内存）。
    pub range_cache: Arc<std::sync::Mutex<crate::apps::RangeCache>>,
}

/// 就绪探针：数据库可用、存储目标已装载且关键后台任务仍有心跳。
pub async fn readyz(State(state): State<AppState>) -> Response {
    let db_ok = state.vfs.db.ping().await.is_ok();
    let configured = state.rt.current().is_configured();
    let target_ok = !configured || state.vfs.tg.current().is_ok();
    let tasks_ok = state.tasks.healthy(&["housekeeping", "inbox"], 2 * 60);
    let ready = db_ok && target_ok && tasks_ok;
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(serde_json::json!({
            "ready": ready,
            "database": db_ok,
            "storage_target": target_ok,
            "background_tasks": tasks_ok,
        })),
    )
        .into_response()
}

pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .merge(crate::apps::public_router())
        .route("/auth/login", post(login))
        // 分享链接:免登录访问
        .route("/share/{token}", get(share_meta))
        .route("/share/{token}/authorize", post(share_authorize))
        .route("/share/{token}/list", get(share_list_dir))
        .route("/share/{token}/download", get(share_download));
    let protected = Router::new()
        .merge(crate::apps::protected_router())
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/events", get(events_sse))
        .route("/fs/list", get(list))
        .route("/fs/entries", get(entries::entries_get))
        .route("/fs/properties", get(properties_get))
        .route("/fs/mkdir", post(mkdir))
        .route(
            "/fs/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/fs/exists", post(exists))
        .route("/fs/download", get(download))
        .route("/fs/thumbnail", get(thumbnail_get))
        .route("/fs/rename", post(rename))
        .route("/fs/copy", post(copy_entry))
        .route("/fs/delete", post(delete))
        .route("/fs/search", get(search))
        .route("/fs/favorite", post(favorite_set))
        .route("/fs/instant", post(instant))
        .route("/uploads", get(uploads_index).post(uploads_create))
        .route("/uploads/{id}", get(upload_status).delete(upload_cancel))
        .route("/uploads/{id}/thumbnail", post(upload_thumbnail))
        .route(
            "/uploads/{id}/parts/{idx}",
            put(upload_part).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/uploads/{id}/complete", post(upload_complete))
        .route("/trash", get(trash_index))
        .route("/trash/restore", post(trash_restore))
        .route("/trash/purge", post(trash_purge))
        .route("/shares", get(shares_index).post(shares_create))
        .route("/shares/{id}", axum::routing::delete(shares_remove))
        .route("/info", get(info))
        .route("/admin/config", get(admin_config).post(admin_save))
        .route("/admin/config/reset", post(admin_reset))
        .route("/admin/test", post(admin_test))
        .route("/admin/detect-chats", post(admin_detect_chats))
        .route("/admin/status", get(admin_status))
        .route("/admin/logs", get(admin_logs))
        .route("/admin/storage-migration", get(admin_storage_migration))
        .route(
            "/admin/storage-migration/start",
            post(admin_storage_migration_start),
        )
        .route(
            "/admin/storage-migration/pause",
            post(admin_storage_migration_pause),
        )
        .route(
            "/admin/inbox-failures",
            get(admin_inbox_failures).delete(admin_inbox_failures_clear),
        )
        .route(
            "/admin/inbox-failures/{update_id}",
            axum::routing::delete(admin_inbox_failure_remove),
        )
        .route("/admin/cleanup", post(admin_cleanup))
        .route("/admin/logout-all", post(admin_logout_all))
        .route("/admin/env-export", get(admin_env_export))
        .route("/admin/backup", get(admin_backup))
        .route("/admin/backup-now", post(admin_backup_now))
        // 保留旧地址，升级前已打开的管理端仍可触发新版多渠道备份。
        .route("/admin/backup-to-channel", post(admin_backup_now))
        .route("/admin/extras", get(extras_get).post(extras_set))
        .route("/admin/queues/delete/run", post(admin_run_delete_queue))
        .route(
            "/admin/queues/delete",
            axum::routing::delete(admin_clear_delete_queue),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state)
}

/// cookie 或 Basic 任一通过即放行
pub async fn auth_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // 隔离应用只能经宿主的权限网关调用能力，不能借浏览器 Cookie 直连管理接口。
    if req
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|value| value == "null")
    {
        return AppError::Forbidden("隔离页面不能直接访问网盘接口".into()).into_response();
    }
    if state.auth.check_headers(req.headers()) {
        return next.run(req).await;
    }
    AppError::Unauthorized.into_response()
}

// ---------- 认证 ----------

// ---------- 事件流 (SSE) ----------

/// 单用户事件流：应用数据、设置、目录范围与文件树的粗粒度变更。
/// 只携带标识不携带内容；重连由 EventSource 自动完成，断档由客户端全量重拉弥补。
async fn events_sse(State(state): State<AppState>) -> Response {
    let receiver = state.events.subscribe();
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(30));
    keepalive.tick().await; // 跳过立即触发的首次 tick
    let stream = futures_util::stream::once(async {
        Ok::<_, std::io::Error>("retry: 5000\n\n".to_string())
    })
    .chain(futures_util::stream::unfold(
        (receiver, keepalive),
        |(mut receiver, mut keepalive)| async move {
            loop {
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) => {
                            let Ok(json) = serde_json::to_string(&event) else { continue };
                            return Some((
                                Ok(format!("event: {}\ndata: {json}\n\n", event.kind)),
                                (receiver, keepalive),
                            ));
                        }
                        // 落后只丢失通知，不影响正确性；继续等待下一事件。
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = keepalive.tick() => {
                        return Some((Ok(": keepalive\n\n".to_string()), (receiver, keepalive)));
                    }
                }
            }
        },
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        // 反向代理（nginx 等）默认缓冲响应体会让 SSE 卡住。
        .header("x-accel-buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .expect("构造事件流失败")
}

fn peer_key(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> String {
    addr.ip().to_string()
}

async fn login(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> AppResult<Response> {
    const LIMIT: usize = 5;
    const WINDOW: i64 = 15 * 60;
    let key = format!("login:{}", peer_key(peer));
    if let Some(wait) = state.attempts.retry_after(&key, LIMIT, WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    if let Err(error) = req.verify(&state.auth) {
        state.attempts.record_failure(&key, WINDOW);
        return Err(error);
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

async fn logout(State(state): State<AppState>) -> Response {
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
    sha256: Option<String>,
    favorite: bool,
}

impl From<&Node> for Entry {
    fn from(n: &Node) -> Self {
        Entry {
            name: n.name.clone(),
            is_dir: n.is_dir,
            size: n.size,
            created_at: n.created_at,
            modified_at: n.modified_at,
            sha256: n.sha256.clone(),
            favorite: n.favorite,
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
    Ok(Json(
        serde_json::json!({ "path": q.path, "entries": entries }),
    ))
}

/// 返回文件或目录属性。目录统计只查询 SQLite 元数据，不读取 Telegram 内容。
async fn properties_get(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&q.path).await?;
    let usage = state.vfs.db.node_usage(node.id).await?;
    let segments = crate::vfs::normalize(&q.path)?;
    let path = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };
    Ok(Json(serde_json::json!({
        "properties": {
            "name": node.name,
            "path": path,
            "is_dir": node.is_dir,
            "size": usage.total_size,
            "created_at": node.created_at,
            "modified_at": node.modified_at,
            "sha256": node.sha256,
            "favorite": node.favorite,
            "file_count": usage.file_count,
            "dir_count": usage.dir_count,
            "chunk_count": usage.chunk_count,
        }
    })))
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
    crate::events::emit(&state.events, crate::events::HostEvent::files());
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

#[derive(Deserialize)]
struct ExistsBody {
    dir: String,
    names: Vec<String>,
}

/// 上传前批量查重:返回目录下已存在的名字,前端据此询问覆盖还是跳过。
/// 跳过的文件由前端直接不发请求,大文件零流量
async fn exists(
    State(state): State<AppState>,
    Json(b): Json<ExistsBody>,
) -> AppResult<Json<serde_json::Value>> {
    // 目录还不存在(如文件夹上传将要新建的子目录)自然没有重名
    let Ok(dir) = state.vfs.resolve(&b.dir).await else {
        return Ok(Json(serde_json::json!({ "existing": [] })));
    };
    if !dir.is_dir {
        return Err(AppError::NotDirectory);
    }
    let mut existing = Vec::new();
    for raw in &b.names {
        // 名字不合法的留给上传时报错,查重阶段跳过
        let Ok(name) = clean_filename(raw) else {
            continue;
        };
        if state.vfs.db.child(dir.id, &name).await?.is_some() {
            existing.push(raw.clone());
        }
    }
    Ok(Json(serde_json::json!({ "existing": existing })))
}

async fn upload(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
    mut mp: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    let dir = state.vfs.resolve(&q.path).await?;
    let mut saved = Vec::new();
    let mut pending_thumbnail: Option<Bytes> = None;
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart 解析失败: {e}")))?
    {
        if field.name() == Some("thumbnail") {
            if pending_thumbnail.is_some() {
                return Err(AppError::BadRequest("同一文件收到多个缩略图".into()));
            }
            let declared = field.content_type().unwrap_or("").to_string();
            let mut data = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(bytes)) => {
                        if data.len() + bytes.len() >= crate::telegram::MAX_THUMBNAIL_SIZE {
                            return Err(AppError::BadRequest(format!(
                                "缩略图必须小于 {}KB",
                                crate::telegram::MAX_THUMBNAIL_SIZE / 1024
                            )));
                        }
                        data.extend_from_slice(&bytes);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(AppError::BadRequest(format!("读取缩略图失败: {e}")));
                    }
                }
            }
            validate_thumbnail_upload(&declared, &data)?;
            pending_thumbnail = Some(Bytes::from(data));
            continue;
        }
        let Some(fname) = field.file_name().map(str::to_string) else {
            continue;
        };
        let name = clean_filename(&fname)?;
        let handle = state
            .vfs
            .start_upload(&dir, &name, pending_thumbnail.take())
            .await?;
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
    if pending_thumbnail.is_some() {
        return Err(AppError::BadRequest("缩略图后缺少对应文件".into()));
    }
    Ok(Json(serde_json::json!({ "saved": saved })))
}

// ---------- 可恢复上传 ----------

#[derive(Deserialize)]
struct UploadCreateBody {
    dir: String,
    name: String,
    size: i64,
    sha256: String,
}

fn upload_session_json(info: &UploadSessionInfo) -> serde_json::Value {
    serde_json::json!({
        "id": info.session.id,
        "path": info.session.display_path,
        "name": info.node.name,
        "size": info.session.expected_size,
        "sha256": info.session.sha256,
        "chunk_size": info.session.chunk_size,
        "part_count": info.part_count(),
        "uploaded_parts": info.uploaded_parts,
        "uploaded_bytes": info.uploaded_bytes,
        "created_at": info.session.created_at,
        "updated_at": info.session.updated_at,
    })
}

/// 多分块文件固定走默认(主) Bot；单块文件才参与上传 Bot 池轮询。
fn requires_primary_upload_target(size: i64, chunk_size: u64) -> bool {
    size > 0
        && u64::try_from(size)
            .ok()
            .is_some_and(|size| size > chunk_size)
}

fn session_upload_concurrency(per_bot: usize, bot_count: usize, part_count: i64) -> usize {
    if part_count > 1 {
        1
    } else {
        per_bot.saturating_mul(bot_count.max(1)).max(1)
    }
}

fn normalized_display_path(dir: &str, name: &str) -> AppResult<String> {
    let mut segments = crate::vfs::normalize(dir)?;
    segments.push(name.to_string());
    Ok(format!("/{}", segments.join("/")))
}

async fn uploads_index(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let sessions = state.vfs.db.upload_sessions().await?;
    let values: Vec<_> = sessions.iter().map(upload_session_json).collect();
    let retry_after = state.vfs.tg.upload_retry_after_secs();
    let bot_count = state.vfs.tg.upload_bot_count();
    let concurrency = state
        .rt
        .current()
        .upload_concurrency
        .saturating_mul(bot_count.max(1));
    Ok(Json(serde_json::json!({
        "sessions": values,
        "concurrency": concurrency,
        "bot_count": bot_count,
        "retry_after": retry_after,
    })))
}

async fn upload_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let info = state.vfs.db.upload_session_info(&id).await?;
    let retry_after = state
        .vfs
        .tg
        .binding(info.session.storage_target_id)
        .map(|target| target.api.upload_retry_after_secs())
        .unwrap_or(0);
    let bot_count = state.vfs.tg.upload_bot_count();
    let concurrency = session_upload_concurrency(
        state.rt.current().upload_concurrency,
        bot_count,
        info.part_count(),
    );
    Ok(Json(serde_json::json!({
        "session": upload_session_json(&info),
        "concurrency": concurrency,
        "bot_count": bot_count,
        "retry_after": retry_after,
    })))
}

async fn uploads_create(
    State(state): State<AppState>,
    Json(body): Json<UploadCreateBody>,
) -> AppResult<Json<serde_json::Value>> {
    let name = clean_filename(&body.name)?;
    let parent = state.vfs.resolve(&body.dir).await?;
    if !parent.is_dir {
        return Err(AppError::NotDirectory);
    }
    let cfg = state.rt.current();
    // 需要拆成多个 Telegram 分块的文件统一固定到主 Bot；只有单块文件
    // 才参与额外上传 Bot 的轮询。这样大文件不会在多个 Bot/连接之间
    // 争抢同一个自建 Bot API 进程的上行带宽。
    let target = if requires_primary_upload_target(body.size, cfg.chunk_size) {
        state.vfs.tg.current()?
    } else {
        state.vfs.tg.next_upload()?
    };
    let display_path = normalized_display_path(&body.dir, &name)?;
    let info = state
        .vfs
        .db
        .create_upload_session(
            parent.id,
            &name,
            &display_path,
            body.size,
            &body.sha256,
            cfg.chunk_size as i64,
            target.storage_target_id,
        )
        .await?;
    // 同一路径的可恢复会话可能早已绑定到另一个池成员，状态必须按实际目标返回。
    let session_target = state.vfs.tg.binding(info.session.storage_target_id)?;
    let bot_count = state.vfs.tg.upload_bot_count();
    Ok(Json(serde_json::json!({
        "session": upload_session_json(&info),
        "concurrency": session_upload_concurrency(
            cfg.upload_concurrency,
            bot_count,
            info.part_count(),
        ),
        "bot_count": bot_count,
        "retry_after": session_target.api.upload_retry_after_secs(),
    })))
}

async fn upload_thumbnail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    data: Bytes,
) -> AppResult<Json<serde_json::Value>> {
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    validate_thumbnail_upload(declared, &data)?;
    state.vfs.db.set_upload_thumbnail(&id, &data).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn upload_part(
    State(state): State<AppState>,
    Path((id, idx)): Path<(String, i64)>,
    request: Request,
) -> AppResult<Json<serde_json::Value>> {
    use crate::engine::upload::ResumablePartStart;

    let handle = match state.vfs.uploader.start_resumable_part(&id, idx).await? {
        ResumablePartStart::Complete => {
            let info = state.vfs.db.upload_session_info(&id).await?;
            return Ok(Json(serde_json::json!({
                "already_uploaded": true,
                "session": upload_session_json(&info),
            })));
        }
        ResumablePartStart::Upload(handle) => handle,
    };
    let mut stream = request.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes =
            chunk.map_err(|error| AppError::BadRequest(format!("读取上传分块失败: {error}")))?;
        handle.feed(bytes).await?;
    }
    handle.finish().await?;
    let info = state.vfs.db.upload_session_info(&id).await?;
    Ok(Json(serde_json::json!({
        "already_uploaded": false,
        "session": upload_session_json(&info),
    })))
}

async fn upload_complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.db.complete_upload_session(&id).await?;
    // 覆盖旧文件时，删除任务已在事务内持久化；立即唤醒处理器。
    state.vfs.uploader.process_pending_deletes();
    crate::events::emit(&state.events, crate::events::HostEvent::files());
    Ok(Json(serde_json::json!({
        "ok": true,
        "entry": Entry::from(&node),
    })))
}

async fn upload_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.uploader.begin_cancel_resumable_session(&id).await;
    let cancelled = state.vfs.db.cancel_upload_session(&id).await;
    state.vfs.uploader.finish_cancel_resumable_session(&id);
    let freed = cancelled?;
    state.vfs.uploader.delete_messages_best_effort(freed);
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------- 视频缩略图 ----------

/// 防止上游异常响应占用过多内存；Telegram 自定义缩略图本身限制为 200KB。
const MAX_DOWNLOADED_THUMBNAIL_SIZE: usize = 512 * 1024;

#[derive(Deserialize)]
struct ThumbnailQuery {
    path: String,
}

fn detect_thumbnail_mime(declared: &str, data: &[u8]) -> AppResult<&'static str> {
    if declared == "image/jpeg" && data.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else {
        Err(AppError::BadRequest("只接受有效的 JPEG 缩略图".into()))
    }
}

/// 从 JPEG 的 SOF 段读取尺寸，不解码像素。
fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut pos = 2usize;
    while pos + 3 < data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        let marker = *data.get(pos)?;
        pos += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let segment_len = u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?]) as usize;
        if segment_len < 2 || pos + segment_len > data.len() {
            return None;
        }
        let is_sof = matches!(
            marker,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
        );
        if is_sof {
            if segment_len < 7 {
                return None;
            }
            let height = u16::from_be_bytes([data[pos + 3], data[pos + 4]]);
            let width = u16::from_be_bytes([data[pos + 5], data[pos + 6]]);
            return (width > 0 && height > 0).then_some((width, height));
        }
        pos += segment_len;
    }
    None
}

fn validate_thumbnail_upload(declared: &str, data: &[u8]) -> AppResult<()> {
    if data.is_empty() || data.len() >= crate::telegram::MAX_THUMBNAIL_SIZE {
        return Err(AppError::BadRequest(format!(
            "缩略图大小必须在 1~{}KB 之间（不含上限）",
            crate::telegram::MAX_THUMBNAIL_SIZE / 1024
        )));
    }
    detect_thumbnail_mime(declared, data)?;
    let (width, height) = jpeg_dimensions(data)
        .ok_or_else(|| AppError::BadRequest("JPEG 缩略图缺少有效尺寸".into()))?;
    if width > 320 || height > 320 {
        return Err(AppError::BadRequest(
            "Telegram 缩略图的宽高都不能超过 320 像素".into(),
        ));
    }
    Ok(())
}

fn downloaded_thumbnail_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn thumbnail_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::empty())
        .expect("构造缩略图未命中响应失败")
}

fn thumbnail_etag(file_id: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(file_id.as_bytes());
    format!(
        "\"telegram-thumbnail-{}\"",
        crate::engine::upload::to_hex(&digest)
    )
}

/// 根据第 0 块记录的 Telegram thumbnail file_id 请求小图；不会读取原视频。
async fn thumbnail_get(
    State(state): State<AppState>,
    Query(q): Query<ThumbnailQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let node = state.vfs.resolve(&q.path).await?;
    thumbnail_for_node(&state, &node, &headers).await
}

pub(crate) async fn thumbnail_for_node(
    state: &AppState,
    node: &Node,
    headers: &HeaderMap,
) -> AppResult<Response> {
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let Some(first) = state.vfs.db.first_chunk(node.id).await? else {
        return Ok(thumbnail_not_found());
    };
    let Some(file_id) = first.tg_thumbnail_file_id.as_deref() else {
        // 404 不缓存，历史消息重新登记或替换后可立即显示。
        return Ok(thumbnail_not_found());
    };
    let etag = thumbnail_etag(file_id);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(axum::body::Body::empty())
            .expect("构造缩略图缓存响应失败"));
    }
    let storage_target_id = first
        .storage_target_id
        .ok_or_else(|| AppError::BadRequest("该历史文件没有可用的 Telegram 存储目标".into()))?;
    let binding = state.vfs.tg.binding(storage_target_id)?;
    let reader = binding
        .api
        .open_thumbnail_stream(file_id, first.tg_message_id)
        .await?;
    let mut limited = reader.take((MAX_DOWNLOADED_THUMBNAIL_SIZE + 1) as u64);
    let mut data = Vec::new();
    limited.read_to_end(&mut data).await?;
    if data.len() > MAX_DOWNLOADED_THUMBNAIL_SIZE {
        return Err(AppError::Telegram("Telegram 缩略图大小异常".into()));
    }
    let mime = downloaded_thumbnail_mime(&data)
        .ok_or_else(|| AppError::Telegram("Telegram 返回的缩略图格式无效".into()))?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::ETAG, etag)
        .header("x-content-type-options", "nosniff")
        .body(axum::body::Body::from(data))
        .expect("构造缩略图响应失败"))
}

/// 解析单个 Range;None=无 Range,Err=不可满足
pub(crate) fn parse_range(headers: &HeaderMap, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let Some(spec) = h.strip_prefix("bytes=") else {
        return Ok(None);
    };
    let first = spec.split(',').next().unwrap_or("").trim();
    let Some((a, b)) = first.split_once('-') else {
        return Err(());
    };
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
pub(crate) async fn stream_node(
    state: &AppState,
    node: &Node,
    headers: &HeaderMap,
    inline: bool,
) -> AppResult<Response> {
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let guessed_mime = mime_guess::from_path(&node.name).first_or_octet_stream();
    let safe_inline = inline_mime_allowed(guessed_mime.as_ref());
    let effective_inline = inline && safe_inline;
    // 主动内容即使带 inline=1 也只按二进制附件返回，避免 HTML/SVG/脚本在网盘
    // 自身域名下执行并读取认证 Cookie。
    let response_mime = if inline && !safe_inline {
        "application/octet-stream"
    } else {
        guessed_mime.as_ref()
    };
    let disposition = {
        let kind = if effective_inline {
            "inline"
        } else {
            "attachment"
        };
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
                .header(header::CONTENT_TYPE, response_mime)
                .header(header::CONTENT_LENGTH, total)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
        Ok(Some((start, end))) => {
            let len = end - start + 1;
            let reader = state.vfs.open_reader(node, start).await?;
            let body =
                axum::body::Body::from_stream(ReaderStream::new(reader.with_read_limit(len)));
            Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, response_mime)
                .header(header::CONTENT_LENGTH, len)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", node.size),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
    }
}

fn inline_mime_allowed(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg"
            | "image/png"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "audio/mpeg"
            | "audio/mp4"
            | "audio/aac"
            | "audio/ogg"
            | "audio/webm"
            | "audio/wav"
            | "audio/flac"
            | "video/mp4"
            | "video/webm"
            | "video/ogg"
            | "video/quicktime"
            | "video/x-matroska"
            | "text/plain"
            | "application/pdf"
    )
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
    crate::events::emit(&state.events, crate::events::HostEvent::files());
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn copy_entry(
    State(state): State<AppState>,
    Json(b): Json<FromTo>,
) -> AppResult<Json<serde_json::Value>> {
    state.vfs.copy(&b.from, &b.to).await?;
    crate::events::emit(&state.events, crate::events::HostEvent::files());
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeleteBody {
    paths: Vec<String>,
    /// 默认仍移入回收站，兼容旧版前端及现有 API 调用方。
    #[serde(default)]
    permanent: bool,
}

async fn delete(
    State(state): State<AppState>,
    Json(b): Json<DeleteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut failed = Vec::new();
    let mut deleted = 0;
    for p in &b.paths {
        let result = if b.permanent {
            state
                .vfs
                .delete_permanently(p)
                .await
                .map(|freed| state.vfs.uploader.delete_messages_best_effort(freed))
        } else {
            state.vfs.delete(p).await
        };
        match result {
            Ok(()) => deleted += 1,
            Err(e) => failed.push(serde_json::json!({ "path": p, "error": e.to_string() })),
        }
    }
    if deleted > 0 {
        crate::events::emit(&state.events, crate::events::HostEvent::files());
    }
    Ok(Json(
        serde_json::json!({ "deleted": deleted, "failed": failed }),
    ))
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
            "proxy_enabled": c.proxy.enabled,
            "proxy_url": crate::proxy::mask_url(&c.proxy.url),
            "bot_token": settings::mask(&c.bot_token),
            "upload_bot_tokens": c
                .upload_bot_tokens
                .iter()
                .map(|token| settings::mask(token))
                .collect::<Vec<_>>()
                .join("\n"),
            "chat_id": c.chat_id,
            "chunk_size": c.chunk_size,
            "upload_concurrency": c.upload_concurrency,
            "download_concurrency": c.download_concurrency,
            "send_video": c.send_video,
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
    let masked_upload_tokens = c
        .upload_bot_tokens
        .iter()
        .map(|token| settings::mask(token))
        .collect::<Vec<_>>()
        .join("\n");
    if patch.upload_bot_tokens.as_deref() == Some(masked_upload_tokens.as_str()) {
        patch.upload_bot_tokens = None;
    } else if let Some(value) = patch.upload_bot_tokens.as_deref() {
        // 多行列表允许保留已有脱敏项并增删真实 Token；逐行把占位恢复为原值。
        let resolved = crate::config::parse_bot_tokens(value)
            .into_iter()
            .map(|submitted| {
                c.upload_bot_tokens
                    .iter()
                    .find(|existing| settings::mask(existing) == submitted)
                    .cloned()
                    .unwrap_or(submitted)
            })
            .collect::<Vec<_>>()
            .join(",");
        patch.upload_bot_tokens = Some(resolved);
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
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
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
    let api_base = pick(b.api_base, &c.api_base)
        .trim_end_matches('/')
        .to_string();
    let token = pick(b.bot_token, &c.bot_token);
    let chat_id = pick(b.chat_id, &c.chat_id);
    if token.trim().is_empty() || chat_id.trim().is_empty() {
        return Err(AppError::BadRequest("请先填写 Bot Token 与存储频道".into()));
    }
    let proxy = c.proxy.patched(b.proxy_enabled, b.proxy_url.as_deref())?;
    let bot = Runtime::probe(&api_base, &token, &chat_id, &proxy).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "bot": bot, "api_base": api_base }),
    ))
}

#[derive(Deserialize, Default)]
struct DetectBody {
    api_base: Option<String>,
    bot_token: Option<String>,
    proxy_enabled: Option<bool>,
    proxy_url: Option<String>,
}

/// 根据 Bot Token 探测最近向 bot 发过消息的频道/用户/群,辅助填写
/// 存储频道 chat_id 与收件白名单;不保存任何配置
async fn admin_detect_chats(
    State(state): State<AppState>,
    body: Option<Json<DetectBody>>,
) -> AppResult<Json<serde_json::Value>> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let c = state.rt.current();
    // 表单原样回传脱敏占位时用已保存的值,与 admin_test 同规则
    let pick = |given: Option<String>, cur: &str| -> String {
        match given {
            Some(v) if !v.trim().is_empty() && v != settings::mask(cur) => v.trim().to_string(),
            _ => cur.to_string(),
        }
    };
    let api_base = pick(b.api_base, &c.api_base)
        .trim_end_matches('/')
        .to_string();
    let token = pick(b.bot_token, &c.bot_token);
    if token.trim().is_empty() {
        return Err(AppError::BadRequest("请先填写 Bot Token".into()));
    }
    // 先验证 token 可用,顺带拿 bot 用户名给前端展示操作指引
    let proxy = c.proxy.patched(b.proxy_enabled, b.proxy_url.as_deref())?;
    let bot = Runtime::probe(&api_base, &token, "0", &proxy).await?;
    // 快速拉一次未确认的消息。收件轮询开着时消息会被它抢走(但它会记进
    // seen_chats),这里失败/为空都不致命,合并库里的记录照样能列出来
    match crate::inbox::probe_updates(&api_base, &token, &proxy).await {
        Ok(found) => {
            if let Err(e) = crate::inbox::remember_chats(&state.vfs.db, found).await {
                tracing::warn!("检测:记录会话失败: {e}");
            }
        }
        Err(e) => tracing::warn!("检测:getUpdates 失败(继续用已记录的结果): {e}"),
    }
    let chats = crate::inbox::seen_chats(&state.vfs.db).await?;
    Ok(Json(serde_json::json!({ "bot": bot, "chats": chats })))
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
    let delete_queue = state.vfs.db.delete_queue_overview().await?;
    let extras = crate::extras::load(&state.vfs.db).await?;
    let inbox_failures = state
        .vfs
        .db
        .settings_with_prefix("extra.inbox_failure.")
        .await?
        .len();
    let c = state.rt.current();
    let db_path = state.rt.env.db_path.clone();
    // getMe 只有 30 秒连接超时、没有整体超时：telegram-bot-api 不可达时，管理页每 5 秒
    // 一次的轮询会整个卡住，连带队列计数和自动开关都读不出来。bot 名字只是展示用，
    // 拿不到就先空着，下次轮询再说。
    let bot = tokio::time::timeout(std::time::Duration::from_secs(3), state.rt.bot_name())
        .await
        .unwrap_or(None);
    Ok(Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "bot": bot,
        "configured": c.is_configured(),
        "api_base": c.api_base,
        "official_api": c.is_official_api(),
        "chunk_size": c.chunk_size,
        "upload_concurrency": c.upload_concurrency,
        "download_concurrency": c.download_concurrency,
        "upload_bot_count": state.vfs.tg.upload_bot_count(),
        "send_video": c.send_video,
        "files": files,
        "dirs": dirs,
        "total_size": total,
        "db_size": db_file_size(&db_path),
        "db_path": db_path,
        "listen": state.rt.env.listen.to_string(),
        "started_at": state.rt.started_at,
        "now": crate::db::now_ts(),
        "pending_deletes": delete_queue.pending,
        "delete_queue": delete_queue,
        "auto_delete_message": extras.auto_delete_message,
        "inbox_failures": inbox_failures,
        "tasks": state.tasks.snapshots(),
    })))
}

async fn admin_storage_migration(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.status(target_id).await?))
}

async fn admin_storage_migration_start(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.start(target_id).await?))
}

async fn admin_storage_migration_pause(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.pause(target_id).await?))
}

#[derive(Deserialize, Default)]
struct AdminLogsQuery {
    /// 每页最多 100 条，默认 50。
    limit: Option<i64>,
    /// 上一页最后一条日志 id。
    before: Option<i64>,
    /// 只看某个级别：debug | info | warn | error；其余取值当作不筛选。
    level: Option<String>,
    /// 只看某个类别，取值来自响应里的 `facets.categories`。
    category: Option<String>,
    /// 在类别与事件内容里做子串搜索。
    q: Option<String>,
}

async fn admin_logs(
    State(state): State<AppState>,
    Query(query): Query<AdminLogsQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let filter = BackendLogFilter {
        level: query.level,
        category: query.category,
        keyword: query.q,
    };
    let (items, has_more) = state
        .vfs
        .db
        .backend_logs(query.limit.unwrap_or(50), query.before, &filter)
        .await?;
    // 切面按全库统计，筛选后为空时前端才能说清“是没有还是不在这一页”
    let facets = state.vfs.db.backend_log_facets().await?;
    Ok(Json(
        serde_json::json!({ "items": items, "has_more": has_more, "facets": facets }),
    ))
}

async fn admin_inbox_failures(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let rows = state
        .vfs
        .db
        .settings_with_prefix("extra.inbox_failure.")
        .await?;
    let mut items = Vec::new();
    for (update_id, raw) in rows {
        if let Ok(serde_json::Value::Object(mut value)) =
            serde_json::from_str::<serde_json::Value>(&raw)
        {
            value.insert(
                "update_id".into(),
                serde_json::json!(update_id.parse::<i64>().unwrap_or(0)),
            );
            items.push(serde_json::Value::Object(value));
        }
    }
    items.sort_by_key(|item| item["update_id"].as_i64().unwrap_or(0));
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn admin_inbox_failure_remove(
    State(state): State<AppState>,
    axum::extract::Path(update_id): axum::extract::Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    state
        .vfs
        .db
        .setting_del(&format!("extra.inbox_failure.{update_id}"))
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn admin_inbox_failures_clear(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let rows = state
        .vfs
        .db
        .settings_with_prefix("extra.inbox_failure.")
        .await?;
    let deletes: Vec<String> = rows
        .keys()
        .map(|update_id| format!("extra.inbox_failure.{update_id}"))
        .collect();
    state.vfs.db.settings_apply(&[], &deletes).await?;
    Ok(Json(serde_json::json!({ "cleared": deletes.len() })))
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
    resp.headers_mut().insert(
        header::SET_COOKIE,
        state
            .auth
            .clear_cookie_header()
            .parse()
            .expect("cookie 头合法"),
    );
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

#[derive(Deserialize, Default)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    extensions: Option<String>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    modified_after: Option<i64>,
    modified_before: Option<i64>,
    #[serde(default)]
    favorite: bool,
    /// 只在该目录子树内搜索；缺省或 `/` 表示整库。
    #[serde(default)]
    pub(crate) under: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    order: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if q.min_size.is_some_and(|size| size < 0) || q.max_size.is_some_and(|size| size < 0) {
        return Err(AppError::BadRequest("文件大小不能为负数".into()));
    }
    if let (Some(min), Some(max)) = (q.min_size, q.max_size)
        && min > max
    {
        return Err(AppError::BadRequest(
            "最小文件大小不能大于最大文件大小".into(),
        ));
    }
    if let (Some(after), Some(before)) = (q.modified_after, q.modified_before)
        && after > before
    {
        return Err(AppError::BadRequest("开始时间不能晚于结束时间".into()));
    }
    let is_dir = match q.kind.as_deref().unwrap_or("all") {
        "all" | "" => None,
        "file" => Some(false),
        "dir" => Some(true),
        _ => return Err(AppError::BadRequest("搜索类型无效".into())),
    };
    let extensions: Vec<String> = q
        .extensions
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect();
    if extensions.len() > 20
        || extensions.iter().any(|ext| {
            ext.len() > 40
                || !ext
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        })
    {
        return Err(AppError::BadRequest("文件后缀筛选格式无效".into()));
    }
    let sort = match q.sort.as_deref().unwrap_or("name") {
        "name" | "" => SearchSort::Name,
        "size" => SearchSort::Size,
        "modified" => SearchSort::Modified,
        "created" => SearchSort::Created,
        _ => return Err(AppError::BadRequest("搜索排序字段无效".into())),
    };
    let descending = match q.order.as_deref().unwrap_or("asc") {
        "asc" | "" => false,
        "desc" => true,
        _ => return Err(AppError::BadRequest("搜索排序方向无效".into())),
    };
    let under = match q.under.as_deref() {
        None => None,
        Some(raw) => {
            let segments = crate::vfs::normalize(raw)?;
            (!segments.is_empty()).then(|| format!("/{}", segments.join("/")))
        }
    };
    let filter = SearchFilter {
        query: q
            .q
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        is_dir,
        extensions,
        min_size: q.min_size,
        max_size: q.max_size,
        modified_after: q.modified_after,
        modified_before: q.modified_before,
        favorite_only: q.favorite,
        under,
        sort,
        descending,
    };
    let hits = state
        .vfs
        .db
        .search_filtered(&filter, q.limit.unwrap_or(500).clamp(1, 2000))
        .await?;
    let results: Vec<serde_json::Value> = hits
        .iter()
        .map(|(n, path)| {
            serde_json::json!({
                "path": path,
                "name": n.name,
                "is_dir": n.is_dir,
                "size": n.size,
                "created_at": n.created_at,
                "modified_at": n.modified_at,
                "favorite": n.favorite,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": results })))
}

#[derive(Deserialize)]
pub(crate) struct FavoriteBody {
    pub(crate) path: String,
    favorite: bool,
}

pub(crate) async fn favorite_set(
    State(state): State<AppState>,
    Json(body): Json<FavoriteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&body.path).await?;
    if node.id == crate::db::ROOT_ID {
        return Err(AppError::BadRequest("不能收藏根目录".into()));
    }
    let updated = state.vfs.db.set_favorite(node.id, body.favorite).await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "favorite": updated.favorite,
    })))
}

#[derive(Deserialize)]
struct InstantBody {
    dir: String,
    name: String,
    sha256: String,
    size: i64,
}

fn normalized_sha256(raw: &str) -> AppResult<String> {
    if raw.len() != 64 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest("sha256 格式不对".into()));
    }
    Ok(raw.to_ascii_lowercase())
}

/// 秒传探测:库里已有同内容文件则零流量落位,否则让前端走真实上传
async fn instant(
    State(state): State<AppState>,
    Json(b): Json<InstantBody>,
) -> AppResult<Json<serde_json::Value>> {
    let name = clean_filename(&b.name)?;
    let sha = normalized_sha256(&b.sha256)?;
    if b.size < 0 {
        return Err(AppError::BadRequest("大小无效".into()));
    }
    let dir = state.vfs.resolve(&b.dir).await?;
    if !dir.is_dir {
        return Err(AppError::NotDirectory);
    }
    match state
        .vfs
        .db
        .instant_link(&sha, b.size, dir.id, &name)
        .await?
    {
        Some((id, freed)) => {
            state.vfs.uploader.delete_messages_best_effort(freed);
            let node = state.vfs.db.node(id).await?;
            crate::events::emit(&state.events, crate::events::HostEvent::files());
            Ok(Json(serde_json::json!({
                "linked": true,
                "pending": false,
                "entry": Entry::from(&node),
            })))
        }
        None => {
            let pending = state.vfs.db.uploading_child_exists(dir.id, &name).await?;
            Ok(Json(
                serde_json::json!({ "linked": false, "pending": pending }),
            ))
        }
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
    let in_trash: Vec<i64> = state
        .vfs
        .db
        .trash_list()
        .await?
        .iter()
        .map(|(n, _, _)| n.id)
        .collect();
    if b.all {
        return Ok(in_trash);
    }
    Ok(b.ids
        .iter()
        .copied()
        .filter(|id| in_trash.contains(id))
        .collect())
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
    if !restored.is_empty() {
        crate::events::emit(&state.events, crate::events::HostEvent::files());
    }
    Ok(Json(
        serde_json::json!({ "restored": restored, "failed": failed }),
    ))
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
    if purged > 0 {
        crate::events::emit(&state.events, crate::events::HostEvent::files());
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
    inline: Option<u8>,
}

#[derive(Deserialize, Default)]
struct ShareAuthorizeBody {
    #[serde(default)]
    password: Option<String>,
}

/// token → (share, 分享根节点);过期/已删除在此统一拦下
async fn share_access(state: &AppState, token: &str) -> AppResult<(crate::db::Share, Node)> {
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

fn share_check_pwd(s: &crate::db::Share, pwd: Option<&str>) -> AppResult<()> {
    if let Some(want) = &s.password
        && !pwd.is_some_and(|given| Auth::secrets_equal(given, want))
    {
        return Err(AppError::Forbidden("提取码错误".into()));
    }
    Ok(())
}

/// 分享内相对路径 → 节点;normalize 拒绝 `..`,不会越出分享根
async fn share_resolve(state: &AppState, root: &Node, rel: &str) -> AppResult<Node> {
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

/// 提取码只通过 POST 请求体提交；成功后签发短期、分享范围内的 HttpOnly 票据。
async fn share_authorize(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    peer: ConnectInfo<SocketAddr>,
    Json(body): Json<ShareAuthorizeBody>,
) -> AppResult<Response> {
    const LIMIT: usize = 10;
    const WINDOW: i64 = 10 * 60;

    let (share, _) = share_access(&state, &token).await?;
    let key = format!("share:{token}:{}", peer_key(peer));
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

fn require_share_ticket(
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

async fn share_list_dir(
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

async fn share_download(
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

// ---------- 备份 / 维护配置 ----------

/// 下载数据库一致性快照(VACUUM INTO 临时文件后流式返回)
async fn admin_backup(State(state): State<AppState>) -> AppResult<Response> {
    let ts = crate::db::now_ts();
    let cfg = state.rt.current();
    let temp_dir = tempfile::Builder::new()
        .prefix("tgdrive-download-")
        .tempdir_in(&cfg.tmp_dir)?;
    let tmp = temp_dir.path().join("snapshot.db");
    let tmp_str = tmp
        .to_str()
        .ok_or_else(|| AppError::BadRequest("临时目录路径无效".into()))?;
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

/// 立即把同一份数据库快照投递到全部已启用渠道。
async fn admin_backup_now(
    State(state): State<AppState>,
) -> AppResult<Json<crate::backup::BackupOutcome>> {
    Ok(Json(
        crate::backup::backup_now(&state.vfs, &state.rt).await?,
    ))
}

async fn extras_get(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let ex = crate::extras::load(&state.vfs.db).await?;
    let last_by_target = crate::backup::last_times(&state.vfs.db).await?;
    let last_backup = [
        last_by_target.telegram,
        last_by_target.webdav,
        last_by_target.s3,
    ]
    .into_iter()
    .flatten()
    .max();
    let trash_count = state.vfs.db.trash_list().await?.len();
    Ok(Json(serde_json::json!({
        "extras": ex,
        "backup_last_ts": last_backup,
        "backup_last_by_target": last_by_target,
        "trash_count": trash_count,
    })))
}

async fn extras_set(
    State(state): State<AppState>,
    Json(p): Json<crate::extras::ExtrasPatch>,
) -> AppResult<Json<serde_json::Value>> {
    let current = crate::extras::load(&state.vfs.db).await?;
    let telegram_enabled = p
        .backup_telegram_enabled
        .unwrap_or(current.backup_telegram_enabled);
    if telegram_enabled && let Some(raw) = p.backup_chunk_size.as_deref() {
        let size = crate::extras::parse_backup_chunk_size(raw)?;
        crate::extras::validate_backup_chunk_size(&state.rt.current().api_base, size)?;
    }
    if p.backup_auto.unwrap_or(current.backup_auto)
        && telegram_enabled
        && !state.rt.current().is_configured()
    {
        return Err(AppError::BadRequest(
            "启用 Telegram 自动备份前请先配置 Bot Token 与存储频道，或关闭该备份渠道".into(),
        ));
    }
    crate::extras::save(&state.vfs.db, &p).await?;
    // 队列开关要立刻推给已在跑的 uploader worker，不然得等下次重启才生效。
    let saved = crate::extras::load(&state.vfs.db).await?;
    state.rt.set_delete_queue_mode(saved.auto_delete_message);
    if saved.auto_delete_message {
        state.vfs.uploader.process_pending_deletes();
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 后台「立即执行」一次队列。单次上限 200 条，返回剩余量让前端提示可否再点。
const MANUAL_QUEUE_BATCH: i64 = 200;

async fn admin_run_delete_queue(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let summary = state
        .vfs
        .uploader
        .run_delete_batch(MANUAL_QUEUE_BATCH, QueueTrigger::Manual)
        .await?;
    state
        .vfs
        .db
        .append_backend_log(
            if summary.failed + summary.discarded > 0 {
                "warn"
            } else {
                "info"
            },
            "消息清理",
            &format!(
                "手动执行完成：成功 {} 条，永久跳过 {} 条，失败 {} 条，剩余 {} 条{}",
                summary.completed,
                summary.discarded,
                summary.failed,
                summary.pending,
                if summary.stopped_early {
                    "（本轮超时收工，可再执行一次）"
                } else {
                    ""
                }
            ),
        )
        .await
        .ok();
    Ok(Json(serde_json::json!(summary)))
}

async fn admin_clear_delete_queue(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let cleared = state.vfs.db.clear_delete_jobs().await?;
    state
        .vfs
        .db
        .append_backend_log(
            "warn",
            "消息清理",
            &format!("后台手动清空消息清理队列，丢弃 {cleared} 条任务；对应消息会留在频道里"),
        )
        .await
        .ok();
    Ok(Json(serde_json::json!({ "cleared": cleared })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_input_validation() {
        let jpeg_320x200 = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0xc8, 0x01, 0x40, 0x01, 0x01, 0x11,
            0x00, 0xff, 0xd9,
        ];
        assert_eq!(jpeg_dimensions(&jpeg_320x200), Some((320, 200)));
        assert!(validate_thumbnail_upload("image/jpeg", &jpeg_320x200).is_ok());
        assert!(validate_thumbnail_upload("image/webp", &jpeg_320x200).is_err());

        let mut too_wide = jpeg_320x200;
        too_wide[9] = 0x01;
        too_wide[10] = 0x41;
        assert!(validate_thumbnail_upload("image/jpeg", &too_wide).is_err());
        assert!(detect_thumbnail_mime("image/png", b"not-an-image").is_err());
        assert_eq!(downloaded_thumbnail_mime(&jpeg_320x200), Some("image/jpeg"));
    }

    #[test]
    fn sha256_input_validation() {
        let upper = "A".repeat(64);
        assert_eq!(normalized_sha256(&upper).unwrap(), "a".repeat(64));
        assert!(normalized_sha256("bad").is_err());
    }

    #[test]
    fn only_multi_part_uploads_use_primary_bot() {
        assert!(!requires_primary_upload_target(0, 256));
        assert!(!requires_primary_upload_target(256, 256));
        assert!(requires_primary_upload_target(257, 256));
        assert!(!requires_primary_upload_target(-1, 256));
    }

    #[test]
    fn multi_part_session_reports_single_concurrency() {
        assert_eq!(session_upload_concurrency(5, 3, 9), 1);
        assert_eq!(session_upload_concurrency(5, 3, 1), 15);
        assert_eq!(session_upload_concurrency(0, 0, 1), 1);
    }

    #[test]
    fn delete_body_defaults_to_recycle_bin_and_accepts_permanent_mode() {
        let recycle: DeleteBody =
            serde_json::from_value(serde_json::json!({ "paths": ["/a.txt"] })).unwrap();
        assert!(!recycle.permanent);

        let permanent: DeleteBody = serde_json::from_value(serde_json::json!({
            "paths": ["/a.txt"],
            "permanent": true
        }))
        .unwrap();
        assert!(permanent.permanent);
    }

    #[test]
    fn range_parsing_covers_boundaries() {
        fn parse(value: &str, total: u64) -> Result<Option<(u64, u64)>, ()> {
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, value.parse().unwrap());
            parse_range(&headers, total)
        }

        assert_eq!(parse_range(&HeaderMap::new(), 10), Ok(None));
        assert_eq!(parse("bytes=2-5", 10), Ok(Some((2, 5))));
        assert_eq!(parse("bytes=8-99", 10), Ok(Some((8, 9))));
        assert_eq!(parse("bytes=7-", 10), Ok(Some((7, 9))));
        assert_eq!(parse("bytes=-3", 10), Ok(Some((7, 9))));
        assert_eq!(parse("bytes=-99", 10), Ok(Some((0, 9))));
        assert_eq!(parse("bytes=0-1, 5-6", 10), Ok(Some((0, 1))));
        assert_eq!(parse("items=0-1", 10), Ok(None));
        assert!(parse("bytes=10-", 10).is_err());
        assert!(parse("bytes=5-4", 10).is_err());
        assert!(parse("bytes=0-0", 0).is_err());
    }

    #[test]
    fn inline_mime_blocks_active_content() {
        for mime in [
            "image/jpeg",
            "image/png",
            "audio/mpeg",
            "video/mp4",
            "text/plain",
            "application/pdf",
        ] {
            assert!(inline_mime_allowed(mime), "应允许内联显示 {mime}");
        }
        for mime in [
            "text/html",
            "image/svg+xml",
            "application/xhtml+xml",
            "application/javascript",
            "text/css",
            "application/octet-stream",
        ] {
            assert!(!inline_mime_allowed(mime), "应阻止内联显示 {mime}");
        }
    }
}
