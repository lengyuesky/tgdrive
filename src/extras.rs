//! 维护类可调项（回收站保留天数 / 多渠道自动备份 / Bot 收件），存 settings 表 `extra.*` 键。
//! 与 `cfg.*` 配置体系分开:这些项没有环境变量层,纯后台开关。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const DEFAULT_TRASH_DAYS: i64 = 30;
pub const DEFAULT_INBOX_DIR: &str = "/收件箱";
/// 数据库备份默认按官方 Bot API 可安全下载的大小分块。
pub const DEFAULT_BACKUP_CHUNK_SIZE: u64 = crate::config::OFFICIAL_CHUNK_DEFAULT;
/// 管理端展示已保存凭据时使用固定占位符，避免泄露长度与内容。
pub const BACKUP_SECRET_PLACEHOLDER: &str = "••••••••";
const MAX_INBOX_RULES: usize = 50;
const MAX_RULE_EXTENSIONS: usize = 100;

/// 用户自定义的收件路由；后缀不带点，统一保存为小写。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxRouteRule {
    pub name: String,
    pub extensions: Vec<String>,
    pub dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Extras {
    /// 回收站保留天数;0 = 永不自动清理
    pub trash_retention_days: i64,
    /// 每日自动把数据库快照投递到所有已启用渠道
    pub backup_auto: bool,
    /// 数据库快照上传到 Telegram 时的单块大小
    pub backup_chunk_size: u64,
    /// 是否把数据库快照备份到当前 Telegram 存储频道；旧配置默认开启。
    pub backup_telegram_enabled: bool,
    /// 是否把数据库快照备份到 WebDAV。
    pub backup_webdav_enabled: bool,
    /// WebDAV 备份目录 URL，目录需要预先存在。
    pub backup_webdav_url: String,
    pub backup_webdav_username: String,
    #[serde(serialize_with = "serialize_backup_secret")]
    pub backup_webdav_password: String,
    /// 是否把数据库快照备份到 S3 兼容对象存储。
    pub backup_s3_enabled: bool,
    pub backup_s3_endpoint: String,
    pub backup_s3_region: String,
    pub backup_s3_bucket: String,
    pub backup_s3_prefix: String,
    #[serde(serialize_with = "serialize_backup_secret")]
    pub backup_s3_access_key: String,
    #[serde(serialize_with = "serialize_backup_secret")]
    pub backup_s3_secret_key: String,
    #[serde(serialize_with = "serialize_backup_secret")]
    pub backup_s3_session_token: String,
    /// true 使用 endpoint/bucket/key，false 使用 bucket.endpoint/key。
    pub backup_s3_path_style: bool,
    /// 白名单用户私聊 Bot 的文件转存。
    pub inbox_bot_enabled: bool,
    /// 存储频道内直接发送的文件自动入库，与私聊收件独立控制。
    pub inbox_channel_enabled: bool,
    /// 允许私聊收件的 chat id，逗号分隔；空表示不接收任何私聊文件。
    pub inbox_allowed_chats: String,
    /// 未命中任何类型规则时的兜底目录
    pub inbox_dir: String,
    /// 内置视频类型的目录
    pub inbox_video_dir: String,
    /// 内置图片类型的目录
    pub inbox_image_dir: String,
    /// 内置书籍类型的目录
    pub inbox_book_dir: String,
    /// 自定义类型规则；优先于内置类型，重叠时更长的后缀优先
    pub inbox_rules: Vec<InboxRouteRule>,
    /// 删除文件后自动清理 Telegram 频道里的对应消息；关闭时改由后台手动执行。
    pub auto_delete_message: bool,
}

fn serialize_backup_secret<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(if value.is_empty() {
        ""
    } else {
        BACKUP_SECRET_PLACEHOLDER
    })
}

async fn get(db: &Db, k: &str) -> AppResult<Option<String>> {
    db.setting_get(&format!("extra.{k}")).await
}

async fn set(db: &Db, k: &str, v: &str) -> AppResult<()> {
    db.setting_set(&format!("extra.{k}"), v).await
}

