//! 可恢复分块上传。

use super::*;

// ---------- 可恢复上传 ----------

#[derive(Deserialize)]
pub(super) struct UploadCreateBody {
    pub(super) dir: String,
    pub(super) name: String,
    pub(super) size: i64,
    pub(super) sha256: String,
}

pub(super) fn upload_session_json(info: &UploadSessionInfo) -> serde_json::Value {
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
pub(super) fn requires_primary_upload_target(size: i64, chunk_size: u64) -> bool {
    size > 0
        && u64::try_from(size)
            .ok()
            .is_some_and(|size| size > chunk_size)
}

pub(super) fn session_upload_concurrency(
    per_bot: usize,
    bot_count: usize,
    part_count: i64,
) -> usize {
    if part_count > 1 {
        1
    } else {
        per_bot.saturating_mul(bot_count.max(1)).max(1)
    }
}

pub(super) fn normalized_display_path(dir: &str, name: &str) -> AppResult<String> {
    let mut segments = crate::vfs::normalize(dir)?;
    segments.push(name.to_string());
    Ok(format!("/{}", segments.join("/")))
}

pub(super) async fn uploads_index(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
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

pub(super) async fn upload_status(
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

pub(super) async fn uploads_create(
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

pub(super) async fn upload_thumbnail(
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

pub(super) async fn upload_part(
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

pub(super) async fn upload_complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.db.complete_upload_session(&id).await?;
    // 覆盖旧文件时，删除任务已在事务内持久化；立即唤醒处理器。
    state.vfs.uploader.process_pending_deletes();
    emit_files_in_parent(&state, node.parent_id).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "entry": Entry::from(&node),
    })))
}

pub(super) async fn upload_cancel(
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
