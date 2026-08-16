//! Bot 收件:getUpdates 长轮询,把发给 bot 的文件自动登记进网盘。
//!
//! 两条入口:
//! - **存储频道直发**:文件本来就落在存储频道,直接把 file_id/message_id 登记成
//!   单块文件,零流量入库(bot 自己发的消息不会出现在 getUpdates 里,不会自吞分块)。
//! - **私聊白名单**:把文件、视频、图片、音频等按原媒体类型用 file_id 重发到
//!   存储频道(Telegram 侧复制,同样零流量),再登记;完成后回复存放路径。
//!
//! 官方 Bot API 下载单文件上限 20MB:超限的文件照样入库,下载时才会受限(自建端点无此限制)。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::settings::{Runtime, RuntimeCfg};
use crate::vfs::Vfs;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

const POLL_TIMEOUT_SECS: u64 = 25;
const OFFSET_KEY: &str = "extra.inbox_offset";
const FAILURE_KEY_PREFIX: &str = "extra.inbox_failure.";
const MAX_UPDATE_ATTEMPTS: i64 = 5;
/// 见过的会话列表(JSON),供后台「检测频道/用户 ID」使用
const SEEN_CHATS_KEY: &str = "extra.seen_chats";
const SEEN_CHATS_MAX: usize = 30;
/// getUpdates 一次订阅这些类型;my_chat_member 用于捕捉「bot 被拉进频道/群」
const ALLOWED_UPDATES: [&str; 3] = ["message", "channel_post", "my_chat_member"];
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "m4v", "avi", "webm", "flv", "wmv", "ts", "m2ts", "mpeg", "mpg", "3gp",
    "ogv",
];
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "avif", "heic", "heif", "tiff", "tif", "svg", "ico",
];
const BOOK_EXTENSIONS: &[&str] = &[
    "pdf", "epub", "mobi", "azw", "azw3", "fb2", "djvu", "cbz", "cbr", "txt", "rtf",
];

#[derive(Deserialize)]
struct ApiResp<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Update {
    update_id: i64,
    message: Option<Msg>,
    channel_post: Option<Msg>,
    my_chat_member: Option<ChatMemberUpd>,
}

/// bot 被拉进/移出频道、群时的成员变动事件,只关心是哪个会话
#[derive(Deserialize, Serialize)]
struct ChatMemberUpd {
    chat: Chat,
}

#[derive(Deserialize, Serialize)]
struct Msg {
    message_id: i64,
    chat: Chat,
    from: Option<User>,
    caption: Option<String>,
    document: Option<FileRef>,
    video: Option<FileRef>,
    audio: Option<FileRef>,
    animation: Option<FileRef>,
    photo: Option<Vec<PhotoSize>>,
}

#[derive(Deserialize, Serialize)]
struct Chat {
    id: i64,
    username: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct User {
    id: i64,
    username: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    is_bot: Option<bool>,
}

#[derive(Default, Deserialize, Serialize)]
struct FileRef {
    file_id: String,
    file_size: Option<i64>,
    file_name: Option<String>,
    thumbnail: Option<ThumbnailRef>,
    /// Telegram 9.x 会把视频的其他清晰度放在这里；主 file_id 不一定是最高画质。
    #[serde(default)]
    qualities: Vec<VideoQualityRef>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize, Serialize)]
struct VideoQualityRef {
    file_id: String,
    file_size: Option<i64>,
    width: u32,
    height: u32,
}

#[derive(Deserialize, Serialize)]
struct ThumbnailRef {
    file_id: String,
}

#[derive(Deserialize, Serialize)]
struct PhotoSize {
    file_id: String,
    file_size: Option<i64>,
}

pub async fn run(vfs: Vfs, rt: Arc<Runtime>, task: crate::tasks::TaskReporter) {
    // 长轮询本身要 25s,请求超时给足余量
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 15))
        .build()
        .expect("构造 HTTP 客户端失败");
    let mut backoff = 5u64;
    loop {
        if task.is_cancelled() {
            return;
        }
        task.heartbeat();
        let cfg = rt.current();
        let ex = match crate::extras::load(&vfs.db).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("收件:读配置失败: {e}");
                task.report_error(&e.to_string());
                if !task.sleep(Duration::from_secs(30)).await {
                    return;
                }
                continue;
            }
        };
        if !ex.inbox_enabled || !cfg.is_configured() {
            task.clear_error();
            if !task.sleep(Duration::from_secs(15)).await {
                return;
            }
            continue;
        }
        match poll_once(&http, &vfs, &cfg, &ex).await {
            Ok(()) => {
                backoff = 5;
                task.clear_error();
            }
            Err(e) => {
                task.report_error(&e.to_string());
                tracing::warn!("收件轮询失败({e}),{backoff}s 后重试");
                if !task.sleep(Duration::from_secs(backoff)).await {
                    return;
                }
                backoff = (backoff * 2).min(300);
            }
        }
    }
}

