-- 通用应用查询索引：不改变网盘原有节点字段、ID、原生内容版本或写入行为。
CREATE TABLE app_node_meta (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    revision TEXT NOT NULL,
    content_version TEXT,
    children_revision TEXT,
    random_key INTEGER NOT NULL,
    hidden INTEGER NOT NULL CHECK(hidden IN (0,1))
);
INSERT INTO app_node_meta(node_id,revision,content_version,children_revision,random_key,hidden)
SELECT id,lower(hex(randomblob(16))),
    CASE WHEN is_dir=0 THEN lower(hex(randomblob(16))) END,
    CASE WHEN is_dir=1 THEN lower(hex(randomblob(16))) END,
    (random() & 9223372036854775807),substr(name,1,1)='.' FROM nodes;
CREATE INDEX app_nodes_name ON nodes(lower(name) COLLATE BINARY,name COLLATE BINARY,id) WHERE status='ready';
CREATE INDEX app_dirs_name ON nodes(lower(name) COLLATE BINARY,name COLLATE BINARY,id) WHERE status='ready' AND is_dir=1;
CREATE INDEX app_dirs_created ON nodes(created_at,id) WHERE status='ready' AND is_dir=1;
CREATE INDEX app_dirs_modified ON nodes(modified_at,id) WHERE status='ready' AND is_dir=1;
CREATE INDEX app_nodes_size ON nodes(size,id) WHERE status='ready';
CREATE INDEX app_nodes_created ON nodes(created_at,id) WHERE status='ready';
CREATE INDEX app_nodes_modified ON nodes(modified_at,id) WHERE status='ready';
CREATE INDEX app_nodes_parent_name ON nodes(parent_id,lower(name) COLLATE BINARY,name COLLATE BINARY,id) WHERE status='ready';
CREATE INDEX app_nodes_parent_size ON nodes(parent_id,size,id) WHERE status='ready';
CREATE INDEX app_nodes_parent_created ON nodes(parent_id,created_at,id) WHERE status='ready';
CREATE INDEX app_nodes_parent_modified ON nodes(parent_id,modified_at,id) WHERE status='ready';
-- 闭包只保存目录身份关系，不保存路径前缀；索引查询无需每页递归扫描树。
CREATE TABLE app_ancestors (
    ancestor_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    depth INTEGER NOT NULL CHECK(depth>=0),
    PRIMARY KEY(ancestor_id,node_id)
) WITHOUT ROWID;
CREATE INDEX app_ancestors_node ON app_ancestors(node_id,depth DESC,ancestor_id);
WITH RECURSIVE chain(ancestor_id,node_id,depth) AS (
    SELECT id,id,0 FROM nodes
    UNION ALL
    SELECT n.parent_id,c.node_id,c.depth+1 FROM chain c JOIN nodes n ON n.id=c.ancestor_id
    WHERE n.parent_id IS NOT NULL AND c.depth<4096
)
INSERT INTO app_ancestors SELECT ancestor_id,node_id,depth FROM chain;
-- 多段后缀只索引有效后缀，名称小写折叠仅限 SQLite 的 ASCII lower。
CREATE TABLE app_suffixes (
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    extension TEXT NOT NULL COLLATE BINARY,
    parent_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY(node_id,extension)
) WITHOUT ROWID;
CREATE INDEX app_suffixes_extension ON app_suffixes(extension,node_id);
CREATE INDEX app_suffixes_parent ON app_suffixes(parent_id,extension,node_id);
WITH RECURSIVE suffixes(node_id,value) AS (
    SELECT id,substr(lower(name),instr(name,'.')+1) FROM nodes WHERE is_dir=0 AND instr(name,'.')>0
    UNION ALL
    SELECT node_id,substr(value,instr(value,'.')+1) FROM suffixes WHERE instr(value,'.')>0
)
INSERT OR IGNORE INTO app_suffixes
SELECT s.node_id,s.value,n.parent_id FROM suffixes s JOIN nodes n ON n.id=s.node_id WHERE length(s.value) BETWEEN 1 AND 40
    AND s.value NOT GLOB '*[^a-z0-9._+-]*' AND s.value NOT LIKE '.%' AND s.value NOT LIKE '%.' AND instr(s.value,'..')=0;
