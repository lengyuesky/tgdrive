//! Bot 收件:getUpdates 长轮询,把发给 bot 的文件自动登记进网盘。
//!
//! 两条入口:
//! - **存储频道直发**:文件本来就落在存储频道,直接把 file_id/message_id 登记成
//!   单块文件，零流量入库；跳过本服务上传 Bot 的消息与已经入库的分块。
//! - **私聊白名单**:把文件、视频、图片、音频等按原媒体类型用 file_id 重发到
//!   存储频道(Telegram 侧复制,同样零流量),再登记;完成后回复存放路径。
//!
//! 两种入口独立启停，并分别记录来源。
//!
//! 官方 Bot API 下载单文件上限 20MB:超限的文件照样入库,下载时才会受限(自建端点无此限制)。

use crate::db::{Db, InboxSource};
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

pub async fn run(
    vfs: Vfs,
    rt: Arc<Runtime>,
    events: crate::events::EventBus,
    task: crate::tasks::TaskReporter,
) {
    let mut cached_http: Option<(crate::proxy::ProxyConfig, reqwest::Client)> = None;
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
        if (!ex.inbox_bot_enabled && !ex.inbox_channel_enabled) || !cfg.is_configured() {
            task.clear_error();
            if !task.sleep(Duration::from_secs(15)).await {
                return;
            }
            continue;
        }
        let result = async {
            if cached_http
                .as_ref()
                .is_none_or(|(proxy, _)| *proxy != cfg.proxy)
            {
                // 代理变化后重建连接池；正在进行的一轮长轮询不被打断。
                let http =
                    inbox_http_client(&cfg.proxy, Duration::from_secs(POLL_TIMEOUT_SECS + 15))?;
                cached_http = Some((cfg.proxy.clone(), http));
            }
            poll_once(&cached_http.as_ref().unwrap().1, &vfs, &cfg).await
        };
        let ingested = match result.await {
            Ok(ingested) => {
                backoff = 5;
                task.clear_error();
                ingested
            }
            Err(e) => {
                task.report_error(&e.to_string());
                tracing::warn!("收件轮询失败({e}),{backoff}s 后重试");
                if !task.sleep(Duration::from_secs(backoff)).await {
                    return;
                }
                backoff = (backoff * 2).min(300);
                0
            }
        };
        if ingested > 0 {
            crate::events::emit(&events, crate::events::HostEvent::files());
        }
    }
}

