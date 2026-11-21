-- v23：上传结束回执与结果不确定的分块。回执只保留七天，不驱动每日数据备份修订。
CREATE TABLE upload_receipts (
    id TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK(status IN ('completed','cancelled')),
    display_path TEXT NOT NULL,
    expected_size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    node_json TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX upload_receipts_expiry ON upload_receipts(expires_at);

-- 已发到 Telegram 但未能确定结果的分块不能自动重传，否则可能制造重复消息。
CREATE TABLE upload_part_uncertainties (
    session_id TEXT NOT NULL REFERENCES upload_sessions(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(session_id,idx)
);
