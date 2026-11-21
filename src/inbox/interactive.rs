//! 私聊交互与独立收件工作循环。

use super::*;
use crate::db::inbox::{InboxRequest, NewInboxRequest};
use crate::extras::{Extras, InboxDirectory};
use serde_json::{Value, json};

const DIRECTORY_PAGE_SIZE: usize = 6;

#[derive(Deserialize, Serialize)]
pub(super) struct CallbackQuery {
    id: String,
    from: User,
    message: Option<Msg>,
    data: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct ReplyMessage {
    pub message_id: i64,
}

#[derive(Debug)]
pub(super) struct BotCallError {
    detail: String,
    uncertain: bool,
    retry_after: u64,
}

impl BotCallError {
    pub(super) fn app_error(&self) -> AppError {
        AppError::Telegram(self.detail.clone())
    }
}

async fn call<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    cfg: &RuntimeCfg,
    method: &str,
    body: Value,
    media: bool,
) -> Result<T, BotCallError> {
    let url = format!(
        "{}/bot{}/{method}",
        cfg.api_base.trim_end_matches('/'),
        cfg.bot_token
    );
    let response = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|error| BotCallError {
            detail: format!(
                "Telegram 请求失败：{}",
                crate::telegram::reqwest_error_summary(&error)
            ),
            uncertain: media && !error.is_connect(),
            retry_after: 0,
        })?;
    let status = response.status();
    let value: Value = response.json().await.map_err(|_| BotCallError {
        detail: "Telegram 响应无法解析".into(),
        uncertain: media,
        retry_after: 0,
    })?;
    if value["ok"].as_bool() == Some(true) && status.is_success() {
        serde_json::from_value(value["result"].clone()).map_err(|_| BotCallError {
            detail: "Telegram 返回结果不完整".into(),
            uncertain: media,
            retry_after: 0,
        })
    } else {
        let description = value["description"]
            .as_str()
            .unwrap_or("请求被拒绝")
            .replace(&cfg.bot_token, "[已脱敏]");
        Err(BotCallError {
            detail: format!(
                "Telegram [{}]：{}",
                value["error_code"]
                    .as_u64()
                    .unwrap_or(u64::from(status.as_u16())),
                description.chars().take(350).collect::<String>()
            ),
            uncertain: media
                && (status.is_server_error()
                    || value["error_code"].as_u64().is_some_and(|code| code >= 500)
                    || value["ok"].as_bool() != Some(false)),
            retry_after: value["parameters"]["retry_after"].as_u64().unwrap_or(0),
        })
    }
}

#[derive(Deserialize)]
struct SentMessage {
    message_id: i64,
}

pub(super) fn bot_id(cfg: &RuntimeCfg) -> String {
    let prefix = cfg.bot_token.split(':').next().unwrap_or("");
    if prefix.parse::<u64>().is_ok_and(|id| id > 0) {
        return prefix.into();
    }
    // 无效配置与合成测试也需要稳定隔离；不能把 Token 写进游标或管理响应。
    use sha2::{Digest, Sha256};
    format!(
        "unknown-{}",
        crate::engine::upload::to_hex(&Sha256::digest(cfg.bot_token.as_bytes())[..8])
    )
}