pub async fn load(db: &Db) -> AppResult<Extras> {
    let (inbox_bot_enabled, inbox_channel_enabled) = load_inbox_switches(db).await?;
    let inbox_dir = get(db, "inbox_dir")
        .await?
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_INBOX_DIR.into());
    let child_dir = |name: &str| format!("{}/{name}", inbox_dir.trim_end_matches('/'));
    let inbox_rules = match get(db, "inbox_rules").await? {
        Some(raw) => match serde_json::from_str::<Vec<InboxRouteRule>>(&raw)
            .map_err(|e| AppError::BadRequest(format!("自定义收件规则 JSON 无效:{e}")))
            .and_then(|rules| normalize_rules(&rules))
        {
            Ok(rules) => rules,
            Err(e) => {
                tracing::warn!("忽略损坏的自定义收件规则:{e}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let backup_chunk_size = match get(db, "backup_chunk_size").await? {
        Some(raw) => match parse_backup_chunk_size(&raw) {
            Ok(size) => size,
            Err(error) => {
                tracing::warn!(
                    "忽略无效的数据库备份分块大小（{error}），改用默认值 {DEFAULT_BACKUP_CHUNK_SIZE}"
                );
                DEFAULT_BACKUP_CHUNK_SIZE
            }
        },
        None => DEFAULT_BACKUP_CHUNK_SIZE,
    };
    Ok(Extras {
        trash_retention_days: get(db, "trash_retention_days")
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TRASH_DAYS),
        backup_auto: get(db, "backup_auto").await?.as_deref() == Some("1"),
        backup_chunk_size,
        backup_telegram_enabled: get(db, "backup_telegram_enabled").await?.as_deref() != Some("0"),
        backup_webdav_enabled: get(db, "backup_webdav_enabled").await?.as_deref() == Some("1"),
        backup_webdav_url: get(db, "backup_webdav_url").await?.unwrap_or_default(),
        backup_webdav_username: get(db, "backup_webdav_username").await?.unwrap_or_default(),
        backup_webdav_password: get(db, "backup_webdav_password").await?.unwrap_or_default(),
        backup_s3_enabled: get(db, "backup_s3_enabled").await?.as_deref() == Some("1"),
        backup_s3_endpoint: get(db, "backup_s3_endpoint")
            .await?
            .unwrap_or_else(|| "https://s3.amazonaws.com".into()),
        backup_s3_region: get(db, "backup_s3_region")
            .await?
            .unwrap_or_else(|| "us-east-1".into()),
        backup_s3_bucket: get(db, "backup_s3_bucket").await?.unwrap_or_default(),
        backup_s3_prefix: get(db, "backup_s3_prefix")
            .await?
            .unwrap_or_else(|| "tgdrive".into()),
        backup_s3_access_key: get(db, "backup_s3_access_key").await?.unwrap_or_default(),
        backup_s3_secret_key: get(db, "backup_s3_secret_key").await?.unwrap_or_default(),
        backup_s3_session_token: get(db, "backup_s3_session_token")
            .await?
            .unwrap_or_default(),
        backup_s3_path_style: get(db, "backup_s3_path_style").await?.as_deref() != Some("0"),
        inbox_bot_enabled,
        inbox_channel_enabled,
        inbox_allowed_chats: get(db, "inbox_allowed_chats").await?.unwrap_or_default(),
        inbox_video_dir: get(db, "inbox_video_dir")
            .await?
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| child_dir("视频")),
        inbox_image_dir: get(db, "inbox_image_dir")
            .await?
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| child_dir("图片")),
        inbox_book_dir: get(db, "inbox_book_dir")
            .await?
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| child_dir("书籍")),
        inbox_dir,
        inbox_rules,
        // 消息清理队列默认手动，避免 Telegram 永久性错误持续刷屏。
        auto_delete_message: get(db, "auto_delete_message").await?.as_deref() == Some("1"),
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ExtrasPatch {
    pub trash_retention_days: Option<i64>,
    pub backup_auto: Option<bool>,
    /// 字符串形式便于后台使用 `18MB`、`1GB` 等写法。
    pub backup_chunk_size: Option<String>,
    pub backup_telegram_enabled: Option<bool>,
    pub backup_webdav_enabled: Option<bool>,
    pub backup_webdav_url: Option<String>,
    pub backup_webdav_username: Option<String>,
    pub backup_webdav_password: Option<String>,
    pub backup_s3_enabled: Option<bool>,
    pub backup_s3_endpoint: Option<String>,
    pub backup_s3_region: Option<String>,
    pub backup_s3_bucket: Option<String>,
    pub backup_s3_prefix: Option<String>,
    pub backup_s3_access_key: Option<String>,
    pub backup_s3_secret_key: Option<String>,
    pub backup_s3_session_token: Option<String>,
    pub backup_s3_path_style: Option<bool>,
    /// 兼容旧版总开关；新开关同时提交时，以各自的新值为准。
    pub inbox_enabled: Option<bool>,
    pub inbox_bot_enabled: Option<bool>,
    pub inbox_channel_enabled: Option<bool>,
    pub inbox_allowed_chats: Option<String>,
    pub inbox_dir: Option<String>,
    pub inbox_video_dir: Option<String>,
    pub inbox_image_dir: Option<String>,
    pub inbox_book_dir: Option<String>,
    pub inbox_rules: Option<Vec<InboxRouteRule>>,
    pub auto_delete_message: Option<bool>,
}

/// 旧版只有总开关，尚未保存独立开关时继承原有行为。
pub async fn load_inbox_switches(db: &Db) -> AppResult<(bool, bool)> {
    let values = db
        .settings_get(&[
            "extra.inbox_enabled",
            "extra.inbox_bot_enabled",
            "extra.inbox_channel_enabled",
        ])
        .await?;
    let legacy = values
        .get("extra.inbox_enabled")
        .is_some_and(|value| value == "1");
    let enabled = |key: &str| values.get(key).map_or(legacy, |value| value == "1");
    Ok((
        enabled("extra.inbox_bot_enabled"),
        enabled("extra.inbox_channel_enabled"),
    ))
}

fn normalize_http_url(raw: &str, label: &str) -> AppResult<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let mut url = reqwest::Url::parse(raw)
        .map_err(|_| AppError::BadRequest(format!("{label}不是有效 URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AppError::BadRequest(format!(
            "{label}必须是 http:// 或 https:// 地址"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::BadRequest(format!(
            "{label}不能内嵌账号密码，请使用独立凭据字段"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AppError::BadRequest(format!(
            "{label}不能包含查询参数或片段"
        )));
    }
    // 保存为稳定的目录/端点形式，拼接备份文件名时再追加路径段。
    let normalized_path = url.path().trim_end_matches('/').to_string();
    url.set_path(&normalized_path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn normalize_s3_prefix(raw: &str) -> AppResult<String> {
    let prefix = raw.trim().trim_matches('/');
    if prefix.chars().count() > 512
        || prefix.chars().any(char::is_control)
        || prefix.split('/').any(|segment| segment == "..")
    {
        return Err(AppError::BadRequest("S3 对象前缀无效".into()));
    }
    Ok(prefix.to_string())
}

fn validate_backup_destinations(extras: &Extras) -> AppResult<()> {
    if extras.backup_auto
        && !extras.backup_telegram_enabled
        && !extras.backup_webdav_enabled
        && !extras.backup_s3_enabled
    {
        return Err(AppError::BadRequest(
            "开启每日自动备份时至少要启用一个备份渠道".into(),
        ));
    }
    if extras.backup_webdav_enabled && extras.backup_webdav_url.is_empty() {
        return Err(AppError::BadRequest(
            "启用 WebDAV 备份前请填写目录 URL".into(),
        ));
    }
    if extras.backup_webdav_username.is_empty() && !extras.backup_webdav_password.is_empty() {
        return Err(AppError::BadRequest(
            "填写 WebDAV 密码时也需要填写用户名".into(),
        ));
    }
    if extras.backup_s3_enabled {
        if extras.backup_s3_endpoint.is_empty()
            || extras.backup_s3_region.is_empty()
            || extras.backup_s3_bucket.is_empty()
            || extras.backup_s3_access_key.is_empty()
            || extras.backup_s3_secret_key.is_empty()
        {
            return Err(AppError::BadRequest(
                "启用 S3 备份前请完整填写端点、区域、存储桶、Access Key 和 Secret Key".into(),
            ));
        }
        if extras.backup_s3_bucket.len() > 255
            || extras.backup_s3_bucket.contains('/')
            || extras.backup_s3_bucket.chars().any(char::is_control)
        {
            return Err(AppError::BadRequest("S3 存储桶名称无效".into()));
        }
        if extras.backup_s3_region.len() > 128
            || !extras
                .backup_s3_region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(AppError::BadRequest("S3 区域名称无效".into()));
        }
        if !extras.backup_s3_path_style
            && extras
                .backup_s3_endpoint
                .parse::<reqwest::Url>()
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .is_some_and(|host| host.parse::<std::net::IpAddr>().is_ok())
        {
            return Err(AppError::BadRequest(
                "IP 地址形式的 S3 端点必须开启路径样式".into(),
            ));
        }
    }
    Ok(())
}

/// 解析并校验数据库备份分块大小的全局范围。
pub fn parse_backup_chunk_size(raw: &str) -> AppResult<u64> {
    let size = crate::config::parse_size(raw)
        .map_err(|error| AppError::BadRequest(format!("数据库备份分块大小无效：{error}")))?;
    if size < crate::config::MIN_CHUNK {
        return Err(AppError::BadRequest(
            "数据库备份分块大小不得小于 1MB".into(),
        ));
    }
    if size > crate::config::SELF_HOSTED_CHUNK_MAX {
        return Err(AppError::BadRequest(
            "数据库备份分块大小不得超过 1900MB".into(),
        ));
    }
    Ok(size)
}

/// 校验备份分块能否由当前 Bot API 端点上传并重新下载。
pub fn validate_backup_chunk_size(api_base: &str, size: u64) -> AppResult<()> {
    crate::config::validate_chunk_size(api_base, size)
        .map_err(|error| AppError::BadRequest(format!("数据库备份分块大小无效：{error}")))
}

fn normalize_dir(raw: &str, label: &str) -> AppResult<String> {
    let segs = crate::vfs::normalize(raw)?;
    if segs.is_empty() {
        return Err(AppError::BadRequest(format!("{label}不能是根目录")));
    }
    Ok(format!("/{}", segs.join("/")))
}

/// 一个输入框里也允许写成 `mp3, flac`，后端统一拆开、去点、转小写。
fn split_extensions(values: &[String]) -> AppResult<Vec<String>> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for raw in values {
        for part in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
            let ext = part.trim().trim_start_matches('.').to_lowercase();
            if ext.is_empty() {
                continue;
            }
            if ext.chars().count() > 32
                || ext.starts_with('.')
                || ext.ends_with('.')
                || ext.split('.').any(str::is_empty)
                || ext
                    .chars()
                    .any(|c| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')))
            {
                return Err(AppError::BadRequest(format!("文件后缀「{part}」无效")));
            }
            if seen.insert(ext.clone()) {
                normalized.push(ext);
            }
        }
    }
    if normalized.is_empty() {
        return Err(AppError::BadRequest(
            "自定义类型至少需要一个文件后缀".into(),
        ));
    }
    if normalized.len() > MAX_RULE_EXTENSIONS {
        return Err(AppError::BadRequest(format!(
            "每条自定义类型最多 {MAX_RULE_EXTENSIONS} 个后缀"
        )));
    }
    Ok(normalized)
}

fn normalize_rules(rules: &[InboxRouteRule]) -> AppResult<Vec<InboxRouteRule>> {
    if rules.len() > MAX_INBOX_RULES {
        return Err(AppError::BadRequest(format!(
            "自定义类型最多 {MAX_INBOX_RULES} 条"
        )));
    }
    let mut normalized = Vec::with_capacity(rules.len());
    let mut all_extensions = HashSet::new();
    let mut all_names = HashSet::new();
    for rule in rules {
        let name = rule.name.trim();
        if name.is_empty() || name.chars().count() > 40 || name.chars().any(|c| c.is_control()) {
            return Err(AppError::BadRequest(
                "自定义类型名称需为 1~40 个字符".into(),
            ));
        }
        if !all_names.insert(name.to_lowercase()) {
            return Err(AppError::BadRequest(format!(
                "自定义类型名称「{name}」重复"
            )));
        }
        let extensions = split_extensions(&rule.extensions)?;
        for ext in &extensions {
            if !all_extensions.insert(ext.clone()) {
                return Err(AppError::BadRequest(format!(
                    "文件后缀「.{ext}」出现在多条自定义规则中"
                )));
            }
        }
        normalized.push(InboxRouteRule {
            name: name.to_string(),
            extensions,
            dir: normalize_dir(&rule.dir, &format!("「{name}」的存储目录"))?,
        });
    }
    Ok(normalized)
}

pub async fn save(db: &Db, p: &ExtrasPatch) -> AppResult<()> {
    // 先完成全部校验，再写数据库，避免后面的字段无效造成部分保存。
    if let Some(d) = p.trash_retention_days
        && !(0..=3650).contains(&d)
    {
        return Err(AppError::BadRequest(
            "保留天数需在 0~3650 之间(0 为永不清理)".into(),
        ));
    }
    let inbox_allowed_chats = if let Some(v) = &p.inbox_allowed_chats {
        // 只允许数字(可带负号)的 chat id 列表
        for part in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if part.parse::<i64>().is_err() {
                return Err(AppError::BadRequest(format!(
                    "白名单应为数字 chat id(逗号分隔),「{part}」无效"
                )));
            }
        }
        Some(v.trim())
    } else {
        None
    };
    let inbox_dir = p
        .inbox_dir
        .as_deref()
        .map(|v| normalize_dir(v, "默认收件目录"))
        .transpose()?;
    let inbox_video_dir = p
        .inbox_video_dir
        .as_deref()
        .map(|v| normalize_dir(v, "视频目录"))
        .transpose()?;
    let inbox_image_dir = p
        .inbox_image_dir
        .as_deref()
        .map(|v| normalize_dir(v, "图片目录"))
        .transpose()?;
    let inbox_book_dir = p
        .inbox_book_dir
        .as_deref()
        .map(|v| normalize_dir(v, "书籍目录"))
        .transpose()?;
    let inbox_rules = p.inbox_rules.as_deref().map(normalize_rules).transpose()?;
    let backup_chunk_size = p
        .backup_chunk_size
        .as_deref()
        .map(parse_backup_chunk_size)
        .transpose()?;
    let backup_webdav_url = p
        .backup_webdav_url
        .as_deref()
        .map(|value| normalize_http_url(value, "WebDAV 目录 URL"))
        .transpose()?;
    let backup_webdav_username = p
        .backup_webdav_username
        .as_deref()
        .map(str::trim)
        .map(str::to_string);
    let backup_webdav_password = p
        .backup_webdav_password
        .as_deref()
        .filter(|value| *value != BACKUP_SECRET_PLACEHOLDER)
        .map(str::to_string);
    let backup_s3_endpoint = p
        .backup_s3_endpoint
        .as_deref()
        .map(|value| normalize_http_url(value, "S3 端点"))
        .transpose()?;
    let backup_s3_region = p
        .backup_s3_region
        .as_deref()
        .map(str::trim)
        .map(str::to_string);
    let backup_s3_bucket = p
        .backup_s3_bucket
        .as_deref()
        .map(str::trim)
        .map(str::to_string);
    let backup_s3_prefix = p
        .backup_s3_prefix
        .as_deref()
        .map(normalize_s3_prefix)
        .transpose()?;
    let backup_s3_access_key = p
        .backup_s3_access_key
        .as_deref()
        .filter(|value| *value != BACKUP_SECRET_PLACEHOLDER)
        .map(str::trim)
        .map(str::to_string);
    let backup_s3_secret_key = p
        .backup_s3_secret_key
        .as_deref()
        .filter(|value| *value != BACKUP_SECRET_PLACEHOLDER)
        .map(str::to_string);
    let backup_s3_session_token = p
        .backup_s3_session_token
        .as_deref()
        .filter(|value| *value != BACKUP_SECRET_PLACEHOLDER)
        .map(str::to_string);

    // 使用当前配置补齐未提交字段后整体校验，避免只改开关时绕过凭据完整性检查。
    let mut next = load(db).await?;
    if let Some(value) = p.backup_auto {
        next.backup_auto = value;
    }
    if let Some(value) = backup_chunk_size {
        next.backup_chunk_size = value;
    }
    if let Some(value) = p.backup_telegram_enabled {
        next.backup_telegram_enabled = value;
    }
    if let Some(value) = p.backup_webdav_enabled {
        next.backup_webdav_enabled = value;
    }
    if let Some(value) = &backup_webdav_url {
        next.backup_webdav_url.clone_from(value);
    }
    if let Some(value) = &backup_webdav_username {
        next.backup_webdav_username.clone_from(value);
    }
    if let Some(value) = &backup_webdav_password {
        next.backup_webdav_password.clone_from(value);
    }
    if let Some(value) = p.backup_s3_enabled {
        next.backup_s3_enabled = value;
    }
    if let Some(value) = &backup_s3_endpoint {
        next.backup_s3_endpoint.clone_from(value);
    }
    if let Some(value) = &backup_s3_region {
        next.backup_s3_region.clone_from(value);
    }
    if let Some(value) = &backup_s3_bucket {
        next.backup_s3_bucket.clone_from(value);
    }
    if let Some(value) = &backup_s3_prefix {
        next.backup_s3_prefix.clone_from(value);
    }
    if let Some(value) = &backup_s3_access_key {
        next.backup_s3_access_key.clone_from(value);
    }
    if let Some(value) = &backup_s3_secret_key {
        next.backup_s3_secret_key.clone_from(value);
    }
    if let Some(value) = &backup_s3_session_token {
        next.backup_s3_session_token.clone_from(value);
    }
    if let Some(value) = p.backup_s3_path_style {
        next.backup_s3_path_style = value;
    }
    validate_backup_destinations(&next)?;

    if let Some(d) = p.trash_retention_days {
        set(db, "trash_retention_days", &d.to_string()).await?;
    }
    if let Some(b) = p.backup_auto {
        set(db, "backup_auto", if b { "1" } else { "0" }).await?;
    }
    if let Some(size) = backup_chunk_size {
        set(db, "backup_chunk_size", &size.to_string()).await?;
    }
    if let Some(value) = p.backup_telegram_enabled {
        set(db, "backup_telegram_enabled", if value { "1" } else { "0" }).await?;
    }
    if let Some(value) = p.backup_webdav_enabled {
        set(db, "backup_webdav_enabled", if value { "1" } else { "0" }).await?;
    }
    if let Some(value) = backup_webdav_url {
        set(db, "backup_webdav_url", &value).await?;
    }
    if let Some(value) = backup_webdav_username {
        set(db, "backup_webdav_username", &value).await?;
    }
    if let Some(value) = backup_webdav_password {
        set(db, "backup_webdav_password", &value).await?;
    }
    if let Some(value) = p.backup_s3_enabled {
        set(db, "backup_s3_enabled", if value { "1" } else { "0" }).await?;
    }
    if let Some(value) = backup_s3_endpoint {
        set(db, "backup_s3_endpoint", &value).await?;
    }
    if let Some(value) = backup_s3_region {
        set(db, "backup_s3_region", &value).await?;
    }
    if let Some(value) = backup_s3_bucket {
        set(db, "backup_s3_bucket", &value).await?;
    }
    if let Some(value) = backup_s3_prefix {
        set(db, "backup_s3_prefix", &value).await?;
    }
    if let Some(value) = backup_s3_access_key {
        set(db, "backup_s3_access_key", &value).await?;
    }
    if let Some(value) = backup_s3_secret_key {
        set(db, "backup_s3_secret_key", &value).await?;
    }
    if let Some(value) = backup_s3_session_token {
        set(db, "backup_s3_session_token", &value).await?;
    }
    if let Some(value) = p.backup_s3_path_style {
        set(db, "backup_s3_path_style", if value { "1" } else { "0" }).await?;
    }
    if p.inbox_enabled.is_some()
        || p.inbox_bot_enabled.is_some()
        || p.inbox_channel_enabled.is_some()
    {
        let (bot, channel) = load_inbox_switches(db).await?;
        let bot = p.inbox_bot_enabled.or(p.inbox_enabled).unwrap_or(bot);
        let channel = p
            .inbox_channel_enabled
            .or(p.inbox_enabled)
            .unwrap_or(channel);
        // 同时固化两个新开关，关闭其中一个不会再通过旧总开关联动另一个。
        db.settings_apply(
            &[
                (
                    "extra.inbox_bot_enabled".into(),
                    if bot { "1" } else { "0" }.into(),
                ),
                (
                    "extra.inbox_channel_enabled".into(),
                    if channel { "1" } else { "0" }.into(),
                ),
            ],
            &["extra.inbox_enabled".into()],
        )
        .await?;
    }
    if let Some(v) = inbox_allowed_chats {
        set(db, "inbox_allowed_chats", v).await?;
    }
    if let Some(v) = inbox_dir {
        set(db, "inbox_dir", &v).await?;
    }
    if let Some(v) = inbox_video_dir {
        set(db, "inbox_video_dir", &v).await?;
    }
    if let Some(v) = inbox_image_dir {
        set(db, "inbox_image_dir", &v).await?;
    }
    if let Some(v) = inbox_book_dir {
        set(db, "inbox_book_dir", &v).await?;
    }
    if let Some(rules) = inbox_rules {
        let raw = serde_json::to_string(&rules)
            .map_err(|e| AppError::BadRequest(format!("自定义收件规则序列化失败:{e}")))?;
        set(db, "inbox_rules", &raw).await?;
    }
    if let Some(v) = p.auto_delete_message {
        set(db, "auto_delete_message", if v { "1" } else { "0" }).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_rules_are_normalized_and_conflicts_are_rejected() {
        let rules = normalize_rules(&[InboxRouteRule {
            name: " 音频 ".into(),
            extensions: vec![".MP3, flac".into(), "mp3".into()],
            dir: "媒体/音频/".into(),
        }])
        .unwrap();
        assert_eq!(rules[0].name, "音频");
        assert_eq!(rules[0].extensions, ["mp3", "flac"]);
        assert_eq!(rules[0].dir, "/媒体/音频");

        let conflict = normalize_rules(&[
            InboxRouteRule {
                name: "一".into(),
                extensions: vec!["zip".into()],
                dir: "/一".into(),
            },
            InboxRouteRule {
                name: "二".into(),
                extensions: vec!["ZIP".into()],
                dir: "/二".into(),
            },
        ]);
        assert!(conflict.is_err());
    }

    #[test]
    fn backup_chunk_size_uses_official_default_and_validates_range() {
        assert_eq!(DEFAULT_BACKUP_CHUNK_SIZE, 18 * 1024 * 1024);
        assert_eq!(parse_backup_chunk_size("8MB").unwrap(), 8 * 1024 * 1024);
        assert!(parse_backup_chunk_size("512KB").is_err());
        assert!(
            validate_backup_chunk_size(crate::config::OFFICIAL_API_BASE, 25 * 1024 * 1024).is_err()
        );
        assert!(
            validate_backup_chunk_size("http://telegram-bot-api:8081", 512 * 1024 * 1024).is_ok()
        );
    }

    #[tokio::test]
    async fn inbox_switches_upgrade_and_save_independently() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("switches.db").to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (false, false));

        db.setting_set("extra.inbox_enabled", "1").await.unwrap();
        let old = load(&db).await.unwrap();
        assert!(old.inbox_bot_enabled && old.inbox_channel_enabled);
        save(
            &db,
            &ExtrasPatch {
                inbox_bot_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (false, true));
        assert!(
            db.setting_get("extra.inbox_enabled")
                .await
                .unwrap()
                .is_none()
        );

        save(
            &db,
            &ExtrasPatch {
                inbox_channel_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (false, false));
        save(
            &db,
            &ExtrasPatch {
                inbox_bot_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (true, false));

        // 旧客户端仍可提交总开关，新客户端显式提交的独立值优先。
        save(
            &db,
            &ExtrasPatch {
                inbox_enabled: Some(true),
                inbox_channel_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (true, false));
        save(
            &db,
            &ExtrasPatch {
                inbox_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(load_inbox_switches(&db).await.unwrap(), (true, true));
    }

    #[tokio::test]
    async fn inbox_routing_settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("extras.db").to_str().unwrap())
            .await
            .unwrap();

        // 新增分类目录没有历史配置时，应跟随原有默认收件目录派生。
        save(
            &db,
            &ExtrasPatch {
                inbox_dir: Some("/所有".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let defaults = load(&db).await.unwrap();
        assert_eq!(defaults.backup_chunk_size, DEFAULT_BACKUP_CHUNK_SIZE);
        assert_eq!(defaults.inbox_video_dir, "/所有/视频");
        assert_eq!(defaults.inbox_image_dir, "/所有/图片");
        assert_eq!(defaults.inbox_book_dir, "/所有/书籍");

        save(
            &db,
            &ExtrasPatch {
                inbox_dir: Some("/所有".into()),
                backup_chunk_size: Some("8MB".into()),
                inbox_video_dir: Some("/分类/视频".into()),
                inbox_image_dir: Some("/分类/图片".into()),
                inbox_book_dir: Some("/分类/书籍".into()),
                inbox_rules: Some(vec![InboxRouteRule {
                    name: "代码".into(),
                    extensions: vec!["RS".into(), ".ts".into()],
                    dir: "/项目/源码".into(),
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let got = load(&db).await.unwrap();
        assert_eq!(got.backup_chunk_size, 8 * 1024 * 1024);
        assert_eq!(got.inbox_dir, "/所有");
        assert_eq!(got.inbox_video_dir, "/分类/视频");
        assert_eq!(got.inbox_image_dir, "/分类/图片");
        assert_eq!(got.inbox_book_dir, "/分类/书籍");
        assert_eq!(got.inbox_rules[0].extensions, ["rs", "ts"]);
    }

    #[tokio::test]
    async fn invalid_rule_does_not_partially_save_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("atomic.db").to_str().unwrap())
            .await
            .unwrap();
        save(
            &db,
            &ExtrasPatch {
                inbox_dir: Some("/原目录".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let result = save(
            &db,
            &ExtrasPatch {
                inbox_dir: Some("/不应保存".into()),
                inbox_rules: Some(vec![
                    InboxRouteRule {
                        name: "压缩一".into(),
                        extensions: vec!["zip".into()],
                        dir: "/一".into(),
                    },
                    InboxRouteRule {
                        name: "压缩二".into(),
                        extensions: vec!["ZIP".into()],
                        dir: "/二".into(),
                    },
                ]),
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(load(&db).await.unwrap().inbox_dir, "/原目录");
    }

    #[tokio::test]
    async fn backup_credentials_are_masked_and_placeholders_preserve_saved_values() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(
            directory
                .path()
                .join("backup-settings.db")
                .to_str()
                .unwrap(),
        )
        .await
        .unwrap();
        save(
            &db,
            &ExtrasPatch {
                backup_webdav_enabled: Some(true),
                backup_webdav_url: Some("https://dav.example.com/backups/".into()),
                backup_webdav_username: Some("alice".into()),
                backup_webdav_password: Some("dav-secret".into()),
                backup_s3_enabled: Some(true),
                backup_s3_endpoint: Some("https://s3.example.com/".into()),
                backup_s3_region: Some("auto".into()),
                backup_s3_bucket: Some("db-backups".into()),
                backup_s3_prefix: Some("/tgdrive/daily/".into()),
                backup_s3_access_key: Some("ACCESS".into()),
                backup_s3_secret_key: Some("s3-secret".into()),
                backup_s3_session_token: Some("session-token".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let loaded = load(&db).await.unwrap();
        assert_eq!(loaded.backup_webdav_url, "https://dav.example.com/backups");
        assert_eq!(loaded.backup_s3_prefix, "tgdrive/daily");
        assert_eq!(loaded.backup_webdav_password, "dav-secret");
        let public = serde_json::to_value(&loaded).unwrap();
        assert_eq!(public["backup_webdav_password"], BACKUP_SECRET_PLACEHOLDER);
        assert_eq!(public["backup_s3_secret_key"], BACKUP_SECRET_PLACEHOLDER);
        assert_eq!(public["backup_s3_access_key"], BACKUP_SECRET_PLACEHOLDER);
        assert_eq!(public["backup_s3_session_token"], BACKUP_SECRET_PLACEHOLDER);

        save(
            &db,
            &ExtrasPatch {
                backup_webdav_password: Some(BACKUP_SECRET_PLACEHOLDER.into()),
                backup_s3_access_key: Some(BACKUP_SECRET_PLACEHOLDER.into()),
                backup_s3_secret_key: Some(BACKUP_SECRET_PLACEHOLDER.into()),
                backup_s3_session_token: Some(BACKUP_SECRET_PLACEHOLDER.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let preserved = load(&db).await.unwrap();
        assert_eq!(preserved.backup_webdav_password, "dav-secret");
        assert_eq!(preserved.backup_s3_access_key, "ACCESS");
        assert_eq!(preserved.backup_s3_secret_key, "s3-secret");
        assert_eq!(preserved.backup_s3_session_token, "session-token");
    }

    #[tokio::test]
    async fn automatic_backup_requires_at_least_one_destination() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(
            directory
                .path()
                .join("backup-validation.db")
                .to_str()
                .unwrap(),
        )
        .await
        .unwrap();
        let result = save(
            &db,
            &ExtrasPatch {
                backup_auto: Some(true),
                backup_telegram_enabled: Some(false),
                backup_webdav_enabled: Some(false),
                backup_s3_enabled: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err());
        let unchanged = load(&db).await.unwrap();
        assert!(!unchanged.backup_auto);
        assert!(unchanged.backup_telegram_enabled);
    }
}
