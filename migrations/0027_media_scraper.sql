-- 刮削批次只由页面显式推进；记录不是后台调度队列，Token 不写入本库。
CREATE TABLE app_scraper_state (
    app_id TEXT PRIMARY KEY,
    secret_generation TEXT NOT NULL DEFAULT ''
);
CREATE TABLE app_scraper_batches (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    config_key TEXT NOT NULL,
    root_id INTEGER NOT NULL,
    root_path TEXT NOT NULL,
    owner TEXT NOT NULL,
    generation TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    read_only INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL DEFAULT 0,
    message TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX app_scraper_one_active ON app_scraper_batches(app_id)
    WHERE status IN ('running', 'stopping');
CREATE INDEX app_scraper_history ON app_scraper_batches(app_id, updated_at DESC);
CREATE TABLE app_scraper_sources (
    batch_id TEXT NOT NULL REFERENCES app_scraper_batches(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    source_id TEXT NOT NULL,
    movie TEXT NOT NULL,
    PRIMARY KEY(batch_id, provider, source_id)
);
CREATE TABLE app_scraper_items (
    id TEXT PRIMARY KEY,
    batch_id TEXT NOT NULL REFERENCES app_scraper_batches(id) ON DELETE CASCADE,
    group_key TEXT NOT NULL,
    position INTEGER NOT NULL,
    files TEXT NOT NULL,
    plan TEXT NOT NULL DEFAULT 'null',
    result TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending',
    UNIQUE(batch_id, group_key)
);
CREATE TABLE app_scraper_directories (
    batch_id TEXT NOT NULL REFERENCES app_scraper_batches(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    node_id INTEGER NOT NULL,
    PRIMARY KEY(batch_id, path)
);
-- 上传影子在提交事务中核对应用、批次、路径和资料集合；不能被普通覆盖接口绕过。
CREATE TABLE app_scraper_upload_guards (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    -- 清除应用历史不能移除正在上传的提交保护；缺少批次时保护检查必须失败。
    batch_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    role TEXT NOT NULL,
    guard TEXT NOT NULL
);
CREATE INDEX app_scraper_items_next ON app_scraper_items(batch_id,status,position);
