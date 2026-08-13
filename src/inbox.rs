//! Bot 收件:getUpdates 长轮询,把发给 bot 的文件自动登记进网盘。
//!
//! 两条入口:
//! - **存储频道直发**:文件本来就落在存储频道,直接把 file_id/message_id 登记成
//!   单块文件,零流量入库(bot 自己发的消息不会出现在 getUpdates 里,不会自吞分块)。
//! - **私聊白名单**:把 document 以 file_id 重发到存储频道(Telegram 侧复制,同样
//!   零流量),再登记;完成后回复存放路径。
//!
//! 官方 Bot API 下载单文件上限 20MB:超限的文件照样入库,下载时才会受限(自建端点无此限制)。

use crate::error::{AppError, AppResult};
use crate::settings::{Runtime, RuntimeCfg};
use crate::vfs::Vfs;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const POLL_TIMEOUT_SECS: u64 = 25;
const OFFSET_KEY: &str = "extra.inbox_offset";

#[derive(Deserialize)]
struct ApiResp<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Msg>,
    channel_post: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    message_id: i64,
    chat: Chat,
    caption: Option<String>,
    document: Option<FileRef>,
    video: Option<FileRef>,
    audio: Option<FileRef>,
    photo: Option<Vec<PhotoSize>>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    username: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct FileRef {
    file_id: String,
    file_size: Option<i64>,
    file_name: Option<String>,
}

#[derive(Deserialize)]
struct PhotoSize {
    file_id: String,
    file_size: Option<i64>,
}

pub async fn run(vfs: Vfs, rt: Arc<Runtime>) {
    // 长轮询本身要 25s,请求超时给足余量
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 15))
        .build()
        .expect("构造 HTTP 客户端失败");
    let mut backoff = 5u64;
    loop {
        let cfg = rt.current();
        let ex = match crate::extras::load(&vfs.db).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("收件:读配置失败: {e}");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        if !ex.inbox_enabled || !cfg.is_configured() {
            tokio::time::sleep(Duration::from_secs(15)).await;
            continue;
        }
        match poll_once(&http, &vfs, &cfg, &ex).await {
            Ok(()) => backoff = 5,
            Err(e) => {
                tracing::warn!("收件轮询失败({e}),{backoff}s 后重试");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
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
            "allowed_updates": ["message", "channel_post"],
        }))
        .send()
        .await
        .map_err(|e| AppError::Telegram(format!("getUpdates 请求失败: {e}")))?;
    let parsed: ApiResp<Vec<Update>> = resp
        .json()
        .await
        .map_err(|e| AppError::Telegram(format!("getUpdates 响应解析失败: {e}")))?;
    if !parsed.ok {
        return Err(AppError::Telegram(format!(
            "getUpdates: {}",
            parsed.description.unwrap_or_else(|| "未知错误".into())
        )));
    }

    let updates = parsed.result.unwrap_or_default();
    let mut max_id = offset - 1;
    for up in &updates {
        max_id = max_id.max(up.update_id);
        if let Err(e) = handle_update(http, vfs, cfg, ex, up).await {
            // 单条失败不阻塞后续;offset 照常推进,靠日志排查
            tracing::warn!("收件:处理 update {} 失败: {e}", up.update_id);
        }
    }
    if !updates.is_empty() {
        vfs.db.setting_set(OFFSET_KEY, &(max_id + 1).to_string()).await?;
    }
    Ok(())
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
            if let Some((name, size, file_id)) = pick_file(post) {
                let path = ingest(vfs, ex, &name, size, &file_id, post.message_id).await?;
                tracing::info!("收件:频道文件已入库 {path}");
            }
        }
        return Ok(());
    }

    // 私聊:白名单内的 document 转存到频道再登记
    let Some(msg) = &up.message else { return Ok(()) };
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

    if let Some(doc) = &msg.document {
        let (mid, fid, size) = resend_document(http, cfg, &doc.file_id).await?;
        let size = size.or(doc.file_size).unwrap_or(0);
        if size <= 0 {
            reply(http, cfg, msg, "❌ 拿不到文件大小,无法登记").await;
            return Ok(());
        }
        let name = doc.file_name.clone().unwrap_or_else(|| format!("file_{mid}"));
        let path = ingest(vfs, ex, &name, size, &fid, mid).await?;
        reply(http, cfg, msg, &format!("✅ 已存入 {path}")).await;
    } else if msg.photo.is_some() || msg.video.is_some() || msg.audio.is_some() {
        reply(http, cfg, msg, "请以「文件」方式发送(不压缩),才能原样入库").await;
    }
    Ok(())
}

