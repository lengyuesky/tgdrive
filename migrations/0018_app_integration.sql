-- 旧安装保留原目录授权；新安装由安装事务明确写入 none。
ALTER TABLE apps ADD COLUMN scope_mode TEXT NOT NULL DEFAULT 'all';
UPDATE apps SET scope_mode = 'selected' WHERE scope_paths != '[]';

-- 每应用仅保留一次更新前的程序、配置、私有数据快照；随主库一起备份。
CREATE TABLE app_recovery (
    app_id TEXT PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
    manifest TEXT NOT NULL,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    package BLOB NOT NULL,
    source TEXT NOT NULL,
    settings TEXT NOT NULL,
    data TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE app_lifecycle (
    -- 卸载保留私有数据时也保留数据版本，避免重装旧程序误读新版数据。
    app_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL DEFAULT 0,
    phase TEXT NOT NULL DEFAULT 'installed',
    message TEXT NOT NULL DEFAULT '',
    owner TEXT NOT NULL DEFAULT '',
    lease_until INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
