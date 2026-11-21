-- 新应用平台与旧表完全分离；卸载默认只移除安装记录，保留应用命名空间的 KV。
CREATE TABLE app_installations (
    id TEXT PRIMARY KEY,
    manifest TEXT NOT NULL CHECK(json_valid(manifest)),
    generation TEXT NOT NULL,
    state_revision TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    pinned INTEGER NOT NULL CHECK(pinned IN (0,1)),
    scope TEXT NOT NULL CHECK(json_valid(scope)),
    scope_revision TEXT NOT NULL,
    target TEXT NOT NULL,
    package_sha256 TEXT NOT NULL,
    package_size INTEGER NOT NULL CHECK(package_size > 0),
    installed_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE app_kv (
    app_id TEXT NOT NULL,
    key TEXT NOT NULL COLLATE BINARY,
    value TEXT NOT NULL CHECK(json_valid(value)),
    revision TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes >= 0),
    PRIMARY KEY(app_id,key)
) WITHOUT ROWID;
CREATE TABLE app_kv_usage (
    app_id TEXT PRIMARY KEY,
    entries INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TRIGGER app_kv_insert_usage AFTER INSERT ON app_kv BEGIN
    INSERT INTO app_kv_usage(app_id,entries,bytes) VALUES(NEW.app_id,1,NEW.bytes)
    ON CONFLICT(app_id) DO UPDATE SET entries=entries+1,bytes=bytes+NEW.bytes;
END;
CREATE TRIGGER app_kv_update_usage AFTER UPDATE ON app_kv BEGIN
    UPDATE app_kv_usage SET bytes=bytes-OLD.bytes+NEW.bytes WHERE app_id=NEW.app_id;
END;
CREATE TRIGGER app_kv_delete_usage AFTER DELETE ON app_kv BEGIN
    UPDATE app_kv_usage SET entries=entries-1,bytes=bytes-OLD.bytes WHERE app_id=OLD.app_id;
END;
-- 快照保留原始目录，包括暂时无法解析的未来版本；历史永远不因刷新或卸载删除。
CREATE TABLE app_catalog_snapshots (
    repository TEXT PRIMARY KEY,
    raw_catalog TEXT NOT NULL,
    last_success_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE app_release_history (
    repository TEXT NOT NULL,
    app_id TEXT NOT NULL,
    version TEXT NOT NULL,
    manifest TEXT NOT NULL,
    packages TEXT NOT NULL,
    PRIMARY KEY(repository,app_id,version)
) WITHOUT ROWID;
CREATE TABLE app_package_bindings (
    app_id TEXT NOT NULL,
    version TEXT NOT NULL,
    target TEXT NOT NULL,
    manifest TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY(app_id,version,target)
) WITHOUT ROWID;
CREATE TABLE app_package_uploads (
    id TEXT PRIMARY KEY,
    manifest TEXT NOT NULL,
    package TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE app_install_previews (
    id TEXT PRIMARY KEY,
    preview TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX app_package_uploads_expiry ON app_package_uploads(expires_at);
CREATE INDEX app_install_previews_expiry ON app_install_previews(expires_at);
-- 应用安装、目录授权与 KV 会改变需要备份的持久数据；缓存和下载目录不纳入。
CREATE TRIGGER app_installations_backup_insert AFTER INSERT ON app_installations BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_installations_backup_update AFTER UPDATE ON app_installations BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_installations_backup_delete AFTER DELETE ON app_installations BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_kv_backup_insert AFTER INSERT ON app_kv BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_kv_backup_update AFTER UPDATE ON app_kv BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_package_bindings_backup_insert AFTER INSERT ON app_package_bindings BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_release_history_backup_insert AFTER INSERT ON app_release_history BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_release_history_backup_update AFTER UPDATE ON app_release_history
WHEN OLD.manifest<>NEW.manifest OR OLD.packages<>NEW.packages BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_kv_backup_delete AFTER DELETE ON app_kv BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
