-- 应用私有数据随数据库备份；卸载默认保留，因此不关联 apps 的级联删除。
CREATE TABLE app_data (
    app_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    revision TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, key)
);
CREATE INDEX idx_app_data_recent ON app_data(app_id, updated_at DESC, key);
