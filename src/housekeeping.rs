//! 定期维护任务:回收站超期清理 + 每日自动备份数据库到存储频道。
//! 每 30 分钟检查一次;备份保留最近 7 份,更早的消息尽力删除。

use crate::db::{StoredMessage, now_ts};
use crate::error::{AppError, AppResult};
use crate::settings::Runtime;
use crate::telegram::UploadBody;
use crate::vfs::Vfs;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30 * 60);
const BACKUP_INTERVAL_SECS: i64 = 86400;
const BACKUP_KEEP: usize = 7;
const UPLOAD_SESSION_KEEP_SECS: i64 = 7 * 86400;
const BACKUP_HISTORY_KEY: &str = "extra.backup_msg_groups";
const LEGACY_BACKUP_HISTORY_KEY: &str = "extra.backup_msg_ids";

/// 一次数据库备份可能对应多条 Telegram 分块消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupRecord {
    name: String,
    messages: Vec<StoredMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupOutcome {
    pub name: String,
    pub parts: u64,
}

pub async fn run(vfs: Vfs, rt: Arc<Runtime>, task: crate::tasks::TaskReporter) {
    loop {
        if task.is_cancelled() {
            return;
        }
        task.heartbeat();
        match tick(&vfs, &rt).await {
            Ok(()) => task.clear_error(),
            Err(e) => {
                task.report_error(&e.to_string());
                tracing::warn!("维护任务出错(下轮重试): {e}");
            }
        }
        if !task.sleep(TICK).await {
            return;
        }
    }
}

async fn tick(vfs: &Vfs, rt: &Runtime) -> AppResult<()> {
    vfs.uploader.process_pending_deletes();
    let ex = crate::extras::load(&vfs.db).await?;

    let abandoned = vfs
        .db
        .cleanup_expired_upload_sessions(now_ts() - UPLOAD_SESSION_KEEP_SECS)
        .await?;
    if !abandoned.is_empty() {
        tracing::info!("清理过期断点续传任务：释放 {} 条消息", abandoned.len());
        vfs.uploader.delete_messages_best_effort(abandoned);
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
            let backup = backup_to_channel(vfs, rt).await?;
            tracing::info!("自动备份完成: {}（{} 个分块）", backup.name, backup.parts);
        }
    }
    Ok(())
}

/// 把数据库一致性快照分块发到存储频道（不入虚拟目录树）。
/// 单块备份可直接恢复；多块备份按文件名序号二进制合并后恢复。
pub async fn backup_to_channel(vfs: &Vfs, rt: &Runtime) -> AppResult<BackupOutcome> {
    let cfg = rt.current();
    if !cfg.is_configured() {
        return Err(AppError::BadRequest(
            "还没配置 Telegram 存储,无法备份到频道".into(),
        ));
    }
    let target_id = cfg
        .storage_target_id
        .ok_or_else(|| AppError::BadRequest("当前 Telegram 配置缺少存储目标".into()))?;
    let target = vfs.tg.binding(target_id)?;
    let extras = crate::extras::load(&vfs.db).await?;
    crate::extras::validate_backup_chunk_size(&cfg.api_base, extras.backup_chunk_size)?;
    let ts = now_ts();
    let temp_dir = tempfile::Builder::new()
        .prefix("tgdrive-backup-")
        .tempdir_in(&cfg.tmp_dir)?;
    let tmp = temp_dir.path().join("snapshot.db");
    let tmp_str = tmp
        .to_str()
        .ok_or_else(|| AppError::BadRequest("临时目录路径无效".into()))?;
    vfs.db.vacuum_into(tmp_str).await?;
    let len = std::fs::metadata(&tmp)?.len();
    let name = format!("tgdrive-backup-{}.db", fmt_compact_utc8(ts));
    let part_count = len.div_ceil(extras.backup_chunk_size).max(1);
    let mut uploaded = Vec::with_capacity(part_count as usize);

    for part_index in 0..part_count {
        let offset = part_index * extras.backup_chunk_size;
        let part_len = (len - offset).min(extras.backup_chunk_size);
        let body_file =
            match create_backup_part(tmp.clone(), cfg.tmp_dir.clone(), offset, part_len).await {
                Ok(file) => file,
                Err(error) => {
                    enqueue_cleanup(vfs, &uploaded).await;
                    return Err(error);
                }
            };
        let filename = backup_part_filename(&name, part_index, part_count);
        let caption = backup_part_caption(part_index, part_count);
        let body = UploadBody::TempFile {
            file: body_file,
            len: part_len,
        };
        let msg = match target
            .api
            .send_document(&filename, Some(caption), None, &body)
            .await
        {
            Ok(msg) => msg,
            Err(error) => {
                enqueue_cleanup(vfs, &uploaded).await;
                return Err(error);
            }
        };
        uploaded.push(StoredMessage {
            storage_target_id: target_id,
            message_id: msg.message_id,
        });
    }

    // 滚动保留最近 N 组备份；一组中的所有分块始终一起保留或清理。
    let mut history = match load_backup_history(&vfs.db).await {
        Ok(history) => history,
        Err(error) => {
            enqueue_cleanup(vfs, &uploaded).await;
            return Err(error);
        }
    };
    let remove_count = history.len().saturating_add(1).saturating_sub(BACKUP_KEEP);
    let expired: Vec<BackupRecord> = history.drain(..remove_count).collect();
    for record in &expired {
        for message in &record.messages {
            if let Err(error) = vfs.db.enqueue_delete_message(*message).await {
                enqueue_cleanup(vfs, &uploaded).await;
                return Err(error);
            }
        }
    }
    history.push(BackupRecord {
        name: name.clone(),
        messages: uploaded.clone(),
    });
    if let Err(error) = vfs
        .db
        .setting_set(
            BACKUP_HISTORY_KEY,
            &serde_json::to_string(&history).map_err(|error| AppError::Internal(error.into()))?,
        )
        .await
    {
        tracing::warn!("保存数据库备份分块记录失败，将清理刚上传的消息");
        enqueue_cleanup(vfs, &uploaded).await;
        return Err(error);
    }
    // 新格式已成功保存后再移除旧键；旧版本每条消息就是一份完整备份。
    if let Err(error) = vfs.db.setting_del(LEGACY_BACKUP_HISTORY_KEY).await {
        tracing::warn!("移除旧版数据库备份记录失败，下次将继续使用新记录：{error}");
    }
    vfs.db
        .setting_set("extra.backup_last_ts", &ts.to_string())
        .await?;
    vfs.uploader.process_pending_deletes();
    Ok(BackupOutcome {
        name,
        parts: part_count,
    })
}

