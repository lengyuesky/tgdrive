-- 原生批量导入显式声明是否允许覆盖，旧网页/历史会话保留原先覆盖语义。
ALTER TABLE upload_sessions ADD COLUMN overwrite INTEGER NOT NULL DEFAULT 1 CHECK(overwrite IN (0, 1));
