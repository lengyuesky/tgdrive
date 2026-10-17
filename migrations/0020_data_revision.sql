-- v20：用户数据修订号：只在实质数据（文件、分享、设置等）变化时递增。
-- 运行噪声（备份状态、收件游标、维护日志等）不触发，供「无变化跳过每日备份」比对。
-- 仿照 0019 的 reading_tree_revision：SQLite 触发器随写入事务原子自增，无应用侧维护成本。

CREATE TABLE data_revision (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
);
INSERT INTO data_revision VALUES (1, 0);

-- ---------- 普通表：任何写入都算实质变化 ----------

CREATE TRIGGER data_revision_nodes_insert AFTER INSERT ON nodes
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_nodes_delete AFTER DELETE ON nodes
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
-- 不用 UPDATE OF 列清单：sha256/favorite 由后续迁移 ALTER 加入，逐列清单无法随之维护。
CREATE TRIGGER data_revision_nodes_update AFTER UPDATE ON nodes
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_chunks_insert AFTER INSERT ON chunks
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_chunks_delete AFTER DELETE ON chunks
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_chunks_update AFTER UPDATE ON chunks
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_shares_insert AFTER INSERT ON shares
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_shares_delete AFTER DELETE ON shares
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
-- downloads 是分享被领取下载的运行时计数器，不算数据变化。
CREATE TRIGGER data_revision_shares_update AFTER UPDATE ON shares
WHEN NEW.downloads = OLD.downloads
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_trash_insert AFTER INSERT ON trash
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_trash_delete AFTER DELETE ON trash
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_trash_update AFTER UPDATE ON trash
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_storage_targets_insert AFTER INSERT ON storage_targets
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_storage_targets_delete AFTER DELETE ON storage_targets
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_storage_targets_update AFTER UPDATE ON storage_targets
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_inbox_sources_insert AFTER INSERT ON inbox_sources
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_inbox_sources_delete AFTER DELETE ON inbox_sources
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_inbox_sources_update AFTER UPDATE ON inbox_sources
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_apps_insert AFTER INSERT ON apps
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_apps_delete AFTER DELETE ON apps
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_apps_update AFTER UPDATE ON apps
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_app_settings_insert AFTER INSERT ON app_settings
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_settings_delete AFTER DELETE ON app_settings
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_settings_update AFTER UPDATE ON app_settings
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_app_data_insert AFTER INSERT ON app_data
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_data_delete AFTER DELETE ON app_data
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_data_update AFTER UPDATE ON app_data
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_app_recovery_insert AFTER INSERT ON app_recovery
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_recovery_delete AFTER DELETE ON app_recovery
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_recovery_update AFTER UPDATE ON app_recovery
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER data_revision_app_lifecycle_insert AFTER INSERT ON app_lifecycle
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_lifecycle_delete AFTER DELETE ON app_lifecycle
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER data_revision_app_lifecycle_update AFTER UPDATE ON app_lifecycle
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;

-- ---------- settings：排除运行时状态键 ----------
-- GLOB 的 _ 是字面量（LIKE 的 _ 是单字符通配符），键名含下划线，必须用 GLOB。
-- 备份渠道配置、加密口令、skip 开关本身是用户配置，不在排除之列，修改应算变化。

CREATE TRIGGER data_revision_settings_insert AFTER INSERT ON settings
WHEN NEW.k NOT IN (
    'extra.backup_telegram_last_ts', 'extra.backup_webdav_last_ts',
    'extra.backup_s3_last_ts', 'extra.backup_last_ts',
    'extra.backup_msg_groups', 'extra.backup_msg_ids',
    'extra.backup_webdav_history', 'extra.backup_s3_history',
    'extra.backup_telegram_data_revision', 'extra.backup_webdav_data_revision',
    'extra.backup_s3_data_revision',
    'extra.inbox_offset', 'extra.seen_chats'
) AND NEW.k NOT GLOB 'extra.inbox_failure.*'
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
    'extra.inbox_offset', 'extra.seen_chats'
) AND NEW.k NOT GLOB 'extra.inbox_failure.*'
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
    'extra.inbox_offset', 'extra.seen_chats'
) AND OLD.k NOT GLOB 'extra.inbox_failure.*'
  AND OLD.k NOT GLOB 'mtproto.session.*'
BEGIN UPDATE data_revision SET revision = revision + 1 WHERE id = 1; END;