-- 随机环的组合索引只重复最小定位键，支持「授权根 + 后缀」直接寻址，不随机 OFFSET。
CREATE TABLE app_random_index (
    root_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    extension TEXT NOT NULL COLLATE BINARY,
    random_key INTEGER NOT NULL,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY(root_id,extension,random_key,node_id)
) WITHOUT ROWID;
CREATE INDEX app_random_index_node ON app_random_index(node_id,root_id);
INSERT INTO app_random_index
SELECT a.ancestor_id,'',m.random_key,n.id FROM nodes n JOIN app_node_meta m ON m.node_id=n.id JOIN app_ancestors a ON a.node_id=n.id WHERE n.is_dir=0;
INSERT INTO app_random_index
SELECT a.ancestor_id,s.extension,m.random_key,s.node_id FROM app_suffixes s JOIN app_node_meta m ON m.node_id=s.node_id JOIN app_ancestors a ON a.node_id=s.node_id;
CREATE TABLE app_change_state(
    id INTEGER PRIMARY KEY CHECK(id=1),
    epoch TEXT NOT NULL,
    cursor_key TEXT NOT NULL,
    trimmed_seq INTEGER NOT NULL DEFAULT 0,
    restore_pending INTEGER NOT NULL DEFAULT 0 CHECK(restore_pending IN(0,1)),
    retention_ms INTEGER,
    max_entries INTEGER
);
INSERT INTO app_change_state(id,epoch,cursor_key) VALUES(1,lower(hex(randomblob(16))),lower(hex(randomblob(32))));
-- 保留恢复来源的旧签名域，只用于验证旧游标后返回失效，不能变成另一应用身份。
CREATE TABLE app_cursor_keys(key TEXT PRIMARY KEY) WITHOUT ROWID;
INSERT INTO app_cursor_keys SELECT cursor_key FROM app_change_state WHERE id=1;
CREATE TABLE app_file_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at INTEGER NOT NULL,
    node_id INTEGER NOT NULL,
    is_dir INTEGER NOT NULL,
    structural INTEGER NOT NULL,
    previous_ancestors TEXT NOT NULL,
    previous_path TEXT
);
CREATE INDEX app_file_changes_time ON app_file_changes(occurred_at,seq);
-- 现有整棵回收/恢复操作显式标记事务根，日志只发一条子树失效。
CREATE TABLE app_change_batches(root_id INTEGER PRIMARY KEY);
CREATE TRIGGER app_file_changes_retention AFTER INSERT ON app_file_changes BEGIN
    -- 限额由 Db::open 从契约常量填入，不在迁移另抄线上限额。
    UPDATE app_change_state SET trimmed_seq=max(trimmed_seq,COALESCE(NEW.seq-max_entries,0),
        COALESCE((SELECT max(seq) FROM app_file_changes INDEXED BY app_file_changes_time WHERE occurred_at<NEW.occurred_at-retention_ms),0)) WHERE id=1;
    DELETE FROM app_file_changes WHERE seq <= NEW.seq-(SELECT max_entries FROM app_change_state WHERE id=1);
    DELETE FROM app_file_changes WHERE occurred_at < NEW.occurred_at-(SELECT retention_ms FROM app_change_state WHERE id=1);
END;
CREATE TRIGGER app_nodes_insert AFTER INSERT ON nodes BEGIN
    INSERT INTO app_node_meta VALUES(NEW.id,lower(hex(randomblob(16))),
        CASE WHEN NEW.is_dir=0 THEN lower(hex(randomblob(16))) END,
        CASE WHEN NEW.is_dir=1 THEN lower(hex(randomblob(16))) END,
        (random() & 9223372036854775807),substr(NEW.name,1,1)='.');
    INSERT INTO app_ancestors VALUES(NEW.id,NEW.id,0);
    INSERT INTO app_ancestors SELECT ancestor_id,NEW.id,depth+1 FROM app_ancestors WHERE node_id=NEW.parent_id;
    INSERT OR IGNORE INTO app_suffixes
    SELECT NEW.id,value,NEW.parent_id FROM (
        WITH RECURSIVE suffix(value) AS (
            SELECT substr(lower(NEW.name),instr(NEW.name,'.')+1) WHERE NEW.is_dir=0 AND instr(NEW.name,'.')>0
            UNION ALL SELECT substr(value,instr(value,'.')+1) FROM suffix WHERE instr(value,'.')>0
        ) SELECT value FROM suffix
    ) WHERE length(value) BETWEEN 1 AND 40 AND value NOT GLOB '*[^a-z0-9._+-]*' AND value NOT LIKE '.%' AND value NOT LIKE '%.' AND instr(value,'..')=0;
    INSERT INTO app_random_index
    SELECT a.ancestor_id,'',m.random_key,NEW.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=NEW.id WHERE a.node_id=NEW.id AND NEW.is_dir=0;
    INSERT INTO app_random_index
    SELECT a.ancestor_id,s.extension,m.random_key,NEW.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=NEW.id JOIN app_suffixes s ON s.node_id=NEW.id WHERE a.node_id=NEW.id;
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id=NEW.parent_id AND NEW.status='ready';
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    SELECT CAST(unixepoch('subsec')*1000 AS INTEGER),NEW.id,NEW.is_dir,0,'[]',NULL WHERE NEW.status='ready';
END;
CREATE TRIGGER app_nodes_update AFTER UPDATE ON nodes
WHEN OLD.parent_id IS NOT NEW.parent_id OR OLD.name<>NEW.name OR OLD.is_dir<>NEW.is_dir
    OR OLD.size<>NEW.size OR OLD.status<>NEW.status OR OLD.created_at<>NEW.created_at
    OR OLD.modified_at<>NEW.modified_at OR OLD.sha256 IS NOT NEW.sha256
