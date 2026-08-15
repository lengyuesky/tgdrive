//! 定期维护任务:回收站超期清理 + 每日自动备份数据库到存储频道。
//! 每 30 分钟检查一次;备份保留最近 7 份,更早的消息尽力删除。

use crate::db::now_ts;
use crate::error::{AppError, AppResult};
use crate::settings::Runtime;
use crate::telegram::UploadBody;
use crate::vfs::Vfs;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30 * 60);
const BACKUP_INTERVAL_SECS: i64 = 86400;
const BACKUP_KEEP: usize = 7;

pub async fn run(vfs: Vfs, rt: Arc<Runtime>) {
    loop {
        if let Err(e) = tick(&vfs, &rt).await {
            tracing::warn!("维护任务出错(下轮重试): {e}");
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn tick(vfs: &Vfs, rt: &Runtime) -> AppResult<()> {
    let ex = crate::extras::load(&vfs.db).await?;

    let removed_thumbnails = vfs.db.cleanup_orphan_thumbnails().await?;
    if removed_thumbnails > 0 {
        tracing::info!("清理 {removed_thumbnails} 个无文件引用的视频缩略图");
    }

    if ex.trash_retention_days > 0 {
        let freed = vfs.db.trash_purge_expired(ex.trash_retention_days).await?;
        if !freed.is_empty() {
            tracing::info!("回收站超期清理:释放 {} 条消息", freed.len());
            vfs.uploader.delete_messages_best_effort(freed);
        }
    }

    if ex.backup_auto && rt.current().is_configured() {
        let last: i64 = vfs
            .db
            .setting_get("extra.backup_last_ts")
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now_ts() - last >= BACKUP_INTERVAL_SECS {
            let name = backup_to_channel(vfs, rt).await?;
            tracing::info!("自动备份完成: {name}");
        }
    }
    Ok(())
}

/// 把数据库一致性快照作为文件发到存储频道(不入虚拟目录树),
/// 元数据库丢失时可直接从频道找回。返回备份文件名。
pub async fn backup_to_channel(vfs: &Vfs, rt: &Runtime) -> AppResult<String> {
    let cfg = rt.current();
    if !cfg.is_configured() {
        return Err(AppError::BadRequest("还没配置 Telegram 存储,无法备份到频道".into()));
    }
    let ts = now_ts();
    let tmp = cfg.tmp_dir.join(format!("tgdrive-backup-{ts}.tmp.db"));
    let tmp_str =
        tmp.to_str().ok_or_else(|| AppError::BadRequest("临时目录路径无效".into()))?;
    vfs.db.vacuum_into(tmp_str).await?;
    let data = tokio::fs::read(&tmp).await?;
    let _ = std::fs::remove_file(&tmp);

    let name = format!("tgdrive-backup-{}.db", fmt_compact_utc8(ts));
    let msg = vfs
        .tg
        .send_document(
            &name,
            Some("tgdrive 数据库备份(含配置与文件索引,勿删最近几份)".into()),
            &UploadBody::Memory(bytes::Bytes::from(data)),
        )
        .await?;

    // 滚动保留最近 N 份
    let mut ids: Vec<i64> = vfs
        .db
        .setting_get("extra.backup_msg_ids")
        .await?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    ids.push(msg.message_id);
    while ids.len() > BACKUP_KEEP {
        let old = ids.remove(0);
        if let Err(e) = vfs.tg.delete_message(old).await {
            tracing::warn!("删除旧备份消息 {old} 失败(可手动清理): {e}");
        }
    }
    vfs.db
        .setting_set(
            "extra.backup_msg_ids",
            &serde_json::to_string(&ids).map_err(|e| AppError::Internal(e.into()))?,
        )
        .await?;
    vfs.db.setting_set("extra.backup_last_ts", &ts.to_string()).await?;
    Ok(name)
}

/// unix 秒 → "YYYYMMDD-HHMM"(东八区),备份文件名用
fn fmt_compact_utc8(ts: i64) -> String {
    let t = ts + 8 * 3600;
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let secs = t.rem_euclid(86400);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}", secs / 3600, (secs % 3600) / 60)
}

/// 天数(自 1970-01-01) → 公历年月日;Howard Hinnant 的 civil_from_days
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_949), (2024, 8, 14));
        // 2026-08-12 00:00 UTC = 1786492800
        assert_eq!(fmt_compact_utc8(1_786_492_800), "20260812-0800");
    }
}
