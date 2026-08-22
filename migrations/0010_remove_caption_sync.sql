-- v10：取消 Telegram 消息路径 caption 与重命名同步机制。
-- 目录结构只以 SQLite 为准；升级时直接丢弃旧的待同步任务和自动开关。

DROP TABLE IF EXISTS caption_sync_jobs;
DELETE FROM settings WHERE k = 'extra.auto_caption_sync';