async fn poll_once(http: &reqwest::Client, vfs: &Vfs, cfg: &RuntimeCfg) -> AppResult<usize> {
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
    // 长轮询最多等待 25 秒，返回后重新读取设置，避免继续使用关闭开关前的配置。
    let mut ex = crate::extras::load(&vfs.db).await?;
    let mut ingested = 0usize;
    for up in &updates {
        // 一批私聊转存可能较慢，两个开关还要在每条消息开始前重新检查。
        (ex.inbox_bot_enabled, ex.inbox_channel_enabled) =
            crate::extras::load_inbox_switches(&vfs.db).await?;
        match handle_update(http, vfs, cfg, &ex, up).await {
            Ok(count) => {
                ingested += count;
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
    Ok(ingested)
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
) -> AppResult<usize> {
    // 存储频道里的直发文件:零流量登记
    if let Some(post) = &up.channel_post {
        if ex.inbox_channel_enabled && is_storage_chat(&cfg.chat_id, &post.chat) {
            if is_storage_bot(cfg, post)
                || vfs
                    .db
                    .has_stored_message_in_chat(&cfg.chat_id, post.chat.id, post.message_id)
                    .await?
            {
                return Ok(0);
            }
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
                    InboxSource::Channel,
                )
                .await?;
                tracing::info!("收件:频道文件已入库 {path}");
                return Ok(1);
            } else if has_supported_media(post) {
                tracing::warn!("收件:频道消息 {} 缺少有效文件大小，已跳过", post.message_id);
            }
        }
        return Ok(0);
    }

    // 私聊:白名单内的媒体转存到频道再登记
    let Some(msg) = &up.message else {
        return Ok(0);
    };
    if !ex.inbox_bot_enabled || msg.chat.kind != "private" {
        return Ok(0);
    }
    let allowed: Vec<i64> = ex
        .inbox_allowed_chats
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if !allowed.contains(&msg.chat.id) {
        return Ok(0); // 陌生人消息静默忽略,不暴露 bot 用途
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
            InboxSource::Bot,
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
        return Ok(1);
    } else if has_supported_media(msg) {
        reply(
            http,
            cfg,
            msg,
            "❌ Telegram 没有返回有效的文件大小，无法登记",
        )
        .await;
    }
    Ok(0)
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

fn is_storage_chat(cfg_chat: &str, chat: &Chat) -> bool {
    if let Ok(id) = cfg_chat.trim().parse::<i64>() {
        return chat.id == id;
    }
    cfg_chat
        .strip_prefix('@')
        .is_some_and(|n| chat.username.as_deref() == Some(n))
}

fn is_storage_bot(cfg: &RuntimeCfg, msg: &Msg) -> bool {
    msg.from.as_ref().is_some_and(|sender| {
        cfg.all_upload_bot_tokens().iter().any(|token| {
            token
                .split_once(':')
                .and_then(|(id, _)| id.parse::<i64>().ok())
                == Some(sender.id)
        })
    })
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

/// 收件目录里同名文件很常见(比如手机发来的 `image.jpg`),依次尝试的编号上限
const MAX_NAME_SUFFIX: u32 = 999;

/// `image.jpg` → `image (n).jpg`;无扩展名或隐藏文件(`.bashrc`)直接在末尾编号
fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// 在目录里为收件文件占一个不与已有文件、目录或上传中文件重名的上传占位。
async fn create_unique_uploading(vfs: &Vfs, dir_id: i64, name: &str) -> AppResult<(i64, String)> {
    for n in 0..=MAX_NAME_SUFFIX {
        let candidate = if n == 0 {
            name.to_string()
        } else {
            numbered_name(name, n)
        };
        if vfs.db.child(dir_id, &candidate).await?.is_some() {
            continue;
        }
        match vfs.db.create_uploading(dir_id, &candidate).await {
            Ok(node_id) => return Ok((node_id, candidate)),
            Err(AppError::AlreadyExists | AppError::IsDirectory) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(AppError::AlreadyExists)
}

/// 把频道里的一条文件消息登记为网盘单块文件;同名时自动编号,不覆盖已有文件
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
    source: InboxSource,
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
    let (node_id, name) = create_unique_uploading(vfs, dir.id, &name).await?;
    let result = async {
        vfs.db.record_inbox_source(node_id, source).await?;
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
pub async fn probe_updates(
    api_base: &str,
    token: &str,
    proxy: &crate::proxy::ProxyConfig,
) -> AppResult<Vec<SeenChat>> {
    let http = inbox_http_client(proxy, Duration::from_secs(10))?;
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

fn inbox_http_client(
    proxy: &crate::proxy::ProxyConfig,
    timeout: Duration,
) -> AppResult<reqwest::Client> {
    proxy
        .http_builder()?
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()
        .map_err(|_| AppError::Telegram("构造收件 HTTP 客户端失败".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::{Extras, InboxRouteRule};
    use axum::{
        Json, Router,
        extract::{Path, State},
        routing::post,
    };
    use serde_json::{Value, json};
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct MockApi {
        calls: Arc<Mutex<Vec<String>>>,
        updates: Arc<Mutex<Vec<Value>>>,
        disable_on_poll: Arc<Mutex<Option<Db>>>,
    }

    async fn mock_api(
        State(state): State<MockApi>,
        Path(path): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let method = path.rsplit('/').next().unwrap();
        state.calls.lock().unwrap().push(method.to_string());
        let result = match method {
            "getUpdates" => {
                let disable = state.disable_on_poll.lock().unwrap().clone();
                if let Some(db) = disable {
                    crate::extras::save(
                        &db,
                        &crate::extras::ExtrasPatch {
                            inbox_bot_enabled: Some(false),
                            inbox_channel_enabled: Some(false),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                }
                let offset = body["offset"].as_i64().unwrap_or(0);
                json!(
                    state
                        .updates
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|update| update["update_id"].as_i64().unwrap() >= offset)
                        .collect::<Vec<_>>()
                )
            }
            "sendDocument" => json!({
                "message_id": 99,
                "document": {"file_id": "copied-file", "file_size": 12}
            }),
            "sendMessage" => json!({"message_id": 100}),
            _ => panic!("意外的 Telegram 请求: {method}"),
        };
        Json(json!({"ok": true, "result": result}))
    }

    async fn test_vfs() -> (
        Vfs,
        RuntimeCfg,
        MockApi,
        sqlx::SqlitePool,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let mock = MockApi::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/{*path}", post(mock_api))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        let db = Db::open(path.to_str().unwrap()).await.unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let target = db
            .ensure_storage_target(&api_base, "100:main", "-1001")
            .await
            .unwrap();
        let target_id = target.id;
        let tg = Arc::new(
            crate::telegram::TgRegistry::new(
                &[target],
                Some(target_id),
                vec![target_id],
                None,
                crate::proxy::ProxyConfig::default(),
            )
            .unwrap(),
        );
        let knobs = Arc::new(crate::engine::upload::UploadKnobs::new(
            1024,
            dir.path().to_path_buf(),
            1,
            false,
        ));
        let uploader = crate::engine::upload::Uploader::new(tg.clone(), db.clone(), knobs, 1024);
        let cfg = RuntimeCfg {
            proxy: crate::proxy::ProxyConfig::default(),
            api_base,
            bot_token: "100:main".into(),
            upload_bot_tokens: vec!["200:extra".into()],
            chat_id: "-1001".into(),
            chunk_size: 1024,
            upload_concurrency: 1,
            download_concurrency: crate::config::DEFAULT_DOWNLOAD_CONCURRENCY,
            send_video: false,
            tmp_dir: dir.path().to_path_buf(),
            username: String::new(),
            password: String::new(),
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            storage_target_id: Some(target_id),
            upload_target_ids: vec![target_id],
        };
        (Vfs { db, tg, uploader }, cfg, mock, pool, server, dir)
    }

    fn private_update() -> Value {
        json!({"update_id": 31, "message": {
            "message_id": 8, "chat": {"id": 42, "type": "private"},
            "document": {"file_id": "private-file", "file_name": "private.pdf", "file_size": 12}
        }})
    }

    fn channel_update() -> Value {
        json!({"update_id": 32, "channel_post": {
            "message_id": 7, "chat": {"id": -1001, "type": "channel"},
            "document": {"file_id": "channel-file", "file_name": "channel.pdf", "file_size": 12}
        }})
    }

    #[tokio::test]
    async fn private_and_channel_switches_control_ingest_and_resend_independently() {
        for (bot, channel) in [(false, false), (true, false), (false, true), (true, true)] {
            let (vfs, cfg, mock, pool, server, _dir) = test_vfs().await;
            let mut ex = extras();
            ex.inbox_bot_enabled = bot;
            ex.inbox_channel_enabled = channel;
            ex.inbox_allowed_chats = "42".into();
            for raw in [private_update(), channel_update()] {
                handle_update(
                    &reqwest::Client::new(),
                    &vfs,
                    &cfg,
                    &ex,
                    &serde_json::from_value(raw).unwrap(),
                )
                .await
                .unwrap();
            }
            assert_eq!(vfs.resolve("/阅读/private.pdf").await.is_ok(), bot);
            assert_eq!(vfs.resolve("/阅读/channel.pdf").await.is_ok(), channel);
            let sources: Vec<(String, String)> = sqlx::query_as(
                "SELECT n.name,s.source FROM inbox_sources s JOIN nodes n ON n.id=s.node_id ORDER BY n.name",
            ).fetch_all(&pool).await.unwrap();
            let mut expected = Vec::new();
            if channel {
                expected.push(("channel.pdf".into(), "channel".into()));
            }
            if bot {
                expected.push(("private.pdf".into(), "bot".into()));
            }
            assert_eq!(sources, expected);
            let calls = mock.calls.lock().unwrap().clone();
            assert_eq!(
                calls.iter().filter(|call| *call == "sendDocument").count(),
                usize::from(bot)
            );
            assert_eq!(
                calls.iter().filter(|call| *call == "sendMessage").count(),
                usize::from(bot)
            );
            server.abort();
        }
    }

    #[test]
    fn numbered_name_keeps_extension() {
        assert_eq!(numbered_name("image.jpg", 1), "image (1).jpg");
        assert_eq!(numbered_name("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered_name("README", 3), "README (3)");
        assert_eq!(numbered_name(".bashrc", 1), ".bashrc (1)");
    }

    #[tokio::test]
    async fn same_name_files_are_numbered_instead_of_overwritten() {
        let (vfs, cfg, _mock, _pool, server, _dir) = test_vfs().await;
        let mut ex = extras();
        ex.inbox_channel_enabled = true;
        // 目标目录里已有同名目录:收件文件既不能覆盖它,也不能因此失败
        let dir = vfs.mkdir_p("/阅读").await.unwrap();
        vfs.db.mkdir(dir.id, "channel (2).pdf").await.unwrap();
        for message_id in [7, 8, 9] {
            let mut raw = channel_update();
            raw["update_id"] = json!(100 + message_id);
            raw["channel_post"]["message_id"] = json!(message_id);
            raw["channel_post"]["document"]["file_id"] = json!(format!("file-{message_id}"));
            handle_update(
                &reqwest::Client::new(),
                &vfs,
                &cfg,
                &ex,
                &serde_json::from_value(raw).unwrap(),
            )
            .await
            .unwrap();
        }
        for (path, msg) in [
            ("/阅读/channel.pdf", 7),
            ("/阅读/channel (1).pdf", 8),
            ("/阅读/channel (3).pdf", 9),
        ] {
            let node = vfs.resolve(path).await.unwrap();
            let chunks = vfs.db.chunks_of(node.id).await.unwrap();
            assert_eq!(chunks[0].tg_message_id, msg, "{path}");
        }
        assert!(vfs.resolve("/阅读/channel (2).pdf").await.unwrap().is_dir);
        server.abort();
    }

    #[tokio::test]
    async fn disabled_updates_advance_offset_and_poll_reloads_switches() {
        let (vfs, cfg, mock, pool, server, _dir) = test_vfs().await;
        crate::extras::save(
            &vfs.db,
            &crate::extras::ExtrasPatch {
                inbox_bot_enabled: Some(true),
                inbox_channel_enabled: Some(true),
                inbox_allowed_chats: Some("42".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        *mock.updates.lock().unwrap() = vec![private_update(), channel_update()];
        // 模拟管理员在长轮询等待期间关掉两个入口。
        *mock.disable_on_poll.lock().unwrap() = Some(vfs.db.clone());
        poll_once(&reqwest::Client::new(), &vfs, &cfg)
            .await
            .unwrap();
        assert_eq!(
            vfs.db.setting_get(OFFSET_KEY).await.unwrap().as_deref(),
            Some("33")
        );
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nodes WHERE is_dir=0")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(*mock.calls.lock().unwrap(), ["getUpdates"]);
        assert!(
            vfs.db
                .settings_with_prefix(FAILURE_KEY_PREFIX)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(seen_chats(&vfs.db).await.unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn channel_inbox_skips_upload_bots_and_shared_channel_messages() {
        let (vfs, cfg, mock, pool, server, _dir) = test_vfs().await;
        let ex = extras();
        let extra = vfs
            .db
            .ensure_storage_target(&cfg.api_base, "200:extra", &cfg.chat_id)
            .await
            .unwrap();
        let uploaded = vfs
            .db
            .create_uploading(crate::db::ROOT_ID, "uploaded.pdf")
            .await
            .unwrap();
        vfs.db
            .add_chunk(uploaded, 0, 12, "original-upload", 7, extra.id, None)
            .await
            .unwrap();
        // 即使上传尚未完成、没有 SHA-256，也不能把同一分块登记成独立文件。
        let update: Update = serde_json::from_value(channel_update()).unwrap();
        handle_update(&reqwest::Client::new(), &vfs, &cfg, &ex, &update)
            .await
            .unwrap();
        assert!(vfs.resolve("/阅读/channel.pdf").await.is_err());

        let mut own_bot = channel_update();
        own_bot["channel_post"]["message_id"] = json!(8);
        own_bot["channel_post"]["from"] = json!({"id": 200, "is_bot": true});
        handle_update(
            &reqwest::Client::new(),
            &vfs,
            &cfg,
            &ex,
            &serde_json::from_value(own_bot).unwrap(),
        )
        .await
        .unwrap();
        assert!(vfs.resolve("/阅读/channel.pdf").await.is_err());

        let other = vfs
            .db
            .ensure_storage_target(&cfg.api_base, "200:extra", "-1002")
            .await
            .unwrap();
        let different_chat = vfs
            .db
            .create_uploading(crate::db::ROOT_ID, "other.pdf")
            .await
            .unwrap();
        vfs.db
            .add_chunk(different_chat, 0, 12, "other-upload", 9, other.id, None)
            .await
            .unwrap();
        let mut external = channel_update();
        external["channel_post"]["message_id"] = json!(9);
        handle_update(
            &reqwest::Client::new(),
            &vfs,
            &cfg,
            &ex,
            &serde_json::from_value(external).unwrap(),
        )
        .await
        .unwrap();
        assert!(vfs.resolve("/阅读/channel.pdf").await.is_ok());
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inbox_sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(mock.calls.lock().unwrap().is_empty());
        server.abort();
    }

    fn extras() -> Extras {
        Extras {
            trash_retention_days: 30,
            backup_auto: false,
            backup_chunk_size: crate::extras::DEFAULT_BACKUP_CHUNK_SIZE,
            backup_telegram_enabled: true,
            backup_webdav_enabled: false,
            backup_webdav_url: String::new(),
            backup_webdav_username: String::new(),
            backup_webdav_password: String::new(),
            backup_s3_enabled: false,
            backup_s3_endpoint: "https://s3.amazonaws.com".into(),
            backup_s3_region: "us-east-1".into(),
            backup_s3_bucket: String::new(),
            backup_s3_prefix: "tgdrive".into(),
            backup_s3_access_key: String::new(),
            backup_s3_secret_key: String::new(),
            backup_s3_session_token: String::new(),
            backup_s3_path_style: true,
            inbox_bot_enabled: true,
            inbox_channel_enabled: true,
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
            auto_delete_message: false,
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
