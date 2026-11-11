-- 先保存收件请求，用户确认后才产生 Telegram 副本；操作结果与网盘登记原子提交。
CREATE TABLE inbox_requests (
    id TEXT PRIMARY KEY,
    bot_id TEXT NOT NULL,
    origin_target_id INTEGER REFERENCES storage_targets(id),
    chat_id INTEGER NOT NULL,
    source_message_id INTEGER NOT NULL,
    media_kind TEXT NOT NULL,
    original_name TEXT NOT NULL,
    original_suffix TEXT NOT NULL,
    name TEXT NOT NULL,
    file_id TEXT NOT NULL,
    file_size INTEGER NOT NULL CHECK(file_size > 0),
    thumbnail_file_id TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN
        ('pending','renaming','queued','copying','copied','failed','uncertain','stored','cancelled','expired','invalidated')),
    selected_dir_id INTEGER,
    selected_path TEXT,
    operation_message_id INTEGER,
    rename_prompt_id INTEGER,
    storage_target_id INTEGER REFERENCES storage_targets(id),
    copied_message_id INTEGER,
    copied_file_id TEXT,
    copied_size INTEGER,
    copied_thumbnail_file_id TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    config_revision INTEGER NOT NULL DEFAULT 0,
    copy_started_at INTEGER,
    node_id INTEGER,
    result_path TEXT,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    ended_at INTEGER,
    notice_pending INTEGER NOT NULL DEFAULT 1,
    notice_after INTEGER NOT NULL DEFAULT 0,
    notice_attempts INTEGER NOT NULL DEFAULT 0,
    UNIQUE(bot_id, chat_id, source_message_id)
);
CREATE INDEX idx_inbox_requests_work ON inbox_requests(bot_id, status, updated_at, id);
CREATE INDEX idx_inbox_requests_quota ON inbox_requests(ended_at, bot_id, chat_id);
CREATE INDEX idx_inbox_requests_history ON inbox_requests(created_at DESC, id DESC);
CREATE INDEX idx_inbox_requests_notice ON inbox_requests(bot_id, notice_pending, notice_after);
CREATE INDEX idx_inbox_requests_reply ON inbox_requests(bot_id, chat_id, rename_prompt_id);

-- 按 Bot 隔离的游标与内部提交版本不触发自动备份；目录配置本身仍算实质变化。
DROP TRIGGER data_revision_settings_insert;
DROP TRIGGER data_revision_settings_update;
DROP TRIGGER data_revision_settings_delete;
CREATE TRIGGER data_revision_settings_insert AFTER INSERT ON settings
WHEN NEW.k NOT IN (
    'extra.backup_telegram_last_ts', 'extra.backup_webdav_last_ts',
    'extra.backup_s3_last_ts', 'extra.backup_last_ts',
    'extra.backup_msg_groups', 'extra.backup_msg_ids',
    'extra.backup_webdav_history', 'extra.backup_s3_history',
    'extra.backup_telegram_data_revision', 'extra.backup_webdav_data_revision',
    'extra.backup_s3_data_revision',
    'extra.inbox_offset', 'extra.inbox_offset_owner', 'extra.inbox_routes_revision', 'extra.seen_chats'
) AND NEW.k NOT GLOB 'extra.inbox_failure.*'
  AND NEW.k NOT GLOB 'extra.inbox_offset.*'
  AND NEW.k NOT GLOB 'mtproto.session.*'
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

-- OLD.v IS NOT NEW.v 挡住同值 UPSERT 重写，防未来出现非排除键的同值噪声写入。
CREATE TRIGGER data_revision_settings_update AFTER UPDATE ON settings
WHEN OLD.k = NEW.k AND OLD.v IS NOT NEW.v
  AND NEW.k NOT IN (
    'extra.backup_telegram_last_ts', 'extra.backup_webdav_last_ts',
    'extra.backup_s3_last_ts', 'extra.backup_last_ts',
    'extra.backup_msg_groups', 'extra.backup_msg_ids',
    'extra.backup_webdav_history', 'extra.backup_s3_history',
    'extra.backup_telegram_data_revision', 'extra.backup_webdav_data_revision',
    'extra.backup_s3_data_revision',
    'extra.inbox_offset', 'extra.inbox_offset_owner', 'extra.inbox_routes_revision', 'extra.seen_chats'
) AND NEW.k NOT GLOB 'extra.inbox_failure.*'
  AND NEW.k NOT GLOB 'extra.inbox_offset.*'
  AND NEW.k NOT GLOB 'mtproto.session.*'
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_settings_delete AFTER DELETE ON settings
WHEN OLD.k NOT IN (
    'extra.backup_telegram_last_ts', 'extra.backup_webdav_last_ts',
    'extra.backup_s3_last_ts', 'extra.backup_last_ts',
    'extra.backup_msg_groups', 'extra.backup_msg_ids',
    'extra.backup_webdav_history', 'extra.backup_s3_history',
    'extra.backup_telegram_data_revision', 'extra.backup_webdav_data_revision',
    'extra.backup_s3_data_revision',
    'extra.inbox_offset', 'extra.inbox_offset_owner', 'extra.inbox_routes_revision', 'extra.seen_chats'
) AND OLD.k NOT GLOB 'extra.inbox_failure.*'
  AND OLD.k NOT GLOB 'extra.inbox_offset.*'
  AND OLD.k NOT GLOB 'mtproto.session.*'
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