pub(super) fn allowed(ex: &Extras) -> Vec<i64> {
    ex.inbox_allowed_chats
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

fn safe_source_name(raw: &str, message_id: i64) -> String {
    let name: String = raw
        .chars()
        .map(|character| {
            if matches!(character, '/' | '\\') || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect();
    if name.trim().is_empty() || matches!(name.as_str(), "." | "..") {
        return format!("file_{message_id}");
    }
    name
}

pub(super) fn suffix<'a>(ex: &Extras, original: &'a str) -> &'a str {
    let lower = original.to_lowercase();
    let custom = ex
        .inbox_rules
        .iter()
        .flat_map(|rule| &rule.extensions)
        .filter(|extension| has_extension(&lower, extension))
        .max_by_key(|extension| extension.len());
    let length = custom.map(|extension| extension.len() + 1).or_else(|| {
        ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"]
            .iter()
            .find(|extension| has_extension(&lower, extension))
            .map(|extension| extension.len() + 1)
    });
    if let Some(length) = length {
        let dots = lower[lower.len() - length..].matches('.').count();
        if let Some((index, _)) = original.rmatch_indices('.').nth(dots.saturating_sub(1)) {
            return &original[index..];
        }
    }
    match original.rfind('.') {
        Some(dot) if dot > 0 => &original[dot..],
        _ => "",
    }
}

fn renamed_file(extension: &str, input: &str) -> AppResult<String> {
    let mut stem = input.trim();
    if !extension.is_empty() {
        let dots = extension.matches('.').count();
        if let Some((index, _)) = stem.rmatch_indices('.').nth(dots.saturating_sub(1))
            && stem[index..].to_lowercase() == extension.to_lowercase()
        {
            stem = &stem[..index];
        }
    }
    if stem.is_empty()
        || matches!(stem, "." | "..")
        || stem
            .chars()
            .any(|character| matches!(character, '/' | '\\') || character.is_control())
    {
        return Err(AppError::BadRequest(
            "请输入有效的文件主体名，不能包含目录路径或控制字符".into(),
        ));
    }
    let name = format!("{stem}{extension}");
    if name.len() > 255 || crate::vfs::normalize(&name)?.as_slice() != [name.as_str()] {
        return Err(AppError::BadRequest("文件名过长或无效，请重新输入".into()));
    }
    Ok(name)
}

fn available_directories<'a>(ex: &'a Extras, request: &InboxRequest) -> Vec<&'a InboxDirectory> {
    ex.inbox_directories_for(&request.original_name)
        .iter()
        .filter(|directory| {
            directory.node_id.is_some_and(|id| id > crate::db::ROOT_ID) && !directory.missing
        })
        .collect()
}

fn callback_data(id: &str, action: &str, value: Option<i64>) -> String {
    match value {
        Some(value) => format!("ib:{id}:{action}:{value}"),
        None => format!("ib:{id}:{action}"),
    }
}

fn button(text: impl Into<String>, id: &str, action: &str, value: Option<i64>) -> Value {
    json!({"text": text.into(), "callback_data": callback_data(id, action, value)})
}

