-- v3:按内容哈希缓存视频缩略图；复制、秒传和重命名可直接复用。
CREATE TABLE thumbnails (
    sha256     TEXT PRIMARY KEY,
    mime       TEXT NOT NULL,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
