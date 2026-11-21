-- v2:回收站 / 分享链接 / 文件哈希(秒传)

-- 文件内容哈希(十六进制 sha256;历史文件为 NULL,新上传时写入)
ALTER TABLE nodes ADD COLUMN sha256 TEXT;
CREATE INDEX idx_nodes_sha ON nodes(sha256) WHERE sha256 IS NOT NULL;

-- 回收站:软删除的子树根。子树所有节点 status 置为 'trashed',
-- 恢复时整树改回 'ready';彻底删除走原有 delete_node(引用计数删 TG 消息)。
CREATE TABLE trash (
    node_id        INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    orig_parent_id INTEGER NOT NULL,
    orig_path      TEXT    NOT NULL,
    trashed_at     INTEGER NOT NULL
);
CREATE INDEX idx_trash_at ON trash(trashed_at);

-- 分享链接:免登录访问某个文件/目录
CREATE TABLE shares (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    token         TEXT    NOT NULL UNIQUE,
    node_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    -- 提取码(明文;单用户自部署场景,库内本就有更敏感的 bot token)
    password      TEXT,
    expires_at    INTEGER,          -- NULL = 永久
    max_downloads INTEGER,          -- NULL = 不限次数
    downloads     INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);
CREATE INDEX idx_shares_node ON shares(node_id);
