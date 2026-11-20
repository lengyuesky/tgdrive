-- 新平台后端身份独立于安装 generation；保留数据卸载后也不把 UID 分给另一应用。
CREATE TABLE app_runtime_identities (
    app_id TEXT PRIMARY KEY,
    uid INTEGER NOT NULL UNIQUE CHECK(uid>0),
    gid INTEGER NOT NULL UNIQUE CHECK(gid>0),
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE app_root_selection (
    id INTEGER PRIMARY KEY CHECK(id=1),
    app_id TEXT REFERENCES app_installations(id) ON DELETE SET NULL,
    revision TEXT NOT NULL
);
INSERT INTO app_root_selection VALUES(1,NULL,lower(hex(randomblob(16))));
CREATE TRIGGER app_root_backup AFTER UPDATE ON app_root_selection
WHEN OLD.app_id IS NOT NEW.app_id BEGIN
    UPDATE data_revision SET revision=revision+1 WHERE id=1;
END;
CREATE TRIGGER app_root_uninstall_revision AFTER DELETE ON app_installations
BEGIN
    UPDATE app_root_selection SET revision=lower(hex(randomblob(16))) WHERE id=1 AND app_id IS NULL;
END;