/// 从快照中复制一个分块到独立临时文件，供 Telegram 上传重试时重新打开。
async fn create_backup_part(
    snapshot: PathBuf,
    temp_dir: PathBuf,
    offset: u64,
    len: u64,
) -> AppResult<tempfile::NamedTempFile> {
    tokio::task::spawn_blocking(move || -> std::io::Result<tempfile::NamedTempFile> {
        let mut source = std::fs::File::open(snapshot)?;
        source.seek(SeekFrom::Start(offset))?;
        let mut part = tempfile::Builder::new()
            .prefix("tgdrive-backup-part-")
            .tempfile_in(temp_dir)?;
        let copied = std::io::copy(&mut source.take(len), part.as_file_mut())?;
        if copied != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("数据库备份分块应为 {len} 字节，实际复制 {copied} 字节"),
            ));
        }
        Ok(part)
    })
    .await
    .map_err(|error| AppError::Internal(anyhow::anyhow!("数据库备份分块任务失败：{error}")))?
    .map_err(AppError::from)
}

fn backup_part_filename(base: &str, part_index: u64, part_count: u64) -> String {
    if part_count == 1 {
        base.to_string()
    } else {
        format!("{base}.part{:04}-of-{part_count:04}", part_index + 1)
    }
}

fn backup_part_caption(part_index: u64, part_count: u64) -> String {
    if part_count == 1 {
        "tgdrive 数据库备份（含配置与文件索引，请勿删除最近几份）".into()
    } else {
        format!(
            "tgdrive 数据库备份分块 {}/{}（按文件名顺序二进制合并后恢复）",
            part_index + 1,
            part_count
        )
    }
}

async fn load_backup_history(db: &crate::db::Db) -> AppResult<Vec<BackupRecord>> {
    if let Some(raw) = db.setting_get(BACKUP_HISTORY_KEY).await? {
        return Ok(serde_json::from_str(&raw).unwrap_or_else(|error| {
            tracing::warn!("数据库备份分组记录损坏，为避免误删将忽略旧记录：{error}");
            Vec::new()
        }));
    }
    let Some(raw) = db.setting_get(LEGACY_BACKUP_HISTORY_KEY).await? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str::<Vec<StoredMessage>>(&raw)
        .unwrap_or_else(|error| {
            tracing::warn!("旧版数据库备份记录损坏，为避免误删将忽略：{error}");
            Vec::new()
        })
        .into_iter()
        .map(|message| BackupRecord {
            name: "旧版数据库备份".into(),
            messages: vec![message],
        })
        .collect())
}

/// 上传中途失败时，把已经成功发送且可定位的分块加入持久化删除队列。
async fn enqueue_cleanup(vfs: &Vfs, messages: &[StoredMessage]) {
    for message in messages {
        if let Err(error) = vfs.db.enqueue_delete_message(*message).await {
            tracing::error!(
                "清理失败数据库备份的消息 {} 时无法写入删除队列：{error}",
                message.message_id
            );
        }
    }
    vfs.uploader.process_pending_deletes();
}

/// unix 秒 → "YYYYMMDD-HHMM"(东八区),备份文件名用
fn fmt_compact_utc8(ts: i64) -> String {
    let t = ts + 8 * 3600;
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let secs = t.rem_euclid(86400);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
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

    #[test]
    fn backup_part_names_are_stable_and_sortable() {
        let base = "tgdrive-backup-20260816-1200.db";
        assert_eq!(backup_part_filename(base, 0, 1), base);
        assert_eq!(
            backup_part_filename(base, 0, 12),
            "tgdrive-backup-20260816-1200.db.part0001-of-0012"
        );
        assert_eq!(
            backup_part_filename(base, 11, 12),
            "tgdrive-backup-20260816-1200.db.part0012-of-0012"
        );
    }

    #[tokio::test]
    async fn backup_part_copies_only_requested_range() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot.db");
        std::fs::write(&snapshot, b"0123456789").unwrap();

        let part = create_backup_part(snapshot, dir.path().to_path_buf(), 3, 4)
            .await
            .unwrap();
        assert_eq!(std::fs::read(part.path()).unwrap(), b"3456");
    }

    #[tokio::test]
    async fn legacy_backup_messages_become_single_part_groups() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(dir.path().join("history.db").to_str().unwrap())
            .await
            .unwrap();
        let legacy = vec![
            StoredMessage {
                storage_target_id: 1,
                message_id: 10,
            },
            StoredMessage {
                storage_target_id: 2,
                message_id: 20,
            },
        ];
        db.setting_set(
            LEGACY_BACKUP_HISTORY_KEY,
            &serde_json::to_string(&legacy).unwrap(),
        )
        .await
        .unwrap();

        let history = load_backup_history(&db).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].messages, [legacy[0]]);
        assert_eq!(history[1].messages, [legacy[1]]);
    }
}
