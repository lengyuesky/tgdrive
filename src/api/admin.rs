//! 后台管理、备份与维护配置。

use super::*;

// ---------- 后台管理 ----------

/// 密码一律以固定长度占位返回,连长度都不泄露
pub(super) const PASSWORD_PLACEHOLDER: &str = "••••••••";

pub(super) async fn admin_config(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
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

pub(super) async fn admin_save(
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
pub(super) struct ResetBody {
    pub(super) keys: Vec<String>,
}

pub(super) async fn admin_reset(
    State(state): State<AppState>,
    Json(b): Json<ResetBody>,
) -> AppResult<Json<ApplyOutcome>> {
    Ok(Json(state.rt.reset(&b.keys).await?))
}

#[derive(Deserialize, Default)]
pub(super) struct TestBody {
    pub(super) api_base: Option<String>,
    pub(super) bot_token: Option<String>,
    pub(super) chat_id: Option<String>,
    pub(super) proxy_enabled: Option<bool>,
    pub(super) proxy_url: Option<String>,
}

/// 试连当前或页面上正在编辑的配置,不保存
pub(super) async fn admin_test(
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
pub(super) struct DetectBody {
    pub(super) api_base: Option<String>,
    pub(super) bot_token: Option<String>,
    pub(super) proxy_enabled: Option<bool>,
    pub(super) proxy_url: Option<String>,
}

/// 根据 Bot Token 探测最近向 bot 发过消息的频道/用户/群,辅助填写
/// 存储频道 chat_id 与收件白名单;不保存任何配置
pub(super) async fn admin_detect_chats(
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
pub(super) fn db_file_size(path: &str) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|sfx| std::fs::metadata(format!("{path}{sfx}")).ok())
        .map(|m| m.len())
        .sum()
}

pub(super) async fn admin_status(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
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

pub(super) async fn admin_storage_migration(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.status(target_id).await?))
}

pub(super) async fn admin_storage_migration_start(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.start(target_id).await?))
}

pub(super) async fn admin_storage_migration_pause(
    State(state): State<AppState>,
) -> AppResult<Json<crate::migration::StorageMigrationStatus>> {
    let target_id = crate::migration::primary_target_id(state.rt.current().storage_target_id)?;
    Ok(Json(state.migration.pause(target_id).await?))
}

#[derive(Deserialize, Default)]
pub(super) struct AdminLogsQuery {
    /// 每页最多 100 条，默认 50。
    pub(super) limit: Option<i64>,
    /// 上一页最后一条日志 id。
    pub(super) before: Option<i64>,
    /// 只看某个级别：debug | info | warn | error；其余取值当作不筛选。
    pub(super) level: Option<String>,
    /// 只看某个类别，取值来自响应里的 `facets.categories`。
    pub(super) category: Option<String>,
    /// 在类别与事件内容里做子串搜索。
    pub(super) q: Option<String>,
}

pub(super) async fn admin_logs(
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

pub(super) async fn admin_inbox_failures(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
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

pub(super) async fn admin_inbox_failure_remove(
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

pub(super) async fn admin_inbox_failures_clear(
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

pub(super) async fn admin_cleanup(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let freed = state.vfs.db.cleanup_stale_uploads().await?;
    let n = freed.len();
    state.vfs.uploader.delete_messages_best_effort(freed);
    Ok(Json(serde_json::json!({ "cleaned": n })))
}

/// 轮换签名密钥:所有设备上的登录态(含本会话)立即失效
pub(super) async fn admin_logout_all(State(state): State<AppState>) -> AppResult<Response> {
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

pub(super) async fn admin_env_export(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.rt.env_export(),
    )
        .into_response()
}

// ---------- 备份 / 维护配置 ----------

/// 下载数据库一致性快照(VACUUM INTO 临时文件后流式返回)
pub(super) async fn admin_backup(State(state): State<AppState>) -> AppResult<Response> {
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
pub(super) async fn admin_backup_now(
    State(state): State<AppState>,
) -> AppResult<Json<crate::backup::BackupOutcome>> {
    Ok(Json(
        crate::backup::backup_now(&state.vfs, &state.rt).await?,
    ))
}

pub(super) async fn extras_get(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
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

pub(super) async fn extras_set(
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
pub(super) const MANUAL_QUEUE_BATCH: i64 = 200;

pub(super) async fn admin_run_delete_queue(
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
            if summary.failed > 0 { "warn" } else { "info" },
            "消息清理",
            &format!(
                "手动执行完成：成功 {} 条，超过 48 小时无法删除已跳过 {} 条，失败 {} 条，剩余 {} 条{}",
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

pub(super) async fn admin_clear_delete_queue(
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

/// Prometheus 文本格式指标。需登录（Prometheus 可用 Basic 认证抓取）。
pub(super) async fn metrics(State(state): State<AppState>) -> AppResult<Response> {
    use std::fmt::Write as _;
    let (files, dirs, total) = state.vfs.db.stats().await?;
    let queue = state.vfs.db.delete_queue_overview().await?;
    let mut out = String::new();
    crate::security::write_http_metrics(&mut out);
    let gauges: [(&str, &str, i64); 5] = [
        ("tgdrive_files", "可见文件数", files),
        ("tgdrive_dirs", "可见目录数（不含根）", dirs),
        ("tgdrive_stored_bytes", "可见文件逻辑总大小", total),
        (
            "tgdrive_delete_queue_pending",
            "待删除的 Telegram 消息数",
            queue.pending,
        ),
        (
            "tgdrive_db_bytes",
            "元数据库文件大小",
            db_file_size(&state.rt.env.db_path) as i64,
        ),
    ];
    for (name, help, value) in gauges {
        let _ = writeln!(
            out,
            "# HELP {name} {help}。\n# TYPE {name} gauge\n{name} {value}"
        );
    }
    out.push_str(
        "# HELP tgdrive_task_running 后台任务是否在运行。\n# TYPE tgdrive_task_running gauge\n",
    );
    for task in state.tasks.snapshots() {
        let _ = writeln!(
            out,
            "tgdrive_task_running{{task=\"{}\"}} {}",
            task.name,
            u8::from(task.running)
        );
    }
    out.push_str("# HELP tgdrive_task_restarts_total 后台任务重启次数。\n# TYPE tgdrive_task_restarts_total counter\n");
    for task in state.tasks.snapshots() {
        let _ = writeln!(
            out,
            "tgdrive_task_restarts_total{{task=\"{}\"}} {}",
            task.name, task.restarts
        );
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response())
}
