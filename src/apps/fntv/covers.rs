//! 影视封面：来源身份、可再生缓存及单并发后台处理，不在页面查询中解码视频。

use super::{
    APP_ID, Context, authorized, data, frame, guid,
    index::{self, Entry, Source, Thumbnail},
};
use crate::{
    api::AppState,
    db::{Db, apps::CoverLimits},
    error::{AppError, AppResult},
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

pub(super) const IMAGE_LIMIT: usize = 256 * 1024;
const POSTER_LIMIT: usize = 2 * 1024 * 1024;
const THUMBNAIL_LIMIT: usize = 512 * 1024;

#[derive(Clone, Deserialize, Serialize)]
struct Selected {
    key: String,
    kind: String,
    source: Option<Source>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Record {
    fingerprint: String,
    state: String,
    selected: Option<Selected>,
    error: String,
    attempts: u8,
    next_retry: i64,
}

impl Record {
    fn queued(entry: &Entry) -> Self {
        Self {
            fingerprint: fingerprint(entry),
            state: "queued".into(),
            selected: None,
            error: String::new(),
            attempts: 0,
            next_retry: 0,
        }
    }
}

fn candidates(entry: &Entry) -> Vec<Source> {
    if entry.posters.is_empty() {
        entry.poster.iter().cloned().collect()
    } else {
        entry.posters.clone()
    }
}

fn fingerprint(entry: &Entry) -> String {
    let posters = candidates(entry)
        .iter()
        .map(|s| s.guid("poster"))
        .collect::<Vec<_>>()
        .join(":");
    let thumb = entry
        .thumbnail
        .as_ref()
        .map(|t| {
            guid(&[
                &t.target.unwrap_or_default().to_string(),
                &t.message.to_string(),
                &t.file,
            ])
        })
        .unwrap_or_default();
    guid(&["fntv-cover-policy-1", &entry.media_guid, &posters, &thumb])
}

async fn inputs(state: &AppState, mut entries: Vec<Entry>) -> AppResult<Vec<Entry>> {
    let ids: Vec<_> = entries.iter().map(|e| e.node_id).collect();
    let chunks: HashMap<_, _> = state
        .vfs
        .db
        .first_chunks(&ids)
        .await?
        .into_iter()
        .map(|c| (c.node_id, c))
        .collect();
    for entry in &mut entries {
        entry.thumbnail = chunks.get(&entry.node_id).and_then(|c| {
            c.tg_thumbnail_file_id.as_ref().map(|file| Thumbnail {
                target: c.storage_target_id,
                message: c.tg_message_id,
                file: file.clone(),
            })
        });
    }
    Ok(entries)
}

async fn records(state: &AppState) -> AppResult<HashMap<String, Record>> {
    Ok(state
        .vfs
        .db
        .cover_states(APP_ID)
        .await?
        .into_iter()
        .filter_map(|(key, meta)| serde_json::from_str(&meta).ok().map(|r| (key, r)))
        .collect())
}

/// 列表只查小型状态和首块引用，不读取缓存图片，更不读原视频。
pub(super) async fn decorate(state: &AppState, entries: Vec<Entry>) -> AppResult<Vec<Entry>> {
    let mut entries = inputs(state, entries).await?;
    let records = records(state).await?;
    let mut changed = false;
    for entry in &mut entries {
        if let Some(record) = records
            .get(&entry.media_guid)
            .filter(|r| r.fingerprint == fingerprint(entry))
        {
            entry.cover = record
                .selected
                .as_ref()
                .filter(|_| record.state == "ready")
                .map(|s| s.key.clone());
        } else {
            changed = true;
        }
    }
    if changed {
        state.vfs.db.fntv.cover_wake.notify_one();
    }
    Ok(entries)
}

pub(super) async fn schedule(state: &AppState, context: &Context, manual: bool) -> AppResult<()> {
    let entries = inputs(state, index::entries(state).await?).await?;
    let old = records(state).await?;
    let images: HashSet<_> = state
        .vfs
        .db
        .cover_metadata(APP_ID)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    // 调用方持有生命周期锁；文件读取仍另行检查当前节点。
    context.lease.check()?;
    let mut live = Vec::new();
    for entry in entries {
        let previous = old.get(&entry.media_guid);
        let reset = previous.is_none_or(|r| r.fingerprint != fingerprint(&entry))
            || manual
                && previous.is_some_and(|r| {
                    r.state == "failed"
                        || r.state == "ready"
                            && r.selected.as_ref().is_none_or(|s| !images.contains(&s.key))
                });
        if reset {
            if authorized(state, context, entry.node_id, &entry.version)
                .await
                .is_err()
            {
                continue;
            }
            let record = Record::queued(&entry);
            state
                .vfs
                .db
                .cover_state_put(
                    APP_ID,
                    &entry.media_guid,
                    &record.fingerprint,
                    &serde_json::to_string(&record).map_err(|e| AppError::Internal(e.into()))?,
                )
                .await?;
        }
        // 未变条目来自已经发布的索引；实际读图和生成仍重新核验当前授权。
        live.push(entry.media_guid.clone());
    }
    state.vfs.db.cover_state_prune(APP_ID, &live).await?;
    let live: HashSet<_> = live.into_iter().collect();
    let stale: Vec<_> = state
        .vfs
        .db
        .cover_metadata(APP_ID)
        .await?
        .into_iter()
        .filter_map(|(key, meta)| {
            let metadata: Value = serde_json::from_str(meta.as_deref().unwrap_or("")).ok()?;
            (!live.contains(metadata["media_guid"].as_str().unwrap_or(""))).then_some(key)
        })
        .collect();
    state.vfs.db.cover_delete(APP_ID, &stale).await?;
    context.lease.check()?;
    state.vfs.db.fntv.cover_wake.notify_one();
    Ok(())
}

pub(super) async fn status(state: &AppState, context: &Context) -> AppResult<Value> {
    let entries = inputs(state, index::entries(state).await?).await?;
    let records = records(state).await?;
    let images: HashSet<_> = state
        .vfs
        .db
        .cover_metadata(APP_ID)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let (mut ready, mut pending, mut failed, mut retrying) = (0, 0, 0, 0);
    let mut errors = Vec::new();
    for entry in entries {
        if authorized(state, context, entry.node_id, &entry.version)
            .await
            .is_err()
        {
            continue;
        }
        match records
            .get(&entry.media_guid)
            .filter(|r| r.fingerprint == fingerprint(&entry))
        {
            Some(r)
                if r.state == "ready"
                    && r.selected.as_ref().is_some_and(|s| images.contains(&s.key)) =>
            {
                ready += 1
            }
            Some(r) if r.state == "failed" => {
                failed += 1;
                if r.next_retry > 0 {
                    retrying += 1;
                }
                if errors.len() < 12 {
                    errors.push(
                        json!({"title":entry.title,"reason":r.error,"retry_at":r.next_retry}),
                    );
                }
            }
            _ => pending += 1,
        }
    }
    let stats = state.vfs.db.cover_stats(APP_ID).await?;
    Ok(
        json!({"ready":ready,"pending":pending,"failed":failed,"retrying":retrying,"errors":errors,
        "bytes":stats.bytes,"limit_bytes":crate::db::apps::COVER_LIMIT_BYTES,"frame_available":frame::available().await}),
    )
}

pub(super) struct Foreground {
    db: Db,
    epoch: u64,
}
impl Drop for Foreground {
    fn drop(&mut self) {
        self.db.fntv.end_foreground(self.epoch);
    }
}
pub(super) fn foreground(state: &AppState) -> Foreground {
    let epoch = state.vfs.db.fntv.begin_foreground();
    Foreground {
        db: state.vfs.db.clone(),
        epoch,
    }
}
pub(super) fn foreground_body(body: Body, guard: Foreground) -> Body {
    Body::from_stream(futures_util::stream::try_unfold(
        (body.into_data_stream(), guard),
        |(mut stream, guard)| async move {
            match stream.next().await {
                Some(bytes) => bytes.map(|bytes| Some((bytes, (stream, guard)))),
                None => Ok(None),
            }
        },
    ))
}

pub(super) fn start_worker(state: &AppState) {
    let lease = state.vfs.db.fntv.lease();
    {
        let mut inner = state.vfs.db.fntv.lock();
        if inner.cover_worker_epoch == lease.epoch {
            return;
        }
        inner.cover_worker_epoch = lease.epoch;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let _single = tokio::select! { biased; _=lease.cancelled.cancelled()=>return, lock=state.vfs.db.fntv.cover_lock.lock()=>lock };
        loop {
            let Ok(context) = super::context(&state, true).await else {
                break;
            };
            if context.lease.epoch != lease.epoch
                || lease.cancelled.is_cancelled()
                || state.restore.shutdown.is_cancelled()
            {
                break;
            }
            {
                let _guard = super::super::INSTALL_LOCK.lock().await;
                if let Err(error) = schedule(&state, &context, false).await {
                    tracing::warn!(%error, "影视封面任务状态稍后重试");
                }
            }
            // 消费本次状态发布的唤醒，不在一批图片内为每一项重复加载整个索引。
            let _ = state.vfs.db.fntv.cover_wake.notified().now_or_never();
            let mut delay = Duration::from_secs(30);
            if let (Ok(entries), Ok(records)) = (
                inputs(&state, index::entries(&state).await.unwrap_or_default()).await,
                records(&state).await,
            ) {
                if let Some(next) = records
                    .values()
                    .filter(|r| r.next_retry > crate::db::now_ts())
                    .map(|r| r.next_retry)
                    .min()
                {
                    delay = delay.min(Duration::from_secs(
                        (next - crate::db::now_ts()).max(1) as u64
                    ));
                }
                for entry in entries {
                    let Some(mut record) = records
                        .get(&entry.media_guid)
                        .filter(|r| {
                            r.fingerprint == fingerprint(&entry)
                                && (r.state == "queued"
                                    || r.state == "failed"
                                        && r.next_retry > 0
                                        && r.next_retry <= crate::db::now_ts())
                        })
                        .cloned()
                    else {
                        continue;
                    };
                    let Some(task_context) = state.vfs.db.fntv.background_context(&context) else {
                        delay = Duration::from_secs(2);
                        break;
                    };
                    let result = process(&state, &task_context, &entry).await;
                    if context.lease.check().is_err() || state.restore.shutdown.is_cancelled() {
                        break;
                    }
                    if task_context.lease.check().is_err() {
                        continue;
                    }
                    match result {
                        Ok(selected) => {
                            record.state = "ready".into();
                            record.selected = Some(selected);
                            record.error.clear();
                            record.next_retry = 0;
                        }
                        Err(error) => {
                            let retryable =
                                matches!(error, AppError::Telegram(_) | AppError::Io(_));
                            record.state = "failed".into();
                            record.error = error.public_parts().0;
                            record.next_retry = if retryable {
                                [300, 1800]
                                    .get(record.attempts as usize)
                                    .map(|wait| crate::db::now_ts() + wait)
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            record.attempts = record.attempts.saturating_add(1);
                        }
                    }
                    let _guard = super::super::INSTALL_LOCK.lock().await;
                    if task_context.lease.check().is_ok()
                        && current(&state, &task_context, &entry).await.is_ok()
                    {
                        let encoded = serde_json::to_string(&record).unwrap_or_default();
                        if let Err(error) = state
                            .vfs
                            .db
                            .cover_state_finish(
                                APP_ID,
                                &entry.media_guid,
                                &record.fingerprint,
                                &encoded,
                            )
                            .await
                        {
                            tracing::warn!(%error, "影视封面状态保存失败");
                        }
                    }
                    if state
                        .vfs
                        .db
                        .fntv
                        .cover_wake
                        .notified()
                        .now_or_never()
                        .is_some()
                    {
                        delay = Duration::ZERO;
                        break;
                    }
                }
            }
            tokio::select! {
                biased;
                _=lease.cancelled.cancelled()=>break,
                _=state.restore.shutdown.cancelled()=>break,
                _=state.vfs.db.fntv.cover_wake.notified()=>{},
                _=tokio::time::sleep(delay)=>{},
            }
        }
    });
}

async fn current(state: &AppState, context: &Context, entry: &Entry) -> AppResult<()> {
    state.vfs.db.require_background_enabled()?;
    if state.restore.shutdown.is_cancelled() {
        return Err(AppError::AppChanged);
    }
    authorized(state, context, entry.node_id, &entry.version).await?;
    let latest: Entry = data::get(&state.vfs.db, &format!("fntv:index:{}", entry.node_id))
        .await?
        .ok_or(AppError::NotFound)?;
    let latest = inputs(state, vec![latest])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    if fingerprint(&latest) != fingerprint(entry) {
        return Err(AppError::AppChanged);
    }
    context.lease.check()
}

async fn selected_current(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    selected: &Selected,
) -> AppResult<()> {
    current(state, context, entry).await?;
    if let Some(source) = &selected.source {
        let (video, _) = authorized(state, context, entry.node_id, &entry.version).await?;
        let (image, _) = authorized(state, context, source.id, &source.version).await?;
        if image.parent_id != video.parent_id {
            return Err(AppError::NotFound);
        }
    }
    context.lease.check()
}

async fn cached(state: &AppState, key: &str) -> AppResult<Option<(String, Vec<u8>)>> {
    Ok(state
        .vfs
        .db
        .cover_get_many(APP_ID, &[key.into()])
        .await?
        .pop()
        .filter(|r| {
            !r.data.is_empty()
                && r.data.len() <= POSTER_LIMIT
                && crate::api::downloaded_thumbnail_mime(&r.data) == Some(r.mime.as_str())
        })
        .map(|r| (r.mime, r.data)))
}

/// 图片解码器同时限制尺寸和内存，原始海报或 Telegram 小图不重新压缩。
pub(super) async fn validate_image(bytes: Vec<u8>) -> AppResult<(String, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let mime = crate::api::downloaded_thumbnail_mime(&bytes)
            .ok_or_else(|| AppError::BadRequest("封面不是有效的 JPEG、PNG 或 WebP 图片".into()))?
            .to_owned();
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|_| AppError::BadRequest("封面图片无法识别".into()))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        reader
            .decode()
            .map_err(|_| AppError::BadRequest("封面图片损坏或超出解码限额".into()))?;
        Ok((mime, bytes))
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?
}

async fn sidecar(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    source: &Source,
) -> AppResult<Vec<u8>> {
    let (video, _) = authorized(state, context, entry.node_id, &entry.version).await?;
    let (image, _) = authorized(state, context, source.id, &source.version).await?;
    if image.parent_id != video.parent_id || image.size <= 0 || image.size as usize > POSTER_LIMIT {
        return Err(AppError::BadRequest("同目录海报来源无效".into()));
    }
    let body = super::protocol::byte_stream(state, context, &image, 0, image.size as u64).await?;
    to_bytes(body, POSTER_LIMIT)
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|_| AppError::Telegram("同目录海报读取暂时失败".into()))
}

