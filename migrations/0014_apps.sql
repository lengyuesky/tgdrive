-- 安装包随 SQLite 快照备份；解压资源只是可以重新生成的运行缓存。
CREATE TABLE apps (
    id TEXT PRIMARY KEY,
    manifest TEXT NOT NULL,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    revision TEXT NOT NULL,
    package BLOB NOT NULL,
    source TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    pinned INTEGER NOT NULL DEFAULT 0,
    installed_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- 卸载时默认保留设置，因此不对 apps 建级联删除外键。
CREATE TABLE app_settings (
    app_id TEXT PRIMARY KEY,
    settings TEXT NOT NULL DEFAULT '{}'
);

-- 只迁移设置，不替用户安装应用。旧键不再由系统设置读取或写入。
INSERT INTO app_settings (app_id, settings)
SELECT 'shorts', json_object('source_dir', CASE WHEN v = '' THEN '/' ELSE v END)
FROM settings WHERE k = 'extra.shorts_dir';
DELETE FROM settings WHERE k = 'extra.shorts_dir';
