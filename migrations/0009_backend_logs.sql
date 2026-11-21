-- v9：持久化关键后台事件，供管理页查看。
--
-- 这里只记录面向管理员的操作/任务摘要，不复制完整 tracing 输出，避免把
-- 令牌、请求载荷等诊断细节写入数据库。应用层会自动保留最近 2000 条。

CREATE TABLE backend_logs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    level      TEXT NOT NULL,
    category   TEXT NOT NULL,
    message    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_backend_logs_created
ON backend_logs(created_at DESC, id DESC);