async fn poll_once(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    ex: &crate::extras::Extras,
) -> AppResult<()> {
    let offset: i64 = vfs
        .db
        .setting_get(OFFSET_KEY)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let url = format!("{}/bot{}/getUpdates", cfg.api_base, cfg.bot_token);
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "offset": offset,
            "timeout": POLL_TIMEOUT_SECS,
            "allowed_updates": ALLOWED_UPDATES,
        }))
        .send()
        .await
        .map_err(|e| {
            AppError::Telegram(format!(
                "getUpdates 请求失败: {}",
                crate::telegram::reqwest_error_summary(&e)
            ))
        })?;
    let parsed: ApiResp<Vec<Update>> = resp.json().await.map_err(|e| {
        AppError::Telegram(format!(
            "getUpdates 响应解析失败: {}",
            crate::telegram::reqwest_error_summary(&e)
        ))
    })?;
    if !parsed.ok {
        return Err(AppError::Telegram(format!(
            "getUpdates: {}",
            parsed
                .description
                .unwrap_or_else(|| "未知错误".into())
                .replace(&cfg.bot_token, "[已脱敏]")
        )));
    }

    let updates = parsed.result.unwrap_or_default();
    // 顺手记下消息来源,供后台「检测频道/用户 ID」展示
    // (收件轮询会把 offset 推进、消息被"收走",不记的话检测接口就看不到了)
    let found: Vec<SeenChat> = updates.iter().flat_map(chats_from_update).collect();
    if let Err(e) = remember_chats(&vfs.db, found).await {
        tracing::warn!("收件:记录会话来源失败: {e}");
    }
    for up in &updates {
        match handle_update(http, vfs, cfg, ex, up).await {
            Ok(()) => {
                // 每条成功后立即推进，后续消息失败或进程退出都不会让已完成消息重复转存。
                vfs.db
                    .setting_set(OFFSET_KEY, &(up.update_id + 1).to_string())
                    .await?;
                let _ = vfs
                    .db
                    .setting_del(&format!("{FAILURE_KEY_PREFIX}{}", up.update_id))
                    .await;
            }
            Err(e) => {
                let attempts = remember_update_failure(&vfs.db, up, &e.to_string()).await?;
                if attempts < MAX_UPDATE_ATTEMPTS {
                    tracing::warn!(
                        "收件:处理 update {} 失败(第 {attempts}/{MAX_UPDATE_ATTEMPTS} 次): {e}",
                        up.update_id
                    );
                    return Err(e);
                }
                // 持续失败的消息保留完整载荷供人工处理，避免永久阻塞后续收件。
                tracing::error!(
                    "收件:update {} 连续失败 {attempts} 次，已进入死信并继续处理后续消息: {e}",
                    up.update_id
                );
                vfs.db
                    .setting_set(OFFSET_KEY, &(up.update_id + 1).to_string())
                    .await?;
            }
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct InboxFailure {
    attempts: i64,
    last_error: String,
    failed_at: i64,
    update: serde_json::Value,
}

async fn remember_update_failure(db: &Db, update: &Update, error: &str) -> AppResult<i64> {
    let key = format!("{FAILURE_KEY_PREFIX}{}", update.update_id);
    let previous = db
        .setting_get(&key)
        .await?
        .and_then(|raw| serde_json::from_str::<InboxFailure>(&raw).ok());
    let attempts = previous.map(|failure| failure.attempts).unwrap_or(0) + 1;
    let failure = InboxFailure {
        attempts,
        last_error: error.chars().take(500).collect(),
        failed_at: crate::db::now_ts(),
        update: serde_json::to_value(update)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("序列化收件死信失败: {e}")))?,
    };
    db.setting_set(
        &key,
        &serde_json::to_string(&failure)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("保存收件死信失败: {e}")))?,
    )
    .await?;
    Ok(attempts)
}

