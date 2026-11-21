-- 收件来源单独记录；旧文件来源未知时保持空缺，不能按目录或 SHA-256 猜测。
CREATE TABLE inbox_sources (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    source  TEXT NOT NULL CHECK (source IN ('bot', 'channel'))
);

CREATE INDEX idx_inbox_sources_source ON inbox_sources(source, node_id);
