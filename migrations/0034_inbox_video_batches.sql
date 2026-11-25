-- 连发视频批次与共同改名的分段序号。批次只决定是否提供「分段共同重命名」，
-- 目录选择与确认存储仍按文件各自进行。
ALTER TABLE inbox_requests ADD COLUMN batch_id TEXT;
ALTER TABLE inbox_requests ADD COLUMN rename_part INTEGER;
CREATE INDEX idx_inbox_requests_batch ON inbox_requests(batch_id);
