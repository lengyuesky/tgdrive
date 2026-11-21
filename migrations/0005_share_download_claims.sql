-- v5:分享访问票据与原子下载次数领取。
--
-- 同一浏览器下载大文件时通常会发起多个 Range 请求。领取记录把一次短期
-- 票据对同一文件的所有请求合并为一次下载，同时仍让不同文件分别计数。
CREATE TABLE share_download_claims (
    share_id  INTEGER NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
    ticket_id TEXT    NOT NULL,
    node_id   INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (share_id, ticket_id, node_id)
);

CREATE INDEX idx_share_download_claims_exp
    ON share_download_claims(expires_at);