async fn handle_update(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    ex: &crate::extras::Extras,
    up: &Update,
) -> AppResult<()> {
    // 存储频道里的直发文件:零流量登记
    if let Some(post) = &up.channel_post {
        if is_storage_chat(&cfg.chat_id, &post.chat) && !is_own_chunk(post) {
            if let Some(file) = pick_file(post) {
                let path = ingest(
                    vfs,
                    ex,
                    &file.name,
                    file.size,
                    &file.file_id,
                    file.thumbnail_file_id.as_deref(),
                    post.message_id,
                    cfg.storage_target_id.ok_or_else(|| {
                        AppError::BadRequest("当前 Telegram 配置缺少存储目标".into())
                    })?,
                )
                .await?;
                tracing::info!("收件:频道文件已入库 {path}");
            } else if has_supported_media(post) {
                tracing::warn!("收件:频道消息 {} 缺少有效文件大小，已跳过", post.message_id);
            }
        }
        return Ok(());
    }

    // 私聊:白名单内的媒体转存到频道再登记
    let Some(msg) = &up.message else {
        return Ok(());
    };
    if msg.chat.kind != "private" {
        return Ok(());
    }
    let allowed: Vec<i64> = ex
        .inbox_allowed_chats
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if !allowed.contains(&msg.chat.id) {
        return Ok(()); // 陌生人消息静默忽略,不暴露 bot 用途
    }

    if let Some(file) = pick_file(msg) {
        let (mid, fid, size, thumbnail_file_id) =
            match resend_file(http, cfg, file.kind, &file.file_id).await {
                Ok(sent) => sent,
                Err(e) => {
                    reply(http, cfg, msg, "❌ 转存失败，请稍后重新发送").await;
                    return Err(e);
                }
            };
        let size = size.filter(|n| *n > 0).unwrap_or(file.size);
        let storage_target_id = cfg
            .storage_target_id
            .ok_or_else(|| AppError::BadRequest("当前 Telegram 配置缺少存储目标".into()))?;
        let path = match ingest(
            vfs,
            ex,
            &file.name,
            size,
            &fid,
            thumbnail_file_id.as_deref(),
            mid,
            storage_target_id,
        )
        .await
        {
            Ok(path) => path,
            Err(e) => {
                // 私聊文件已经转发成功但落库失败时补偿删除，不能在频道留下孤儿消息。
                let cleanup_result = match vfs.tg.binding(storage_target_id) {
                    Ok(binding) => binding.api.delete_message(mid).await,
                    Err(binding_error) => Err(binding_error),
                };
                if let Err(cleanup) = cleanup_result {
                    tracing::error!("收件消息 {mid} 落库失败({e})，且清理频道消息失败:{cleanup}");
                }
                reply(http, cfg, msg, "❌ 文件登记失败，请稍后重新发送").await;
                return Err(e);
            }
        };
        reply(http, cfg, msg, &format!("✅ 已存入 {path}")).await;
    } else if has_supported_media(msg) {
        reply(
            http,
            cfg,
            msg,
            "❌ Telegram 没有返回有效的文件大小，无法登记",
        )
        .await;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Document,
    Video,
    Audio,
    Animation,
    Photo,
}

impl MediaKind {
    fn method(self) -> &'static str {
        match self {
            Self::Document => "sendDocument",
            Self::Video => "sendVideo",
            Self::Audio => "sendAudio",
            Self::Animation => "sendAnimation",
            Self::Photo => "sendPhoto",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Animation => "animation",
            Self::Photo => "photo",
        }
    }
}

