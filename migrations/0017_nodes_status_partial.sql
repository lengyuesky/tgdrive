-- 启动清理与回收站按状态查找少量非 ready 节点；部分索引只收录这些行，
-- 避免在几十万 ready 节点上全表扫描。
CREATE INDEX idx_nodes_uploading ON nodes(id) WHERE status = 'uploading';
CREATE INDEX idx_nodes_trashed ON nodes(id) WHERE status = 'trashed';
