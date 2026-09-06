-- 连续滚动目录：ready 部分索引固定父目录与类型后，支持各排序正反向 seek。
CREATE INDEX idx_nodes_entries_name ON nodes(
    parent_id, is_dir, name COLLATE NOCASE, name COLLATE BINARY, id
) WHERE status = 'ready';
CREATE INDEX idx_nodes_entries_size ON nodes(
    parent_id, is_dir, size, name COLLATE NOCASE, name COLLATE BINARY, id
) WHERE status = 'ready';
CREATE INDEX idx_nodes_entries_mtime ON nodes(
    parent_id, is_dir, modified_at, name COLLATE NOCASE, name COLLATE BINARY, id
) WHERE status = 'ready';
