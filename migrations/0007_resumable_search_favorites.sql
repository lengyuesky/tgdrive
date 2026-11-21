-- v7：可恢复分块上传、收藏与高级搜索所需索引。

-- 收藏属于节点本身；移动、重命名和进出回收站时都能自然保留。
ALTER TABLE nodes ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_nodes_favorite ON nodes(favorite, status) WHERE favorite = 1;

-- 浏览器上传会话。已经传到 Telegram 的完整分块仍记录在 chunks 表，
-- 因此刷新页面或服务重启后只需补传缺失下标。
CREATE TABLE upload_sessions (
    id                TEXT PRIMARY KEY,
    node_id           INTEGER NOT NULL UNIQUE REFERENCES nodes(id) ON DELETE CASCADE,
    display_path      TEXT    NOT NULL,
    expected_size     INTEGER NOT NULL,
    sha256            TEXT    NOT NULL,
    chunk_size        INTEGER NOT NULL,
    storage_target_id INTEGER NOT NULL REFERENCES storage_targets(id),
    -- 仅在会话期间暂存；第 0 块成功后 Telegram 的 file_id 才是持久来源。
    thumbnail         BLOB,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX idx_upload_sessions_updated ON upload_sessions(updated_at, id);

-- 同一分块的请求锁。进程启动时会全部释放；已完成状态以 chunks 表为准。
CREATE TABLE upload_part_claims (
    session_id TEXT    NOT NULL REFERENCES upload_sessions(id) ON DELETE CASCADE,
    idx        INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    PRIMARY KEY(session_id, idx)
);
