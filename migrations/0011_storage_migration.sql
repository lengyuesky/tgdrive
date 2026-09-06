-- 将历史分块通过 Telegram 服务端复制到主 Bot 的持久化任务状态。
-- 每次只允许一个全库迁移任务；每条消息完成后立即原子更新进度，服务重启可继续。
CREATE TABLE storage_migration_jobs (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    main_target_id    INTEGER NOT NULL REFERENCES storage_targets(id),
    status            TEXT    NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'completed')),
    total_messages    INTEGER NOT NULL DEFAULT 0,
    migrated_messages INTEGER NOT NULL DEFAULT 0,
    total_chunks      INTEGER NOT NULL DEFAULT 0,
    migrated_chunks   INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    started_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
