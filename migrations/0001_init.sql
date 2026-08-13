-- tgdrive 元数据库初始化
-- nodes: 虚拟目录树(根目录 id=1);chunks: 文件分块 ↔ Telegram 消息映射

CREATE TABLE nodes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id   INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    is_dir      INTEGER NOT NULL DEFAULT 0,
    size        INTEGER NOT NULL DEFAULT 0,
    -- ready: 正常可见 | uploading: 上传中(对列表不可见,崩溃后启动时回滚)
    status      TEXT    NOT NULL DEFAULT 'ready',
    created_at  INTEGER NOT NULL,
    modified_at INTEGER NOT NULL
);

-- 同一目录下可见名称唯一;允许并存一个 uploading 影子行用于覆盖写
CREATE UNIQUE INDEX ux_nodes_parent_name_ready
    ON nodes(parent_id, name) WHERE status = 'ready';
CREATE INDEX idx_nodes_parent ON nodes(parent_id);

CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    idx           INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    tg_file_id    TEXT    NOT NULL,
    tg_message_id INTEGER NOT NULL,
    UNIQUE(node_id, idx)
);
CREATE INDEX idx_chunks_msg ON chunks(tg_message_id);

-- 键值设置(自动生成的 cookie secret 等)
CREATE TABLE settings (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);

-- 根目录
INSERT INTO nodes (id, parent_id, name, is_dir, size, status, created_at, modified_at)
VALUES (1, NULL, '', 1, 0, 'ready', unixepoch(), unixepoch());