BEGIN
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    SELECT CAST(unixepoch('subsec')*1000 AS INTEGER),NEW.id,NEW.is_dir,
        NEW.is_dir=1 AND (OLD.parent_id IS NOT NEW.parent_id OR OLD.name<>NEW.name OR OLD.status<>NEW.status),
        CASE WHEN OLD.status='ready' THEN (SELECT json_group_array(ancestor_id) FROM app_ancestors WHERE node_id=OLD.id) ELSE '[]' END,
        CASE WHEN OLD.status='ready' THEN (SELECT '/'||group_concat(name,'/') FROM (
            SELECT CASE WHEN a.ancestor_id=OLD.id THEN OLD.name ELSE n.name END AS name
            FROM app_ancestors a JOIN nodes n ON n.id=a.ancestor_id WHERE a.node_id=OLD.id AND a.ancestor_id<>1 ORDER BY a.depth DESC
        )) END
    WHERE (OLD.status='ready' OR NEW.status='ready')
        AND (OLD.parent_id IS NOT NEW.parent_id OR OLD.name<>NEW.name OR OLD.status<>NEW.status OR OLD.size<>NEW.size OR OLD.sha256 IS NOT NEW.sha256 OR OLD.modified_at<>NEW.modified_at)
        AND (NOT EXISTS(SELECT 1 FROM app_change_batches) OR EXISTS(SELECT 1 FROM app_change_batches WHERE root_id=NEW.id));
    UPDATE app_installations SET scope_revision=lower(hex(randomblob(16)))
    WHERE NEW.is_dir=1 AND (OLD.parent_id IS NOT NEW.parent_id OR OLD.name<>NEW.name OR OLD.status<>NEW.status)
      AND EXISTS(SELECT 1 FROM json_each(scope,'$.directories') g JOIN app_ancestors a ON a.node_id=CAST(json_extract(g.value,'$.directory_id') AS INTEGER) WHERE a.ancestor_id=NEW.id);
    UPDATE app_node_meta SET revision=lower(hex(randomblob(16))),hidden=substr(NEW.name,1,1)='.',
        content_version=CASE WHEN NEW.is_dir=1 THEN NULL WHEN OLD.size<>NEW.size OR OLD.sha256 IS NOT NEW.sha256 OR (OLD.status='uploading' AND NEW.status='ready') THEN lower(hex(randomblob(16))) ELSE content_version END
    WHERE node_id=NEW.id;
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id IN(OLD.parent_id,NEW.parent_id)
        AND (OLD.status='ready' OR NEW.status='ready');
    DELETE FROM app_ancestors WHERE OLD.parent_id IS NOT NEW.parent_id
        AND node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id)
        AND ancestor_id IN(SELECT ancestor_id FROM app_ancestors WHERE node_id=NEW.id AND ancestor_id<>NEW.id);
    INSERT OR IGNORE INTO app_ancestors
    SELECT parent.ancestor_id,sub.node_id,parent.depth+sub.depth+1 FROM app_ancestors parent CROSS JOIN app_ancestors sub
    WHERE OLD.parent_id IS NOT NEW.parent_id AND parent.node_id=NEW.parent_id AND sub.ancestor_id=NEW.id;
    UPDATE app_suffixes SET parent_id=NEW.parent_id WHERE node_id=NEW.id AND OLD.parent_id IS NOT NEW.parent_id;
    DELETE FROM app_suffixes WHERE node_id=NEW.id AND OLD.name<>NEW.name;
    INSERT OR IGNORE INTO app_suffixes
    SELECT NEW.id,value,NEW.parent_id FROM (
        WITH RECURSIVE suffix(value) AS (
            SELECT substr(lower(NEW.name),instr(NEW.name,'.')+1) WHERE NEW.is_dir=0 AND OLD.name<>NEW.name AND instr(NEW.name,'.')>0
            UNION ALL SELECT substr(value,instr(value,'.')+1) FROM suffix WHERE instr(value,'.')>0
        ) SELECT value FROM suffix
    ) WHERE length(value) BETWEEN 1 AND 40 AND value NOT GLOB '*[^a-z0-9._+-]*' AND value NOT LIKE '.%' AND value NOT LIKE '%.' AND instr(value,'..')=0;
    DELETE FROM app_random_index WHERE (OLD.parent_id IS NOT NEW.parent_id AND node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id)) OR (OLD.name<>NEW.name AND node_id=NEW.id);
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,'',m.random_key,n.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id JOIN nodes n ON n.id=a.node_id
    WHERE n.is_dir=0 AND ((OLD.parent_id IS NOT NEW.parent_id AND a.node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id)) OR (OLD.name<>NEW.name AND a.node_id=NEW.id));
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,s.extension,m.random_key,a.node_id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id JOIN app_suffixes s ON s.node_id=a.node_id
    WHERE (OLD.parent_id IS NOT NEW.parent_id AND a.node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id)) OR (OLD.name<>NEW.name AND a.node_id=NEW.id);
