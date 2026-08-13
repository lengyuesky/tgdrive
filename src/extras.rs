//! 维护类可调项(回收站保留天数 / 自动备份 / Bot 收件),存 settings 表 `extra.*` 键。
//! 与 `cfg.*` 配置体系分开:这些项没有环境变量层,纯后台开关。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TRASH_DAYS: i64 = 30;
pub const DEFAULT_INBOX_DIR: &str = "/收件箱";

#[derive(Debug, Clone, Serialize)]
pub struct Extras {
    /// 回收站保留天数;0 = 永不自动清理
    pub trash_retention_days: i64,
    /// 每日自动把数据库快照发到存储频道
    pub backup_auto: bool,
    /// Bot 收件(频道直发 + 私聊白名单转存)
    pub inbox_enabled: bool,
    /// 允许私聊收件的 chat id,逗号分隔;空 = 只收存储频道内直发
    pub inbox_allowed_chats: String,
    /// 收到的文件放到哪个目录
    pub inbox_dir: String,
}

async fn get(db: &Db, k: &str) -> AppResult<Option<String>> {
    db.setting_get(&format!("extra.{k}")).await
}

async fn set(db: &Db, k: &str, v: &str) -> AppResult<()> {
    db.setting_set(&format!("extra.{k}"), v).await
}

pub async fn load(db: &Db) -> AppResult<Extras> {
    Ok(Extras {
        trash_retention_days: get(db, "trash_retention_days")
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TRASH_DAYS),
        backup_auto: get(db, "backup_auto").await?.as_deref() == Some("1"),
        inbox_enabled: get(db, "inbox_enabled").await?.as_deref() == Some("1"),
        inbox_allowed_chats: get(db, "inbox_allowed_chats").await?.unwrap_or_default(),
        inbox_dir: get(db, "inbox_dir")
            .await?
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_INBOX_DIR.into()),
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ExtrasPatch {
    pub trash_retention_days: Option<i64>,
    pub backup_auto: Option<bool>,
    pub inbox_enabled: Option<bool>,
    pub inbox_allowed_chats: Option<String>,
    pub inbox_dir: Option<String>,
}

pub async fn save(db: &Db, p: &ExtrasPatch) -> AppResult<()> {
    if let Some(d) = p.trash_retention_days {
        if !(0..=3650).contains(&d) {
            return Err(AppError::BadRequest("保留天数需在 0~3650 之间(0 为永不清理)".into()));
        }
        set(db, "trash_retention_days", &d.to_string()).await?;
    }
    if let Some(b) = p.backup_auto {
        set(db, "backup_auto", if b { "1" } else { "0" }).await?;
    }
    if let Some(b) = p.inbox_enabled {
        set(db, "inbox_enabled", if b { "1" } else { "0" }).await?;
    }
    if let Some(v) = &p.inbox_allowed_chats {
        // 只允许数字(可带负号)的 chat id 列表
        for part in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if part.parse::<i64>().is_err() {
                return Err(AppError::BadRequest(format!(
                    "白名单应为数字 chat id(逗号分隔),「{part}」无效"
                )));
            }
        }
        set(db, "inbox_allowed_chats", v.trim()).await?;
    }
    if let Some(v) = &p.inbox_dir {
        let segs = crate::vfs::normalize(v)?;
        if segs.is_empty() {
            return Err(AppError::BadRequest("收件目录不能是根目录".into()));
        }
        set(db, "inbox_dir", &format!("/{}", segs.join("/"))).await?;
    }
    Ok(())
}
