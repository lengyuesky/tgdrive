-- v6:Telegram 消息成为缩略图的唯一持久来源。
-- SQLite 只保留 Telegram 返回的缩略图 file_id，不再保存图片 BLOB。
ALTER TABLE chunks ADD COLUMN tg_thumbnail_file_id TEXT;

DROP TABLE thumbnails;
