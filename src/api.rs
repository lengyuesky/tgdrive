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
pub(crate) use self::media::{
    downloaded_thumbnail_mime, inline_mime_allowed, parse_range, stream_node, thumbnail_for_node,
};
pub(crate) use self::search::{FavoriteBody, SearchQuery, favorite_set, search};
mod admin;
mod events;
mod files;
mod media;
mod search;
mod security;
mod session;
mod shares;
mod trash;
mod uploads;
use self::{
    admin::*, events::*, files::*, media::*, search::*, security::*, session::*, shares::*,
    trash::*, uploads::*,
};

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
        .route("/metrics", get(metrics))
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
        .route("/admin/security", get(security_status))
        .route("/admin/security/totp/setup", post(totp_setup))
        .route("/admin/security/totp/enable", post(totp_enable))
        .route("/admin/security/totp/disable", post(totp_disable))
        .route(
            "/admin/security/totp/recovery",
            post(totp_recovery_regenerate),
        )
        .route(
            "/admin/security/webdav-password",
            post(webdav_password_generate).delete(webdav_password_revoke),
        )
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
        .layer(middleware::from_fn(crate::security::csrf_guard))
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
    match authorize(&state, &req, true) {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

/// Basic 认证失败额度：与网页登录分开计数，WebDAV 客户端里残留的旧密码不会
/// 连带锁住网页登录；客户端会自动重试，额度比登录略宽。
const BASIC_LIMIT: usize = 10;
const BASIC_WINDOW: i64 = 15 * 60;

/// 校验登录 Cookie(`allow_cookie` 时)或 Basic 凭据。Basic 失败按来源地址计入
/// 限流，锁定期间即使密码正确也拒绝，避免借 API/WebDAV 入口无限次猜密码。
pub(crate) fn authorize(state: &AppState, req: &Request, allow_cookie: bool) -> AppResult<()> {
    if allow_cookie && state.auth.session_cookie_valid(req.headers()) {
        return Ok(());
    }
    let Some((user, pass)) = crate::auth::basic_credentials(req.headers()) else {
        return Err(AppError::Unauthorized);
    };
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || "unknown".to_string(),
            |info| peer_key(state, *info, req.headers()),
        );
    let key = format!("basic:{peer}");
    if let Some(wait) = state.attempts.retry_after(&key, BASIC_LIMIT, BASIC_WINDOW) {
        return Err(AppError::RateLimited(wait));
    }
    if !state.auth.verify_basic(&user, &pass) {
        state.attempts.record_failure(&key, BASIC_WINDOW);
        return Err(AppError::Unauthorized);
    }
    // 成功不清零：同一出口地址后的正常客户端不能替猜密码者重置额度。
    Ok(())
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
