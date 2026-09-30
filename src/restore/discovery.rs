use super::*;
use crate::db::StoredMessage;
use crate::telegram::TgHistoryDocument;
use std::collections::BTreeMap;

const BACKUP_LIMIT: usize = 5;

#[derive(Default)]
pub(super) struct Discovery {
    target_id: Option<i64>,
    next_before: Option<i32>,
    scanned: usize,
    documents: BTreeMap<i32, TgHistoryDocument>,
}

#[derive(Serialize)]
pub struct DiscoveryResult {
    backups: Vec<HistoryItem>,
    scanned: usize,
    has_more: bool,
}

pub(super) fn record_id(record: &TelegramBackupRecord) -> String {
    format!("channel:{}", history_id(record))
}

impl Discovery {
    fn groups(&self) -> Vec<(TelegramBackupRecord, Vec<Part>)> {
        let mut groups: BTreeMap<String, (TelegramBackupRecord, Vec<Part>)> = BTreeMap::new();
        for document in self.documents.values() {
            let Ok(part) = parse_part(&document.file_name) else {
                continue;
            };
            let (record, parts) = groups.entry(part.base.clone()).or_insert_with(|| {
                (
                    TelegramBackupRecord {
                        name: part.base.clone(),
                        messages: Vec::new(),
                    },
                    Vec::new(),
                )
            });
            record.messages.push(StoredMessage {
                storage_target_id: self.target_id.unwrap(),
                message_id: i64::from(document.message_id),
            });
            parts.push(part);
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by_key(|(record, _)| {
            std::cmp::Reverse(record.messages.last().unwrap().message_id)
        });
        groups
    }

    fn result(&self) -> DiscoveryResult {
        let mut backups: Vec<_> = self
            .groups()
            .into_iter()
            .map(|(record, parts)| {
                let error = validate_parts(&parts).err().map(|e| e.to_string());
                HistoryItem {
                    id: record_id(&record),
                    name: record.name,
                    parts: parts.len(),
                    available: error.is_none(),
                    error,
                }
            })
            .collect();
        let complete = backups.iter().filter(|item| item.available).count();
        // 完整备份优先，各自保持从新到旧；尚未完整的分块不占用完成额度。
        backups.sort_by_key(|item| !item.available);
        backups.truncate(BACKUP_LIMIT);
        DiscoveryResult {
            backups,
            scanned: self.scanned,
            has_more: self.next_before.is_some() && complete < BACKUP_LIMIT,
        }
    }
}

impl RestoreManager {
    pub async fn discover(&self, state: &AppState, reset: bool) -> AppResult<DiscoveryResult> {
        if self.maintenance() {
            return Err(AppError::Conflict("恢复重启中，请稍后探测".into()));
        }
        let mut cache = self
            .discovery
            .try_lock()
            .map_err(|_| AppError::Conflict("频道探测正在进行，请稍候".into()))?;
        let cfg = state.rt.current();
        let target_id = cfg.storage_target_id.ok_or_else(|| {
            bad("请先在 .env 配置 TGDRIVE_BOT_TOKEN 和 TGDRIVE_CHAT_ID，或在后台保存 Telegram 配置")
        })?;
        let binding = state.vfs.tg.binding(target_id)?;
        if reset || cache.target_id != Some(target_id) {
            *cache = Discovery::default();
        } else if cache.target_id.is_some() && !cache.result().has_more {
            return Ok(cache.result());
        }
        let page = binding.api.backup_history_page(cache.next_before).await?;
        // 每页整体成功后才前移游标，超时/限流不会漏掉这一页。
        cache.target_id = Some(target_id);
        cache.next_before = page.next_before;
        cache.scanned += page.scanned;
        for document in page.documents {
            cache.documents.insert(document.message_id, document);
        }
        Ok(cache.result())
    }

    pub(super) async fn discovered_records(
        &self,
        state: &AppState,
    ) -> AppResult<Vec<TelegramBackupRecord>> {
        let cache = self.discovery.lock().await;
        if cache.target_id.is_none() || cache.target_id != state.rt.current().storage_target_id {
            return Err(bad("频道配置已变化或探测结果已过期，请重新探测"));
        }
        Ok(cache
            .groups()
            .into_iter()
            .filter(|(_, parts)| validate_parts(parts).is_ok())
            .take(BACKUP_LIMIT)
            .map(|(record, _)| record)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_missing_and_mixed_parts_but_accepts_legacy_single_file() {
        let mut cache = Discovery {
            target_id: Some(1),
            ..Default::default()
        };
        for (id, name) in [
            (1, "tgdrive-backup-old.db"),
            (4, "tgdrive-backup-missing.db.part0002-of-0002"),
            (7, "tgdrive-backup-duplicate.db.part0001-of-0002"),
            (8, "tgdrive-backup-duplicate.db.part0001-of-0002"),
            (9, "tgdrive-backup-mixed.db.part0001-of-0002"),
            (10, "tgdrive-backup-mixed.db.part0002-of-0003"),
            (11, "tgdrive-backup-malformed.db.part0000-of-0002"),
        ] {
            cache.documents.insert(
                id,
                TgHistoryDocument {
                    message_id: id,
                    file_name: name.into(),
                },
            );
        }
        let result = cache.result();
        assert_eq!(result.backups.len(), 4);
        assert_eq!(result.backups.iter().filter(|b| b.available).count(), 1);
        assert_eq!(result.backups[0].name, "tgdrive-backup-old.db");
        assert!(result.backups[0].available);
    }
}