/// 频道消息里挑出可入库的文件:(文件名, 大小, file_id)
fn pick_file(m: &Msg) -> Option<(String, i64, String)> {
    if let Some(d) = &m.document {
        let name = d.file_name.clone().unwrap_or_else(|| format!("file_{}", m.message_id));
        return Some((name, d.file_size?, d.file_id.clone()));
    }
    if let Some(v) = &m.video {
        let name = v.file_name.clone().unwrap_or_else(|| format!("video_{}.mp4", m.message_id));
        return Some((name, v.file_size?, v.file_id.clone()));
    }
    if let Some(a) = &m.audio {
        let name = a.file_name.clone().unwrap_or_else(|| format!("audio_{}.mp3", m.message_id));
        return Some((name, a.file_size?, a.file_id.clone()));
    }
    if let Some(sizes) = &m.photo {
        // photo 数组末尾是最大分辨率
        let best = sizes.last()?;
        return Some((format!("photo_{}.jpg", m.message_id), best.file_size?, best.file_id.clone()));
    }
    None
}

/// tgdrive 自己传的分块 caption 以 "tgdrive {" 开头;正常情况 getUpdates
/// 收不到 bot 自己的消息,这里只是多一道保险
fn is_own_chunk(m: &Msg) -> bool {
    m.caption.as_deref().is_some_and(|c| c.starts_with("tgdrive {"))
}

fn is_storage_chat(cfg_chat: &str, chat: &Chat) -> bool {
    if let Ok(id) = cfg_chat.trim().parse::<i64>() {
        return chat.id == id;
    }
    cfg_chat.strip_prefix('@').is_some_and(|n| chat.username.as_deref() == Some(n))
}

/// 把频道里的一条文件消息登记为网盘单块文件(同名覆盖,与上传语义一致)
async fn ingest(
    vfs: &Vfs,
    ex: &crate::extras::Extras,
    raw_name: &str,
    size: i64,
    file_id: &str,
    message_id: i64,
) -> AppResult<String> {
    // 文件名可能含路径分隔符等,压成单段安全名
    let name: String = raw_name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let name = if name.trim().is_empty() { format!("file_{message_id}") } else { name };

    let dir = vfs.mkdir_p(&ex.inbox_dir).await?;
    let node_id = vfs.db.create_uploading(dir.id, &name).await?;
    vfs.db.add_chunk(node_id, 0, size, file_id, message_id).await?;
    let freed = vfs.db.finalize_upload(node_id, size, None).await?;
    vfs.uploader.delete_messages_best_effort(freed);
    Ok(format!("{}/{}", ex.inbox_dir.trim_end_matches('/'), name))
}

/// 以 file_id 重发 document 到存储频道(Telegram 侧复制,零流量),
/// 返回 (message_id, 新 file_id, file_size)
async fn resend_document(
    http: &reqwest::Client,
    cfg: &RuntimeCfg,
    file_id: &str,
) -> AppResult<(i64, String, Option<i64>)> {
    #[derive(Deserialize)]
    struct Sent {
        message_id: i64,
        document: Option<FileRef>,
    }
    let url = format!("{}/bot{}/sendDocument", cfg.api_base, cfg.bot_token);
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": cfg.chat_id,
            "document": file_id,
            "disable_notification": true,
        }))
        .send()
        .await
        .map_err(|e| AppError::Telegram(format!("转存失败: {e}")))?;
    let parsed: ApiResp<Sent> = resp
        .json()
        .await
        .map_err(|e| AppError::Telegram(format!("转存响应解析失败: {e}")))?;
    let sent = match (parsed.ok, parsed.result) {
        (true, Some(s)) => s,
        _ => {
            return Err(AppError::Telegram(format!(
                "转存被拒: {}",
                parsed.description.unwrap_or_else(|| "未知错误".into())
            )))
        }
    };
    let doc = sent
        .document
        .ok_or_else(|| AppError::Telegram("转存响应缺少 document".into()))?;
    Ok((sent.message_id, doc.file_id, doc.file_size))
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
        tracing::warn!("收件:回复失败: {e}");
    }
}