struct PickedFile {
    kind: MediaKind,
    name: String,
    size: i64,
    file_id: String,
    thumbnail_file_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SelectedVideoRef {
    file_id: String,
    size: i64,
    width: u32,
    height: u32,
    is_primary: bool,
}

fn positive_size(size: Option<i64>) -> Option<i64> {
    size.filter(|n| *n > 0)
}

/// Telegram 的 `video.file_id` 可能只对应默认 360p，720p/1080p 位于
/// `video.qualities`。只在文件大小有效时参与选择，保证登记的分块长度可用于 Range。
fn select_best_video(video: &FileRef) -> Option<SelectedVideoRef> {
    let primary = positive_size(video.file_size).and_then(|size| {
        (!video.file_id.trim().is_empty()).then(|| SelectedVideoRef {
            file_id: video.file_id.clone(),
            size,
            width: video.width.unwrap_or(0),
            height: video.height.unwrap_or(0),
            is_primary: true,
        })
    });
    let alternatives = video.qualities.iter().filter_map(|quality| {
        let size = positive_size(quality.file_size)?;
        (!quality.file_id.trim().is_empty()).then(|| SelectedVideoRef {
            file_id: quality.file_id.clone(),
            size,
            width: quality.width,
            height: quality.height,
            is_primary: false,
        })
    });
    primary.into_iter().chain(alternatives).max_by_key(|item| {
        (
            u64::from(item.width).saturating_mul(u64::from(item.height)),
            item.is_primary,
            item.size,
        )
    })
}

fn has_supported_media(m: &Msg) -> bool {
    m.document.is_some()
        || m.video.is_some()
        || m.audio.is_some()
        || m.animation.is_some()
        || m.photo.is_some()
}

/// 消息里挑出可入库的媒体，缺少大小时不登记，避免产生不可正确下载的节点。
fn pick_file(m: &Msg) -> Option<PickedFile> {
    // Telegram 为兼容旧客户端，动画消息可能同时带 animation 和 document；
    // 必须先判断 animation，才能按原媒体类型转存。
    if let Some(a) = &m.animation {
        let name = a
            .file_name
            .clone()
            .unwrap_or_else(|| format!("animation_{}.gif", m.message_id));
        return Some(PickedFile {
            kind: MediaKind::Animation,
            name,
            size: positive_size(a.file_size)?,
            file_id: a.file_id.clone(),
            thumbnail_file_id: a.thumbnail.as_ref().map(|thumb| thumb.file_id.clone()),
        });
    }
    if let Some(d) = &m.document {
        let name = d
            .file_name
            .clone()
            .unwrap_or_else(|| format!("file_{}", m.message_id));
        return Some(PickedFile {
            kind: MediaKind::Document,
            name,
            size: positive_size(d.file_size)?,
            file_id: d.file_id.clone(),
            thumbnail_file_id: d.thumbnail.as_ref().map(|thumb| thumb.file_id.clone()),
        });
    }
    if let Some(v) = &m.video {
        let selected = select_best_video(v)?;
        let name = v
            .file_name
            .clone()
            .unwrap_or_else(|| format!("video_{}.mp4", m.message_id));
        return Some(PickedFile {
            kind: MediaKind::Video,
            name,
            size: selected.size,
            file_id: selected.file_id,
            thumbnail_file_id: v.thumbnail.as_ref().map(|thumb| thumb.file_id.clone()),
        });
    }
    if let Some(a) = &m.audio {
        let name = a
            .file_name
            .clone()
            .unwrap_or_else(|| format!("audio_{}.mp3", m.message_id));
        return Some(PickedFile {
            kind: MediaKind::Audio,
            name,
            size: positive_size(a.file_size)?,
            file_id: a.file_id.clone(),
            thumbnail_file_id: a.thumbnail.as_ref().map(|thumb| thumb.file_id.clone()),
        });
    }
    if let Some(sizes) = &m.photo {
        // photo 数组末尾是最大分辨率
        let best = sizes.last()?;
        return Some(PickedFile {
            kind: MediaKind::Photo,
            name: format!("photo_{}.jpg", m.message_id),
            size: positive_size(best.file_size)?,
            file_id: best.file_id.clone(),
            thumbnail_file_id: None,
        });
    }
    None
}

/// tgdrive 自己传的分块 caption 以 "tgdrive {" 开头;正常情况 getUpdates
/// 收不到 bot 自己的消息,这里只是多一道保险
fn is_own_chunk(m: &Msg) -> bool {
    m.caption
        .as_deref()
        .is_some_and(|c| c.starts_with("tgdrive {"))
}

fn is_storage_chat(cfg_chat: &str, chat: &Chat) -> bool {
    if let Ok(id) = cfg_chat.trim().parse::<i64>() {
        return chat.id == id;
    }
    cfg_chat
        .strip_prefix('@')
        .is_some_and(|n| chat.username.as_deref() == Some(n))
}

fn has_extension(lower_name: &str, extension: &str) -> bool {
    lower_name
        .strip_suffix(extension)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

/// 自定义规则优先，重叠时更长的后缀优先；其次是视频/图片/书籍，最后回落到默认目录。
fn inbox_dir_for<'a>(ex: &'a crate::extras::Extras, name: &str) -> &'a str {
    let lower_name = name.to_lowercase();
    let mut custom_match: Option<(&str, usize)> = None;
    for rule in &ex.inbox_rules {
        for ext in &rule.extensions {
            if has_extension(&lower_name, ext)
                && custom_match.is_none_or(|(_, matched_len)| ext.len() > matched_len)
            {
                custom_match = Some((&rule.dir, ext.len()));
            }
        }
    }
    if let Some((dir, _)) = custom_match {
        return dir;
    }
    if VIDEO_EXTENSIONS
        .iter()
        .any(|ext| has_extension(&lower_name, ext))
    {
        &ex.inbox_video_dir
    } else if IMAGE_EXTENSIONS
        .iter()
        .any(|ext| has_extension(&lower_name, ext))
    {
        &ex.inbox_image_dir
    } else if BOOK_EXTENSIONS
        .iter()
        .any(|ext| has_extension(&lower_name, ext))
    {
        &ex.inbox_book_dir
    } else {
        &ex.inbox_dir
    }
}

