-- 封面与运行目录不在主库事务内：安装替换仅排队，提交后可重试，重启后继续。
-- 不关联 apps 外键，应用删除后任务仍需保留。token 隔离后来新建的任务。
CREATE TABLE app_cleanup_jobs (
    app_id TEXT PRIMARY KEY CHECK (app_id IN ('books', 'comics')),
    token TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