fn render(request: &InboxRequest, ex: &Extras, page: usize) -> (String, Value) {
    let original = request.original_name.chars().take(180).collect::<String>();
    let name = request.name.chars().take(180).collect::<String>();
    let mut text = format!("📥 文件：{original}\n保存名称：{name}");
    if request.ended_at.is_none() {
        let selected = request.selected_path.as_deref().unwrap_or("尚未选择");
        text.push_str(&format!(
            "\n目录：{}",
            if selected.len() <= 2048 {
                selected
            } else {
                "目录路径过长，请在管理页调整"
            }
        ));
    }
    let mut keyboard: Vec<Vec<Value>> = Vec::new();
    if request.ended_at.is_some() {
        text.push_str(&match request.status.as_str() {
            "stored" => format!(
                "\n✅ 已存入 {}",
                request.result_path.as_deref().unwrap_or("网盘")
            ),
            "cancelled" => "\n已取消收件，原文件消息保留。".into(),
            "expired" => "\n收件已过期，请重新发送文件。".into(),
            _ => format!(
                "\n收件已失效：{}",
                request.last_error.as_deref().unwrap_or("请重新发送文件")
            ),
        });
    } else if request.status == "uncertain" {
        text.push_str("\n⚠️ 转存结果待核实，请检查存储频道；本次请求不能再次转存。\n结束待办不会删除无法确认的频道副本。");
        keyboard.push(vec![button("结束待办", &request.id, "x", None)]);
    } else if matches!(request.status.as_str(), "copying" | "copied") {
        text.push_str("\n正在存储，请等待处理结果。");
    } else if request.status == "queued" {
        text.push_str("\n已确认，等待开始存储。");
        if !ex.inbox_bot_enabled {
            text.push_str("\n私聊收件已暂停，重新开启后可继续。");
        }
        keyboard.push(vec![button("取消", &request.id, "x", None)]);
    } else {
        let directories = available_directories(ex, request);
        if !ex.inbox_bot_enabled {
            text.push_str("\n私聊收件已暂停，重新开启后可继续。");
        }
        if let Some(error) = &request.last_error {
            text.push_str(&format!("\n提示：{error}"));
        }
        let pages = directories.len().div_ceil(DIRECTORY_PAGE_SIZE).max(1);
        let page = page.min(pages - 1);
        let start = page * DIRECTORY_PAGE_SIZE;
        for directory in directories.iter().skip(start).take(DIRECTORY_PAGE_SIZE) {
            let leaf = directory.path.rsplit('/').next().unwrap_or(&directory.path);
            let duplicate = directories
                .iter()
                .filter(|other| other.path.rsplit('/').next() == Some(leaf))
                .count()
                > 1;
            let label = if duplicate {
                directory.path.clone()
            } else {
                leaf.to_string()
            };
            let selected = directory.node_id == request.selected_dir_id
                && request.selected_path.as_deref() == Some(directory.path.as_str());
            keyboard.push(vec![button(
                format!(
                    "{}{}",
                    if selected { "✓ " } else { "" },
                    label.chars().take(80).collect::<String>()
                ),
                &request.id,
                "d",
                directory.node_id,
            )]);
        }
        if pages > 1 {
            let mut navigation = Vec::new();
            if page > 0 {
                navigation.push(button("上一页", &request.id, "p", Some((page - 1) as i64)));
            }
            if page + 1 < pages {
                navigation.push(button("下一页", &request.id, "p", Some((page + 1) as i64)));
            }
            keyboard.push(navigation);
            text.push_str(&format!("\n目录第 {}/{} 页", page + 1, pages));
        }
        if directories.is_empty() {
            text.push_str("\n暂无有效候选目录，请管理员更新收件配置。");
        }
        if request.status == "renaming" {
            text.push_str("\n请回复改名提示消息，输入文件主体名。");
        }
        if request.status == "renaming" {
            keyboard.push(vec![button("取消改名", &request.id, "n", None)]);
        } else {
            keyboard.push(vec![
                button("重命名", &request.id, "r", None),
                button(
                    if request.status == "failed" {
                        "重试存储"
                    } else {
                        "确认存储"
                    },
                    &request.id,
                    "s",
                    None,
                ),
            ]);
        }
        keyboard.push(vec![button("取消", &request.id, "x", None)]);
        let minutes = ((request.expires_at - crate::db::now_ts()).max(0) as u64).div_ceil(60);
        text.push_str(&format!("\n剩余约 {minutes} 分钟，确认后才存入网盘。"));
    }
    (
        text.chars().take(4000).collect(),
        json!({"inline_keyboard": keyboard}),
    )
}

pub(super) async fn deliver_notice(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    request: &InboxRequest,
    ex: &Extras,
    page: usize,
) -> AppResult<()> {
    deliver_notice_with_actions(http, vfs, cfg, request, ex, page, true).await
}

async fn deliver_notice_with_actions(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    request: &InboxRequest,
    ex: &Extras,
    page: usize,
    actions: bool,
) -> AppResult<()> {
    if request.bot_id != bot_id(cfg) {
        return Ok(());
    }
    if !vfs
        .db
        .inbox_notice_claim(&request.id, request.revision)
        .await?
    {
        return Ok(());
    }
    let (mut text, mut markup) = render(request, ex, page);
    if let Some(target) = request.storage_target_id {
        let target = vfs.db.storage_target(target).await?;
        text.push_str(&format!("\n存储频道：{}", target.chat_id));
    }
    if !actions {
        markup = json!({"inline_keyboard": []});
        text.push_str("\n此收件 Bot 已更换，请联系管理员获取新入口或处理待核实结果。");
    }
    let sent = if let Some(message_id) = request.operation_message_id {
        let edited: Result<Value, _> = call(http, cfg, "editMessageText", json!({"chat_id": request.chat_id, "message_id": message_id, "text": text, "reply_markup": markup}), false).await;
        match edited {
            Ok(_) => Ok(message_id),
            Err(error) if error.detail.contains("message is not modified") => Ok(message_id),
            Err(error) if error.detail.contains("message to edit not found") || error.detail.contains("message can't be edited") => {
                call::<SentMessage>(http, cfg, "sendMessage", json!({"chat_id": request.chat_id,"text": text,"reply_to_message_id": request.source_message_id,"allow_sending_without_reply": true,"reply_markup": markup}), false).await.map(|sent| sent.message_id)
            }
            Err(error) => Err(error),
        }
    } else {
        call::<SentMessage>(http, cfg, "sendMessage", json!({"chat_id": request.chat_id,"text": text,"reply_to_message_id": request.source_message_id,"allow_sending_without_reply": true,"reply_markup": markup}), false).await.map(|sent| sent.message_id)
    };
    match sent {
        Ok(message_id) => {
            vfs.db
                .inbox_notice_done(&request.id, message_id, request.revision, &request.status)
                .await?
        }
        Err(error) => {
            let backoff = 5_i64
                .saturating_mul(1_i64 << request.notice_attempts.clamp(0, 6))
                .min(300)
                .max(error.retry_after.min(3600) as i64);
            vfs.db.inbox_notice_failed(&request.id, backoff).await?;
            tracing::warn!("收件操作消息发送失败：{}", error.detail);
        }
    }
    Ok(())
}