/// 把频道里的一条文件消息登记为网盘单块文件(同名覆盖,与上传语义一致)
#[allow(clippy::too_many_arguments)]
async fn ingest(
    vfs: &Vfs,
    ex: &crate::extras::Extras,
    raw_name: &str,
    size: i64,
    file_id: &str,
    thumbnail_file_id: Option<&str>,
    message_id: i64,
    storage_target_id: i64,
) -> AppResult<String> {
    // 文件名可能含路径分隔符等,压成单段安全名
    let name: String = raw_name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = if name.trim().is_empty() {
        format!("file_{message_id}")
    } else {
        name
    };

    let target_dir = inbox_dir_for(ex, &name);
    let dir = vfs.mkdir_p(target_dir).await?;
    let node_id = vfs.db.create_uploading(dir.id, &name).await?;
    let result = async {
        vfs.db
            .add_chunk(
                node_id,
                0,
                size,
                file_id,
                message_id,
                storage_target_id,
                thumbnail_file_id,
            )
            .await?;
        vfs.db.finalize_upload(node_id, size, None).await
    }
    .await;
    let freed = match result {
        Ok(freed) => freed,
        Err(e) => {
            // 不能留下 uploading 占位，否则同名文件之后会一直被判定为正在上传。
            // 返回的消息 ID 由调用方决定是否删除：频道直发必须保留原消息，私聊转存会补偿删除。
            if let Err(cleanup) = vfs.db.abort_upload_keep_messages(node_id).await {
                tracing::error!("收件节点 {node_id} 登记失败({e})，且回滚临时记录失败:{cleanup}");
            }
            return Err(e);
        }
    };
    vfs.uploader.delete_messages_best_effort(freed);
    Ok(format!("{}/{}", target_dir.trim_end_matches('/'), name))
}