async fn thumbnail(state: &AppState, entry: &Entry) -> AppResult<Vec<u8>> {
    let first = state
        .vfs
        .db
        .first_chunk(entry.node_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if first.tg_thumbnail_file_id.as_deref() != entry.thumbnail.as_ref().map(|t| t.file.as_str()) {
        return Err(AppError::AppChanged);
    }
    let (_, bytes) = crate::api::thumbnail_bytes(state, &first).await?;
    Ok(bytes)
}

async fn store(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    selected: &Selected,
    image: (String, Vec<u8>),
) -> AppResult<()> {
    let _guard = super::super::INSTALL_LOCK.lock().await;
    current(state, context, entry).await?;
    if let Some(source) = &selected.source {
        let (video, _) = authorized(state, context, entry.node_id, &entry.version).await?;
        let (node, _) = authorized(state, context, source.id, &source.version).await?;
        if node.parent_id != video.parent_id {
            return Err(AppError::AppChanged);
        }
    }
    state
        .vfs
        .db
        .cover_put(
            APP_ID,
            &selected.key,
            &image.0,
            &image.1,
            Some(&json!({"media_guid":entry.media_guid,"kind":selected.kind}).to_string()),
            CoverLimits::default(),
        )
        .await?;
    context.lease.check()
}

async fn process(state: &AppState, context: &Context, entry: &Entry) -> AppResult<Selected> {
    current(state, context, entry).await?;
    let mut transient = None;
    for source in candidates(entry) {
        let selected = Selected {
            key: guid(&["fntv-sidecar-1", &entry.media_guid, &source.guid("poster")]),
            kind: "sidecar".into(),
            source: Some(source.clone()),
        };
        let fetch = async {
            if let Some(image) = cached(state, &selected.key).await? {
                return Ok(image);
            }
            validate_image(sidecar(state, context, entry, &source).await?).await
        };
        match bounded(context, fetch).await {
            Ok(image) => {
                store(state, context, entry, &selected, image).await?;
                return Ok(selected);
            }
            Err(error) if context.lease.check().is_err() => return Err(error),
            Err(error @ (AppError::Telegram(_) | AppError::Io(_))) => transient = Some(error),
            Err(_) => {}
        }
    }
    if entry.thumbnail.is_some() {
        let selected = Selected {
            key: guid(&["fntv-telegram-1", &entry.media_guid, &fingerprint(entry)]),
            kind: "telegram".into(),
            source: None,
        };
        let fetch = async {
            if let Some(image) = cached(state, &selected.key).await? {
                return Ok(image);
            }
            let bytes = thumbnail(state, entry).await?;
            if bytes.len() > THUMBNAIL_LIMIT {
                return Err(AppError::BadRequest("Telegram 缩略图超过 512 KiB".into()));
            }
            validate_image(bytes).await
        };
        match bounded(context, fetch).await {
            Ok(image) => {
                store(state, context, entry, &selected, image).await?;
                return Ok(selected);
            }
            Err(error) if context.lease.check().is_err() => return Err(error),
            Err(error @ (AppError::Telegram(_) | AppError::Io(_))) => transient = Some(error),
            Err(_) => {}
        }
    }
    let selected = Selected {
        key: guid(&["fntv-frame-1", &entry.media_guid]),
        kind: "frame".into(),
        source: None,
    };
    let image = if let Some(image) = cached(state, &selected.key).await? {
        image
    } else {
        match frame::extract(state, context, entry).await {
            Ok(image) => ("image/jpeg".into(), image),
            Err(error) => return Err(transient.unwrap_or(error)),
        }
    };
    store(state, context, entry, &selected, image).await?;
    Ok(selected)
}

async fn bounded<T>(
    context: &Context,
    future: impl std::future::Future<Output = AppResult<T>>,
) -> AppResult<T> {
    tokio::select! { biased; _=context.lease.cancelled.cancelled()=>Err(AppError::AppChanged),
    result=tokio::time::timeout(Duration::from_secs(10), future)=>result.unwrap_or_else(|_| Err(AppError::Telegram("图片读取超时，稍后重试".into()))) }
}

pub(super) async fn response(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    reference: &str,
    headers: &HeaderMap,
    method: Method,
) -> AppResult<Response> {
    current(state, context, entry).await?;
    let record = records(state)
        .await?
        .remove(&entry.media_guid)
        .filter(|r| r.state == "ready" && r.fingerprint == fingerprint(entry))
        .ok_or(AppError::NotFound)?;
    let selected = record
        .selected
        .filter(|s| s.key == reference)
        .ok_or(AppError::NotFound)?;
    if let Some(source) = &selected.source {
        let (video, _) = authorized(state, context, entry.node_id, &entry.version).await?;
        let (node, _) = authorized(state, context, source.id, &source.version).await?;
        if node.parent_id != video.parent_id {
            return Err(AppError::NotFound);
        }
    }
    let Some((mime, bytes)) = cached(state, &selected.key).await? else {
        let mut queued = Record::queued(entry);
        queued.attempts = record.attempts;
        let _guard = super::super::INSTALL_LOCK.lock().await;
        current(state, context, entry).await?;
        state
            .vfs
            .db
            .cover_state_finish(
                APP_ID,
                &entry.media_guid,
                &queued.fingerprint,
                &serde_json::to_string(&queued).map_err(|e| AppError::Internal(e.into()))?,
            )
            .await?;
        state.vfs.db.fntv.cover_wake.notify_one();
        return Err(AppError::NotFound);
    };
    use sha2::{Digest, Sha256};
    let etag = format!(
        "\"fntv-image-{}\"",
        crate::engine::upload::to_hex(&Sha256::digest(&bytes))
    );
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(',')
                .any(|h| h.trim() == "*" || h.trim().trim_start_matches("W/") == etag)
        })
    {
        selected_current(state, context, entry, &selected).await?;
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap());
    }
    let size = bytes.len() as u64;
    let range = if headers
        .get(header::IF_RANGE)
        .is_some_and(|h| h.to_str().ok() != Some(etag.as_str()))
    {
        Ok(None)
    } else {
        crate::api::parse_range(headers, size)
    };
    let range = match range {
        Ok(range) => range,
        Err(()) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::ETAG, etag)
                .body(Body::empty())
                .unwrap());
        }
    };
    let (start, end) = range.unwrap_or((0, size - 1));
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::ETAG, etag)
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::ACCEPT_RANGES, "bytes")
        .header("x-content-type-options", "nosniff");
    if range.is_some() {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    selected_current(state, context, entry, &selected).await?;
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let state = state.clone();
        let context = context.clone();
        let entry = entry.clone();
        let selected = selected.clone();
        Body::from_stream(futures_util::stream::try_unfold(
            (bytes, start as usize, end as usize + 1),
            move |(bytes, pos, end)| {
                let state = state.clone();
                let context = context.clone();
                let entry = entry.clone();
                let selected = selected.clone();
                async move {
                    if pos >= end {
                        return Ok::<_, std::io::Error>(None);
                    }
                    selected_current(&state, &context, &entry, &selected)
                        .await
                        .map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "影视封面授权已失效",
                            )
                        })?;
                    let next = (pos + 64 * 1024).min(end);
                    Ok(Some((
                        bytes::Bytes::copy_from_slice(&bytes[pos..next]),
                        (bytes, next, end),
                    )))
                }
            },
        ))
    };
    Ok(response.body(body).unwrap())
}