END;
CREATE TRIGGER app_nodes_delete BEFORE DELETE ON nodes BEGIN
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    SELECT CAST(unixepoch('subsec')*1000 AS INTEGER),OLD.id,OLD.is_dir,OLD.is_dir,
        (SELECT json_group_array(ancestor_id) FROM app_ancestors WHERE node_id=OLD.id),
        (SELECT '/'||group_concat(name,'/') FROM (SELECT n.name FROM app_ancestors a JOIN nodes n ON n.id=a.ancestor_id WHERE a.node_id=OLD.id AND n.id<>1 ORDER BY a.depth DESC))
    WHERE OLD.status='ready' AND (NOT EXISTS(SELECT 1 FROM app_change_batches) OR EXISTS(SELECT 1 FROM app_change_batches WHERE root_id=OLD.id));
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id=OLD.parent_id AND OLD.status='ready';
    UPDATE app_installations SET scope_revision=lower(hex(randomblob(16)))
    WHERE OLD.is_dir=1 AND EXISTS(SELECT 1 FROM json_each(scope,'$.directories') g JOIN app_ancestors a ON a.node_id=CAST(json_extract(g.value,'$.directory_id') AS INTEGER) WHERE a.ancestor_id=OLD.id);
END;
-- 原文件附件改变属于通用元数据变化，不改变原文件内容版本，也不解析附件业务。
CREATE TRIGGER app_thumbnail_insert AFTER INSERT ON chunks
WHEN NEW.idx=0 AND NEW.tg_thumbnail_file_id IS NOT NULL AND EXISTS(SELECT 1 FROM nodes WHERE id=NEW.node_id AND status='ready') BEGIN
    UPDATE app_node_meta SET revision=lower(hex(randomblob(16))) WHERE node_id=NEW.node_id;
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id=(SELECT parent_id FROM nodes WHERE id=NEW.node_id);
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    VALUES(CAST(unixepoch('subsec')*1000 AS INTEGER),NEW.node_id,0,0,(SELECT json_group_array(ancestor_id) FROM app_ancestors WHERE node_id=NEW.node_id),NULL);
END;
CREATE TRIGGER app_thumbnail_update AFTER UPDATE OF tg_thumbnail_file_id ON chunks
WHEN NEW.idx=0 AND OLD.tg_thumbnail_file_id IS NOT NEW.tg_thumbnail_file_id AND EXISTS(SELECT 1 FROM nodes WHERE id=NEW.node_id AND status='ready') BEGIN
    UPDATE app_node_meta SET revision=lower(hex(randomblob(16))) WHERE node_id=NEW.node_id;
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id=(SELECT parent_id FROM nodes WHERE id=NEW.node_id);
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    VALUES(CAST(unixepoch('subsec')*1000 AS INTEGER),NEW.node_id,0,0,(SELECT json_group_array(ancestor_id) FROM app_ancestors WHERE node_id=NEW.node_id),NULL);
END;
CREATE TRIGGER app_thumbnail_delete AFTER DELETE ON chunks
WHEN OLD.idx=0 AND OLD.tg_thumbnail_file_id IS NOT NULL AND EXISTS(SELECT 1 FROM nodes WHERE id=OLD.node_id AND status='ready') BEGIN
    UPDATE app_node_meta SET revision=lower(hex(randomblob(16))) WHERE node_id=OLD.node_id;
    UPDATE app_node_meta SET children_revision=lower(hex(randomblob(16))) WHERE node_id=(SELECT parent_id FROM nodes WHERE id=OLD.node_id);
    INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path)
    VALUES(CAST(unixepoch('subsec')*1000 AS INTEGER),OLD.node_id,0,0,(SELECT json_group_array(ancestor_id) FROM app_ancestors WHERE node_id=OLD.node_id),NULL);
END;