/// 按原媒体类型用 file_id 重发到存储频道(Telegram 侧复制,零流量)，
/// 返回 (message_id, 新 file_id, file_size, thumbnail_file_id)。
async fn resend_file(
    http: &reqwest::Client,
    cfg: &RuntimeCfg,
    kind: MediaKind,
    file_id: &str,
) -> AppResult<(i64, String, Option<i64>, Option<String>)> {
    #[derive(Deserialize)]
    struct Sent {
        message_id: i64,
        document: Option<FileRef>,
        video: Option<FileRef>,
        audio: Option<FileRef>,
        animation: Option<FileRef>,
        photo: Option<Vec<PhotoSize>>,
    }
    let url = format!("{}/bot{}/{}", cfg.api_base, cfg.bot_token, kind.method());
    let mut body = serde_json::Map::new();
    body.insert("chat_id".into(), serde_json::json!(cfg.chat_id));
    body.insert(kind.field().into(), serde_json::json!(file_id));
    body.insert("disable_notification".into(), serde_json::json!(true));
    let resp = http.post(&url).json(&body).send().await.map_err(|e| {
        AppError::Telegram(format!(
            "转存失败: {}",
            crate::telegram::reqwest_error_summary(&e)
        ))
    })?;
    let parsed: ApiResp<Sent> = resp.json().await.map_err(|e| {
        AppError::Telegram(format!(
            "转存响应解析失败: {}",
            crate::telegram::reqwest_error_summary(&e)
        ))
    })?;
    let sent = match (parsed.ok, parsed.result) {
        (true, Some(s)) => s,
        _ => {
            return Err(AppError::Telegram(format!(
                "转存被拒: {}",
                parsed
                    .description
                    .unwrap_or_else(|| "未知错误".into())
                    .replace(&cfg.bot_token, "[已脱敏]")
            )));
        }
    };
    let media = match kind {
        MediaKind::Document => sent.document.map(|f| {
            (
                f.file_id,
                f.file_size,
                f.thumbnail.map(|thumb| thumb.file_id),
            )
        }),
        MediaKind::Video => sent.video.and_then(|f| {
            let selected = select_best_video(&f)?;
            Some((
                selected.file_id,
                Some(selected.size),
                f.thumbnail.map(|thumb| thumb.file_id),
            ))
        }),
        MediaKind::Audio => sent.audio.map(|f| {
            (
                f.file_id,
                f.file_size,
                f.thumbnail.map(|thumb| thumb.file_id),
            )
        }),
        MediaKind::Animation => sent.animation.map(|f| {
            (
                f.file_id,
                f.file_size,
                f.thumbnail.map(|thumb| thumb.file_id),
            )
        }),
        MediaKind::Photo => sent
            .photo
            .and_then(|sizes| sizes.into_iter().last())
            .map(|f| (f.file_id, f.file_size, None)),
    }
    .ok_or_else(|| AppError::Telegram(format!("转存响应缺少 {}", kind.field())))?;
    Ok((sent.message_id, media.0, media.1, media.2))
}

/// 尽力回复私聊,失败只记日志
async fn reply(http: &reqwest::Client, cfg: &RuntimeCfg, to: &Msg, text: &str) {
    let url = format!("{}/bot{}/sendMessage", cfg.api_base, cfg.bot_token);
    let r = http
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": to.chat.id,
            "text": text,
            "reply_to_message_id": to.message_id,
        }))
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(
            "收件:回复失败: {}",
            crate::telegram::reqwest_error_summary(&e)
        );
    }
}

// ---------- 会话来源记录(后台「检测频道/用户 ID」) ----------

/// 一个向 bot 发过消息(或把 bot 拉进过频道/群)的会话
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeenChat {
    pub id: i64,
    /// channel / private / group / supergroup / user(群消息的发送者)
    pub kind: String,
    pub name: String,
    pub username: Option<String>,
    /// 最近一次见到的时间戳
    pub at: i64,
}

