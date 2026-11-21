-- 阅读文件的持久化表达式索引由 SQLite 随节点写入自动维护，无需 App 全盘扫描。
-- 修订是不可预测的令牌，不向插件暴露全盘变更计数。
CREATE TABLE reading_tree_revision (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision TEXT NOT NULL
);
INSERT INTO reading_tree_revision VALUES (1, lower(hex(randomblob(16))));

CREATE TRIGGER reading_tree_insert AFTER INSERT ON nodes
BEGIN
    UPDATE reading_tree_revision SET revision = lower(hex(randomblob(16))) WHERE id = 1;
END;
CREATE TRIGGER reading_tree_delete AFTER DELETE ON nodes
BEGIN
    UPDATE reading_tree_revision SET revision = lower(hex(randomblob(16))) WHERE id = 1;
END;
CREATE TRIGGER reading_tree_update AFTER UPDATE OF parent_id, name, is_dir, status, size, sha256, created_at, modified_at ON nodes
WHEN OLD.parent_id IS NOT NEW.parent_id OR OLD.name IS NOT NEW.name
  OR OLD.is_dir IS NOT NEW.is_dir OR OLD.status IS NOT NEW.status
  OR OLD.size IS NOT NEW.size OR OLD.sha256 IS NOT NEW.sha256
  OR OLD.created_at IS NOT NEW.created_at OR OLD.modified_at IS NOT NEW.modified_at
BEGIN
    UPDATE reading_tree_revision SET revision = lower(hex(randomblob(16))) WHERE id = 1;
END;

-- format 分类以文件名最后一个已知后缀为准；图片节点只用于发现直属图片目录。
CREATE VIEW reading_files AS
SELECT id, parent_id,
    CASE
        WHEN lower(name) GLOB '*.txt' THEN 'txt'
        WHEN lower(name) GLOB '*.epub' THEN 'epub'
        WHEN lower(name) GLOB '*.pdf' THEN 'pdf'
        WHEN lower(name) GLOB '*.cbz' THEN 'cbz'
        WHEN lower(name) GLOB '*.zip' THEN 'zip'
        WHEN substr(name, 1, 1) != '.' AND (
            lower(name) GLOB '*.jpg' OR lower(name) GLOB '*.jpeg' OR lower(name) GLOB '*.png'
            OR lower(name) GLOB '*.webp' OR lower(name) GLOB '*.gif' OR lower(name) GLOB '*.bmp'
            OR lower(name) GLOB '*.avif') THEN 'image'
        ELSE NULL
    END AS format
FROM nodes WHERE status = 'ready' AND is_dir = 0;

CREATE INDEX idx_reading_format ON nodes (
    CASE
        WHEN lower(name) GLOB '*.txt' THEN 'txt'
        WHEN lower(name) GLOB '*.epub' THEN 'epub'
        WHEN lower(name) GLOB '*.pdf' THEN 'pdf'
        WHEN lower(name) GLOB '*.cbz' THEN 'cbz'
        WHEN lower(name) GLOB '*.zip' THEN 'zip'
        WHEN substr(name, 1, 1) != '.' AND (
            lower(name) GLOB '*.jpg' OR lower(name) GLOB '*.jpeg' OR lower(name) GLOB '*.png'
            OR lower(name) GLOB '*.webp' OR lower(name) GLOB '*.gif' OR lower(name) GLOB '*.bmp'
            OR lower(name) GLOB '*.avif') THEN 'image'
        ELSE NULL
    END,
    parent_id, name COLLATE NOCASE, id
) WHERE status = 'ready' AND is_dir = 0;
