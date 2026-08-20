-- v8：重命名或移动后可靠同步 Telegram 消息 caption 中的路径。
--
-- Telegram 不支持原地修改文档附件名，但允许编辑媒体 caption。同步任务只处理
-- 当前仅被一个虚拟文件引用的消息，避免零流量副本共用同一消息时写入错误路径。

CREATE TABLE caption_sync_jobs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    storage_target_id INTEGER NOT NULL REFERENCES storage_targets(id),
    tg_message_id     INTEGER NOT NULL,
    node_id           INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    chunk_idx         INTEGER NOT NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_retry_at     INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    generation        INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    UNIQUE(storage_target_id, tg_message_id)
);

CREATE INDEX idx_caption_sync_jobs_due
ON caption_sync_jobs(next_retry_at, id);