fn display_name(
    title: &Option<String>,
    first: &Option<String>,
    last: &Option<String>,
    username: &Option<String>,
    id: i64,
) -> String {
    if let Some(t) = title.as_deref().filter(|s| !s.trim().is_empty()) {
        return t.to_string();
    }
    let person = [first.as_deref(), last.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if !person.trim().is_empty() {
        return person;
    }
    if let Some(u) = username.as_deref().filter(|s| !s.trim().is_empty()) {
        return format!("@{u}");
    }
    id.to_string()
}

/// 从一条 update 里提取所有可见会话
fn chats_from_update(up: &Update) -> Vec<SeenChat> {
    let now = crate::db::now_ts();
    let mut out = Vec::new();
    let mut push_chat = |c: &Chat| {
        out.push(SeenChat {
            id: c.id,
            kind: c.kind.clone(),
            name: display_name(&c.title, &c.first_name, &c.last_name, &c.username, c.id),
            username: c.username.clone(),
            at: now,
        });
    };
    if let Some(p) = &up.channel_post {
        push_chat(&p.chat);
    }
    if let Some(mc) = &up.my_chat_member {
        push_chat(&mc.chat);
    }
    if let Some(m) = &up.message {
        push_chat(&m.chat);
        // 群消息的发送者是另一个 id,也记下(可用于私聊白名单)
        if let Some(u) = &m.from
            && u.id != m.chat.id
            && !u.is_bot.unwrap_or(false)
        {
            out.push(SeenChat {
                id: u.id,
                kind: "user".into(),
                name: display_name(&None, &u.first_name, &u.last_name, &u.username, u.id),
                username: u.username.clone(),
                at: now,
            });
        }
    }
    out
}

/// 合并进库里的会话列表:按 id 去重,最新的排最前,只留最近若干条
pub async fn remember_chats(db: &Db, found: Vec<SeenChat>) -> AppResult<()> {
    if found.is_empty() {
        return Ok(());
    }
    let mut list = seen_chats(db).await?;
    for c in found {
        list.retain(|x| x.id != c.id);
        list.insert(0, c);
    }
    list.truncate(SEEN_CHATS_MAX);
    let json = serde_json::to_string(&list)
        .map_err(|e| AppError::Telegram(format!("序列化会话列表失败: {e}")))?;
    db.setting_set(SEEN_CHATS_KEY, &json).await
}

/// 库里记录过的会话(损坏时静默从空开始)
pub async fn seen_chats(db: &Db) -> AppResult<Vec<SeenChat>> {
    Ok(db
        .setting_get(SEEN_CHATS_KEY)
        .await?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// 用给定 token 快速拉一次待处理的 updates(不带 offset,不确认任何消息),
/// 提取消息来源。收件轮询开着时消息多半已被它收走(并已记进 seen_chats),
/// 此时这里拿到空结果甚至 409 冲突都正常,调用方应把错误当非致命处理。
pub async fn probe_updates(api_base: &str, token: &str) -> AppResult<Vec<SeenChat>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| AppError::Telegram("构造 HTTP 客户端失败".into()))?;
    let url = format!("{}/bot{}/getUpdates", api_base.trim_end_matches('/'), token);
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "timeout": 0,
            "allowed_updates": ALLOWED_UPDATES,
        }))
        .send()
        .await
        .map_err(|e| {
            AppError::Telegram(format!(
                "getUpdates 请求失败: {}",
                crate::telegram::reqwest_error_summary(&e)
            ))
        })?;
    let parsed: ApiResp<Vec<Update>> = resp.json().await.map_err(|e| {
        AppError::Telegram(format!(
            "getUpdates 响应解析失败: {}",
            crate::telegram::reqwest_error_summary(&e)
        ))
    })?;
    if !parsed.ok {
        return Err(AppError::Telegram(format!(
            "getUpdates: {}",
            parsed
                .description
                .unwrap_or_else(|| "未知错误".into())
                .replace(token, "[已脱敏]")
        )));
    }
    Ok(parsed
        .result
        .unwrap_or_default()
        .iter()
        .flat_map(chats_from_update)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::{Extras, InboxRouteRule};

    fn extras() -> Extras {
        Extras {
            trash_retention_days: 30,
            backup_auto: false,
            backup_chunk_size: crate::extras::DEFAULT_BACKUP_CHUNK_SIZE,
            inbox_enabled: true,
            inbox_allowed_chats: String::new(),
            inbox_dir: "/收件箱".into(),
            inbox_video_dir: "/媒体/视频".into(),
            inbox_image_dir: "/媒体/图片".into(),
            inbox_book_dir: "/阅读".into(),
            inbox_rules: vec![
                InboxRouteRule {
                    name: "Gzip".into(),
                    extensions: vec!["gz".into()],
                    dir: "/压缩".into(),
                },
                InboxRouteRule {
                    name: "压缩包".into(),
                    extensions: vec!["zip".into(), "tar.gz".into(), "mp4".into()],
                    dir: "/归档".into(),
                },
            ],
        }
    }

    #[test]
    fn inbox_routes_by_extension_case_insensitively() {
        let ex = extras();
        assert_eq!(inbox_dir_for(&ex, "Movie.MKV"), "/媒体/视频");
        assert_eq!(inbox_dir_for(&ex, "封面.JpEg"), "/媒体/图片");
        assert_eq!(inbox_dir_for(&ex, "小说.EPUB"), "/阅读");
        assert_eq!(inbox_dir_for(&ex, "没有后缀"), "/收件箱");
    }

    #[test]
    fn custom_rule_supports_multi_part_suffix_and_overrides_builtin() {
        let ex = extras();
        assert_eq!(inbox_dir_for(&ex, "backup.TAR.GZ"), "/归档");
        assert_eq!(inbox_dir_for(&ex, "single.gz"), "/压缩");
        assert_eq!(inbox_dir_for(&ex, "special.mp4"), "/归档");
        assert_eq!(inbox_dir_for(&ex, "almost-tar.gz.txt"), "/阅读");
    }

    fn empty_msg() -> Msg {
        Msg {
            message_id: 7,
            chat: Chat {
                id: 1,
                username: None,
                kind: "private".into(),
                title: None,
                first_name: None,
                last_name: None,
            },
            from: None,
            caption: None,
            document: None,
            video: None,
            audio: None,
            animation: None,
            photo: None,
        }
    }

    #[test]
    fn private_video_and_photo_can_be_picked_for_zero_traffic_resend() {
        let mut video = empty_msg();
        video.video = Some(FileRef {
            file_id: "video-id".into(),
            file_size: Some(123),
            file_name: Some("clip.MP4".into()),
            thumbnail: Some(ThumbnailRef {
                file_id: "video-thumb".into(),
            }),
            ..Default::default()
        });
        let picked = pick_file(&video).unwrap();
        assert_eq!(picked.kind, MediaKind::Video);
        assert_eq!(picked.name, "clip.MP4");
        assert_eq!(picked.size, 123);
        assert_eq!(picked.thumbnail_file_id.as_deref(), Some("video-thumb"));

        let mut photo = empty_msg();
        photo.photo = Some(vec![
            PhotoSize {
                file_id: "small".into(),
                file_size: Some(10),
            },
            PhotoSize {
                file_id: "large".into(),
                file_size: Some(20),
            },
        ]);
        let picked = pick_file(&photo).unwrap();
        assert_eq!(picked.kind, MediaKind::Photo);
        assert_eq!(picked.name, "photo_7.jpg");
        assert_eq!(picked.file_id, "large");

        // Bot API 为兼容旧客户端，动画消息可能同时包含 document。
        let mut animation = empty_msg();
        animation.document = Some(FileRef {
            file_id: "document-id".into(),
            file_size: Some(30),
            file_name: Some("fallback.gif".into()),
            thumbnail: None,
            ..Default::default()
        });
        animation.animation = Some(FileRef {
            file_id: "animation-id".into(),
            file_size: Some(20),
            file_name: Some("animated.gif".into()),
            thumbnail: None,
            ..Default::default()
        });
        let picked = pick_file(&animation).unwrap();
        assert_eq!(picked.kind, MediaKind::Animation);
        assert_eq!(picked.file_id, "animation-id");
    }

    #[test]
    fn forwarded_video_prefers_highest_quality_with_known_size() {
        let video: FileRef = serde_json::from_value(serde_json::json!({
            "file_id": "default-360p",
            "file_size": 100,
            "file_name": "movie.mp4",
            "width": 640,
            "height": 360,
            "thumbnail": { "file_id": "thumb" },
            "qualities": [
                {
                    "file_id": "h264-360p",
                    "file_size": 90,
                    "width": 640,
                    "height": 360,
                    "codec": "h264"
                },
                {
                    "file_id": "h264-720p",
                    "file_size": 400,
                    "width": 1280,
                    "height": 720,
                    "codec": "h264"
                },
                {
                    "file_id": "unknown-size-1080p",
                    "width": 1920,
                    "height": 1080,
                    "codec": "h265"
                }
            ]
        }))
        .unwrap();

        let selected = select_best_video(&video).unwrap();
        assert_eq!(selected.file_id, "h264-720p");
        assert_eq!(selected.size, 400);
        assert_eq!((selected.width, selected.height), (1280, 720));

        let mut message = empty_msg();
        message.video = Some(video);
        let picked = pick_file(&message).unwrap();
        assert_eq!(picked.file_id, "h264-720p");
        assert_eq!(picked.size, 400);
        assert_eq!(picked.thumbnail_file_id.as_deref(), Some("thumb"));
    }

    #[test]
    fn primary_video_wins_when_resolution_is_equal() {
        let video = FileRef {
            file_id: "primary-original".into(),
            file_size: Some(500),
            width: Some(1280),
            height: Some(720),
            qualities: vec![VideoQualityRef {
                file_id: "transcoded-720p".into(),
                file_size: Some(300),
                width: 1280,
                height: 720,
            }],
            ..Default::default()
        };
        assert_eq!(
            select_best_video(&video).unwrap().file_id,
            "primary-original"
        );
    }
}