pub(super) async fn enqueue(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    msg: &Msg,
    file: PickedFile,
) -> AppResult<()> {
    let ex = crate::extras::inbox_dirs::ensure_bound(&vfs.db).await?;
    let name = safe_source_name(&file.name, msg.message_id);
    let directories: Vec<_> = ex
        .inbox_directories_for(&name)
        .iter()
        .filter(|directory| !directory.missing && directory.node_id.is_some())
        .collect();
    let selected = if directories.len() == 1 {
        directories.first().copied()
    } else {
        None
    };
    let request = vfs
        .db
        .create_inbox_request(NewInboxRequest {
            origin_target_id: cfg.storage_target_id,
            bot_id: &bot_id(cfg),
            chat_id: msg.chat.id,
            message_id: msg.message_id,
            media_kind: file.kind.field(),
            name: &name,
            suffix: suffix(&ex, &name),
            file_id: &file.file_id,
            size: file.size,
            thumbnail: file.thumbnail_file_id.as_deref(),
            selected_dir_id: selected.and_then(|directory| directory.node_id),
            selected_path: selected.map(|directory| directory.path.as_str()),
        })
        .await;
    match request {
        Ok(request) => deliver_notice(http, vfs, cfg, &request, &ex, 0).await?,
        Err(AppError::Conflict(error)) => {
            reply(http, cfg, msg, &format!("❌ {error}；原文件消息保留。")).await
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn answer(http: &reqwest::Client, cfg: &RuntimeCfg, id: &str, text: &str) {
    let result: Result<Value, _> = call(
        http,
        cfg,
        "answerCallbackQuery",
        json!({"callback_query_id": id,"text": text.chars().take(180).collect::<String>()}),
        false,
    )
    .await;
    if let Err(error) = result {
        tracing::debug!("收件按钮回执失败：{}", error.detail);
    }
}

pub(super) async fn queue_selected(
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    ex: &Extras,
    request: &InboxRequest,
) -> AppResult<()> {
    if request.bot_id != bot_id(cfg) || !allowed(ex).contains(&request.chat_id) {
        return Err(AppError::Forbidden("该收件身份已失效".into()));
    }
    if !ex.inbox_bot_enabled {
        return Err(AppError::BadRequest("私聊收件已暂停".into()));
    }
    if !request.editable() || request.expires_at <= crate::db::now_ts() {
        return Err(AppError::BadRequest("该收件已经结束或正在处理".into()));
    }
    if request.status == "renaming" {
        return Err(AppError::BadRequest(
            "请先回复改名提示，或取消本次改名".into(),
        ));
    }
    if request.name.len() > 255
        || crate::vfs::normalize(&request.name)?.as_slice() != [request.name.as_str()]
    {
        return Err(AppError::BadRequest("文件名过长或无效，请先重命名".into()));
    }
    let directory = ex
        .inbox_directories_for(&request.original_name)
        .iter()
        .find(|directory| directory.node_id == request.selected_dir_id && !directory.missing)
        .filter(|directory| directory.node_id.is_some());
    let Some(directory) = directory else {
        vfs.db.inbox_set_selection(&request.id, None, None).await?;
        return Err(AppError::BadRequest(
            "候选目录已变化，请重新选择目录".into(),
        ));
    };
    let path = vfs
        .db
        .inbox_directory_path(directory.node_id.ok_or(AppError::NotFound)?)
        .await?;
    if path.len() > 2048 {
        return Err(AppError::BadRequest(
            "收件目录路径过长，请管理员缩短目录路径".into(),
        ));
    }
    if request.selected_path.as_deref() != Some(path.as_str()) {
        vfs.db
            .inbox_set_selection(&request.id, directory.node_id, Some(&path))
            .await?;
        return Err(AppError::BadRequest(
            "目录已改名或移动，请查看新路径并再次确认".into(),
        ));
    }
    if !vfs
        .db
        .inbox_queue_request(&request.id, request.revision, ex.inbox_routes_revision)
        .await?
    {
        return Err(AppError::Conflict(
            "收件状态已变化，请查看最新操作消息".into(),
        ));
    }
    Ok(())
}

pub(super) async fn handle_callback(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    ex: &Extras,
    callback: &CallbackQuery,
) -> AppResult<()> {
    let Some(message) = &callback.message else {
        return Ok(());
    };
    if message.chat.kind != "private"
        || message.chat.id != callback.from.id
        || !allowed(ex).contains(&callback.from.id)
    {
        answer(http, cfg, &callback.id, "无权操作此收件").await;
        return Ok(());
    }
    let parts: Vec<_> = callback.data.as_deref().unwrap_or("").split(':').collect();
    if !(3..=4).contains(&parts.len())
        || parts[0] != "ib"
        || parts[1].len() != 32
        || !parts[1].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        answer(http, cfg, &callback.id, "操作消息无效").await;
        return Ok(());
    }
    let request = match vfs.db.inbox_request(parts[1]).await {
        Ok(request) if request.bot_id == bot_id(cfg) && request.chat_id == callback.from.id => {
            request
        }
        Ok(_) => {
            answer(http, cfg, &callback.id, "无权操作此收件").await;
            return Ok(());
        }
        Err(AppError::NotFound) => {
            answer(http, cfg, &callback.id, "操作消息已过期，请重新发送文件").await;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if request.ended_at.is_some() {
        answer(
            http,
            cfg,
            &callback.id,
            request.result_path.as_deref().unwrap_or("收件已结束"),
        )
        .await;
        return Ok(());
    }
    if request.operation_message_id != Some(message.message_id) {
        answer(http, cfg, &callback.id, "请使用最新收件操作消息").await;
        return Ok(());
    }
    // 先结束客户端按钮等待，后续操作不等待实际转存。
    answer(http, cfg, &callback.id, "已收到操作").await;
    let ex = crate::extras::load(&vfs.db).await?;
    let result: AppResult<usize> = async {
        if parts[2] == "x" {
            if !vfs.db.inbox_cancel_request(&request.id).await? { return Err(AppError::BadRequest("正在存储，请等待处理结果".into())); }
            return Ok(0);
        }
        if !ex.inbox_bot_enabled { return Err(AppError::BadRequest("私聊收件已暂停，请管理员重新开启".into())); }
        if !request.editable() { return Err(AppError::BadRequest("正在处理或等待核实，不能修改".into())); }
        if request.expires_at <= crate::db::now_ts() { return Err(AppError::BadRequest("收件已过期，请重新发送文件".into())); }
        match parts[2] {
            "d" => {
                let id = parts.get(3).and_then(|value| value.parse::<i64>().ok()).ok_or_else(|| AppError::BadRequest("目录无效".into()))?;
                let directory = ex.inbox_directories_for(&request.original_name).iter().find(|directory| directory.node_id == Some(id) && !directory.missing).ok_or_else(|| AppError::BadRequest("目录已不在候选列表，请重新选择".into()))?;
                let path = vfs.db.inbox_directory_path(id).await?;
                if !vfs.db.inbox_set_selection(&request.id, directory.node_id, Some(&path)).await? { return Err(AppError::Conflict("收件状态已变化".into())); }
                Ok(0)
            }
            "r" => {
                let extension = request.original_suffix.as_str();
                let prompt = format!("回复此消息，为「{}」输入文件主体名。\n系统保留后缀「{}」，文件内容与类型保持原样。", request.name.chars().take(180).collect::<String>(), if extension.is_empty() { "无后缀" } else { extension });
                let sent = call::<SentMessage>(http, cfg, "sendMessage", json!({"chat_id": request.chat_id,"text": prompt,"reply_markup": {"force_reply": true,"selective": true,"input_field_placeholder": "输入文件主体名"}}), false).await.map_err(|error| error.app_error())?;
                vfs.db.inbox_set_rename_prompt(&request.id, sent.message_id).await?;
                Ok(0)
            }
            "s" => { queue_selected(vfs, cfg, &ex, &request).await?; Ok(0) }
            "n" => {
                if !vfs.db.inbox_abort_rename(&request.id).await? { return Err(AppError::Conflict("改名状态已变化".into())); }
                Ok(0)
            }
            "p" => {
                vfs.db.inbox_mark_notice(&request.id).await?;
                Ok(parts.get(3).and_then(|value| value.parse::<usize>().ok()).unwrap_or(0).min(3))
            }
            _ => Err(AppError::BadRequest("操作无效".into())),
        }
    }.await;
    let page = match result {
        Ok(page) => page,
        Err(error) => {
            reply(http, cfg, message, &format!("❌ {error}")).await;
            0
        }
    };
    vfs.db.inbox_mark_notice(&request.id).await?;
    let latest = vfs.db.inbox_request(&request.id).await?;
    deliver_notice(http, vfs, cfg, &latest, &ex, page).await?;
    Ok(())
}

pub(super) async fn handle_rename_reply(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    ex: &Extras,
    msg: &Msg,
) -> AppResult<()> {
    let Some(reply_to) = &msg.reply_to_message else {
        return Ok(());
    };
    if msg
        .from
        .as_ref()
        .is_some_and(|user| user.id != msg.chat.id || user.is_bot == Some(true))
    {
        return Ok(());
    }
    let Some(request) = vfs
        .db
        .inbox_rename_request(&bot_id(cfg), msg.chat.id, reply_to.message_id)
        .await?
    else {
        return Ok(());
    };
    match renamed_file(&request.original_suffix, msg.text.as_deref().unwrap_or("")) {
        Ok(name) => {
            vfs.db
                .inbox_set_name(
                    &request.id,
                    request.rename_prompt_id.ok_or(AppError::NotFound)?,
                    &name,
                )
                .await?;
            let latest = vfs.db.inbox_request(&request.id).await?;
            deliver_notice(http, vfs, cfg, &latest, ex, 0).await?;
        }
        Err(error) => {
            reply(
                http,
                cfg,
                msg,
                &format!("❌ {error}；请继续回复同一改名提示消息。"),
            )
            .await
        }
    }
    Ok(())
}

pub(super) async fn copy_media(
    http: &reqwest::Client,
    cfg: &RuntimeCfg,
    kind: MediaKind,
    file_id: &str,
) -> Result<(i64, String, Option<i64>, Option<String>), BotCallError> {
    #[derive(Deserialize)]
    struct Sent {
        message_id: i64,
        document: Option<FileRef>,
        video: Option<FileRef>,
        audio: Option<FileRef>,
        animation: Option<FileRef>,
        photo: Option<Vec<PhotoSize>>,
    }
    let mut body = json!({"chat_id": cfg.chat_id,"disable_notification": true});
    body[kind.field()] = json!(file_id);
    let sent: Sent = call(http, cfg, kind.method(), body, true).await?;
    let media = match kind {
        MediaKind::Document => sent.document.map(|file| {
            (
                file.file_id,
                file.file_size,
                file.thumbnail.map(|thumbnail| thumbnail.file_id),
            )
        }),
        MediaKind::Video => sent.video.and_then(|file| {
            select_best_video(&file).map(|selected| {
                (
                    selected.file_id,
                    Some(selected.size),
                    file.thumbnail.map(|thumbnail| thumbnail.file_id),
                )
            })
        }),
        MediaKind::Audio => sent.audio.map(|file| {
            (
                file.file_id,
                file.file_size,
                file.thumbnail.map(|thumbnail| thumbnail.file_id),
            )
        }),
        MediaKind::Animation => sent.animation.map(|file| {
            (
                file.file_id,
                file.file_size,
                file.thumbnail.map(|thumbnail| thumbnail.file_id),
            )
        }),
        MediaKind::Photo => sent
            .photo
            .and_then(|photos| photos.into_iter().last())
            .map(|photo| (photo.file_id, photo.file_size, None)),
    };
    let Some((id, size, thumbnail)) = media.filter(|file| !file.0.trim().is_empty()) else {
        return Err(BotCallError {
            detail: format!(
                "Telegram 已返回消息 {}，但缺少有效媒体引用，请核实频道",
                sent.message_id
            ),
            uncertain: true,
            retry_after: 0,
        });
    };
    Ok((sent.message_id, id, size, thumbnail))
}

fn media_kind(kind: &str) -> AppResult<MediaKind> {
    match kind {
        "document" => Ok(MediaKind::Document),
        "video" => Ok(MediaKind::Video),
        "audio" => Ok(MediaKind::Audio),
        "animation" => Ok(MediaKind::Animation),
        "photo" => Ok(MediaKind::Photo),
        _ => Err(AppError::BadRequest("收件媒体类型无效".into())),
    }
}

pub(super) async fn process_one(
    vfs: &Vfs,
    rt: &Runtime,
    task: Option<&crate::tasks::TaskReporter>,
) -> AppResult<bool> {
    let cfg = rt.current();
    if !cfg.is_configured() || vfs.db.background_blocked() {
        return Ok(false);
    }
    let Some(request) = vfs.db.inbox_next_work(&bot_id(&cfg)).await? else {
        return Ok(false);
    };
    let ex = crate::extras::load(&vfs.db).await?;
    if request.status == "queued" {
        if !ex.inbox_bot_enabled {
            return Ok(false);
        }
        let target = cfg
            .storage_target_id
            .ok_or_else(|| AppError::BadRequest("尚未配置存储目标".into()))?;
        if request.copied_message_id.is_none() {
            vfs.tg.binding(target)?.api.wait_upload_turn().await;
        }
        // 等待共享发送时隙期间配置或取消可能变化，必须再次检查。
        let current = rt.current();
        let ex = crate::extras::inbox_dirs::ensure_bound(&vfs.db).await?;
        if !ex.inbox_bot_enabled
            || bot_id(&current) != request.bot_id
            || current.storage_target_id != Some(target)
            || !allowed(&ex).contains(&request.chat_id)
        {
            return Ok(false);
        }
        let latest = vfs.db.inbox_request(&request.id).await?;
        if latest.status != "queued" {
            return Ok(false);
        }
        let directory = ex
            .inbox_directories_for(&latest.original_name)
            .iter()
            .find(|directory| directory.node_id == latest.selected_dir_id && !directory.missing);
        let Some(directory) = directory else {
            vfs.db
                .inbox_fail_request(&latest.id, "候选目录已变化，请重新选择", false)
                .await?;
            vfs.db.inbox_set_selection(&latest.id, None, None).await?;
            return Ok(false);
        };
        let path = vfs
            .db
            .inbox_directory_path(directory.node_id.ok_or(AppError::NotFound)?)
            .await?;
        if latest.selected_path.as_deref() != Some(path.as_str()) {
            vfs.db
                .inbox_fail_request(&latest.id, "目录已改名或移动，请再次确认新路径", false)
                .await?;
            vfs.db
                .inbox_set_selection(&latest.id, directory.node_id, Some(&path))
                .await?;
            return Ok(false);
        }
        let _snapshot = crate::proxy::ProxySnapshot::new(current.proxy.clone())?;
        let http = inbox_http_client(&current.proxy, Duration::from_secs(40))?;
        let kind = media_kind(&latest.media_kind)?;
        if !vfs
            .db
            .inbox_claim_request(&latest.id, target, ex.inbox_routes_revision)
            .await?
        {
            return Ok(false);
        }
        let request = vfs.db.inbox_request(&latest.id).await?;
        if request.status == "copying" {
            let after_claim = rt.current();
            if bot_id(&after_claim) != request.bot_id
                || after_claim.storage_target_id != Some(target)
                || after_claim.proxy != current.proxy
            {
                vfs.db.inbox_unclaim_unsent(&request.id).await?;
                return Ok(false);
            }
            match copy_media(&http, &current, kind, &request.file_id).await {
                Ok((message, file_id, size, thumbnail)) => {
                    let size = positive_size(size).unwrap_or(request.file_size);
                    loop {
                        match vfs
                            .db
                            .inbox_record_copy(
                                &request.id,
                                message,
                                &file_id,
                                size,
                                thumbnail
                                    .as_deref()
                                    .or(request.thumbnail_file_id.as_deref()),
                            )
                            .await
                        {
                            Ok(()) => break,
                            Err(error) => {
                                if let Some(task) = task {
                                    task.report_error(&error.to_string());
                                    if !task.sleep(Duration::from_secs(5)).await {
                                        return Ok(false);
                                    }
                                } else {
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    if error.retry_after > 0 {
                        vfs.tg
                            .binding(target)?
                            .api
                            .defer_upload_turn(error.retry_after)
                            .await;
                    }
                    vfs.db
                        .inbox_fail_request(&request.id, &error.detail, error.uncertain)
                        .await?;
                    return Ok(false);
                }
            }
        }
    }
    match vfs.db.finish_inbox_request(&request.id).await {
        Ok(_) => Ok(true),
        Err(error) => {
            vfs.db
                .inbox_fail_request(&request.id, &error.to_string(), false)
                .await?;
            Ok(false)
        }
    }
}

pub(super) async fn worker_loop(
    vfs: Vfs,
    rt: Arc<Runtime>,
    events: crate::events::EventBus,
    task: crate::tasks::TaskReporter,
) {
    loop {
        if task.is_cancelled() {
            return;
        }
        task.heartbeat();
        let result: AppResult<()> = async {
            if vfs.db.background_blocked() {
                return Ok(());
            }
            let cfg = rt.current();
            let ex = crate::extras::load(&vfs.db).await?;
            let current_bot = bot_id(&cfg);
            vfs.db
                .maintain_inbox_requests(
                    if cfg.is_configured() {
                        &current_bot
                    } else {
                        ""
                    },
                    &allowed(&ex),
                )
                .await?;
            if !cfg.is_configured() {
                return Ok(());
            }
            let _snapshot = crate::proxy::ProxySnapshot::new(cfg.proxy.clone())?;
            let http = inbox_http_client(&cfg.proxy, Duration::from_secs(10))?;
            for request in vfs.db.inbox_notices(&bot_id(&cfg)).await? {
                deliver_notice(&http, &vfs, &cfg, &request, &ex, 0).await?;
            }
            if process_one(&vfs, &rt, Some(&task)).await? {
                crate::events::emit(&events, crate::events::HostEvent::files());
            }
            // 当前工作优先；旧 Bot 只发送终态通知，失败最多重试三次。
            for request in vfs.db.inbox_retired_notices(&current_bot).await? {
                deliver_retired_notice(&http, &vfs, &cfg, &request, &ex).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            task.report_error(&error.to_string());
            tracing::warn!("收件工作任务：{error}");
        }
        if !task.sleep(Duration::from_secs(1)).await {
            return;
        }
    }
}

pub(super) async fn deliver_retired_notice(
    http: &reqwest::Client,
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    request: &InboxRequest,
    ex: &Extras,
) -> AppResult<()> {
    let target = vfs
        .db
        .storage_target(request.origin_target_id.ok_or(AppError::NotFound)?)
        .await?;
    let mut origin = cfg.clone();
    origin.api_base = target.api_base;
    origin.bot_token = target.bot_token;
    origin.chat_id = target.chat_id;
    deliver_notice_with_actions(http, vfs, &origin, request, ex, 0, false).await
}

pub async fn admin_retry(vfs: &Vfs, cfg: &RuntimeCfg, id: &str) -> AppResult<()> {
    let ex = crate::extras::load(&vfs.db).await?;
    let request = vfs.db.inbox_request(id).await?;
    if request.status != "failed" {
        return Err(AppError::BadRequest("仅明确失败的收件可以重试".into()));
    }
    queue_selected(vfs, cfg, &ex, &request).await
}
