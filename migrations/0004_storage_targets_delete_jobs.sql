-- v4:存储目标版本化与可靠删除。
--
-- Telegram 的 message_id 只在 chat 内唯一，file_id 也与 Bot 身份绑定。
-- 因此分块必须记录创建它的完整存储目标，不能在删除/读取时套用当前配置。

CREATE TABLE storage_targets (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    api_base   TEXT    NOT NULL,
    bot_token  TEXT    NOT NULL,
    chat_id    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(api_base, bot_token, chat_id)
);

-- 旧库无法仅凭历史分块推断当时使用的 Bot/频道，先允许为空；
-- 应用启动后会用当前生效配置做一次明确的兼容回填。
ALTER TABLE chunks ADD COLUMN storage_target_id INTEGER REFERENCES storage_targets(id);
CREATE INDEX idx_chunks_target_msg ON chunks(storage_target_id, tg_message_id);

-- 删除 Telegram 消息属于跨系统操作，必须先持久化再异步重试。
CREATE TABLE delete_jobs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    storage_target_id INTEGER NOT NULL REFERENCES storage_targets(id),
    tg_message_id     INTEGER NOT NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_retry_at     INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    created_at        INTEGER NOT NULL,
    UNIQUE(storage_target_id, tg_message_id)
);
CREATE INDEX idx_delete_jobs_due ON delete_jobs(next_retry_at, id);
