//! 维护类可调项(回收站保留天数 / 自动备份 / Bot 收件),存 settings 表 `extra.*` 键。
//! 与 `cfg.*` 配置体系分开:这些项没有环境变量层,纯后台开关。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const DEFAULT_TRASH_DAYS: i64 = 30;
pub const DEFAULT_INBOX_DIR: &str = "/收件箱";
/// 数据库备份默认按官方 Bot API 可安全下载的大小分块。
pub const DEFAULT_BACKUP_CHUNK_SIZE: u64 = crate::config::OFFICIAL_CHUNK_DEFAULT;
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
    /// 每日自动把数据库快照发到存储频道
    pub backup_auto: bool,
    /// 数据库快照上传到 Telegram 时的单块大小
    pub backup_chunk_size: u64,
    /// Bot 收件(频道直发 + 私聊白名单转存)
    pub inbox_enabled: bool,
    /// 允许私聊收件的 chat id,逗号分隔;空 = 只收存储频道内直发
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
}

async fn get(db: &Db, k: &str) -> AppResult<Option<String>> {
    db.setting_get(&format!("extra.{k}")).await
}

async fn set(db: &Db, k: &str, v: &str) -> AppResult<()> {
    db.setting_set(&format!("extra.{k}"), v).await
}

pub async fn load(db: &Db) -> AppResult<Extras> {
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
        inbox_enabled: get(db, "inbox_enabled").await?.as_deref() == Some("1"),
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
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ExtrasPatch {
    pub trash_retention_days: Option<i64>,
    pub backup_auto: Option<bool>,
    /// 字符串形式便于后台使用 `18MB`、`1GB` 等写法。
    pub backup_chunk_size: Option<String>,
    pub inbox_enabled: Option<bool>,
    pub inbox_allowed_chats: Option<String>,
    pub inbox_dir: Option<String>,
    pub inbox_video_dir: Option<String>,
    pub inbox_image_dir: Option<String>,
    pub inbox_book_dir: Option<String>,
    pub inbox_rules: Option<Vec<InboxRouteRule>>,
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

    if let Some(d) = p.trash_retention_days {
        set(db, "trash_retention_days", &d.to_string()).await?;
    }
    if let Some(b) = p.backup_auto {
        set(db, "backup_auto", if b { "1" } else { "0" }).await?;
    }
    if let Some(size) = backup_chunk_size {
        set(db, "backup_chunk_size", &size.to_string()).await?;
    }
    if let Some(b) = p.inbox_enabled {
        set(db, "inbox_enabled", if b { "1" } else { "0" }).await?;
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
}
