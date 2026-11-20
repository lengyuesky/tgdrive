-- 事务幂等绑定和上传句柄均按应用隔离，卸载保留数据时不级联丢失已提交回执。
CREATE TABLE app_write_requests (
    app_id TEXT NOT NULL,
    key TEXT NOT NULL,
    generation TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN('mutation','upload')),
    request TEXT NOT NULL,
    receipt TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY(app_id,key)
) WITHOUT ROWID;
CREATE INDEX app_write_requests_expiry ON app_write_requests(expires_at);
CREATE TABLE app_uploads (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    generation TEXT NOT NULL,
    caller_id TEXT NOT NULL,
    caller_kind TEXT NOT NULL CHECK(caller_kind IN('page','backend')),
    idempotency_key TEXT NOT NULL,
    target TEXT NOT NULL,
    scope_revision TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size>=0),
    sha256 TEXT,
    received_bytes INTEGER NOT NULL DEFAULT 0 CHECK(received_bytes>=0),
    state TEXT NOT NULL CHECK(state IN('awaiting_body','receiving','ready','committed','failed','cancelled')),
    staging_node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    result TEXT,
    error_code TEXT,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX app_uploads_active ON app_uploads(app_id,state,expires_at);
CREATE INDEX app_uploads_staging ON app_uploads(staging_node_id);
CREATE INDEX app_uploads_pending ON app_uploads(id) WHERE state IN('awaiting_body','receiving','ready');
CREATE INDEX app_uploads_expiry ON app_uploads(state,expires_at);

-- 写事务沿用目录原语和既有索引，只收窄随机索引维护的访问路径。
DROP TRIGGER app_nodes_update;
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
    -- 将带门控的 OR 分成两组索引寻址，单文件移动/改名不扫描整个随机索引。
    DELETE FROM app_random_index WHERE OLD.parent_id IS NOT NEW.parent_id
        AND node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id);
    DELETE FROM app_random_index WHERE OLD.name<>NEW.name AND node_id=NEW.id;
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,'',m.random_key,n.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id JOIN nodes n ON n.id=a.node_id
    WHERE OLD.parent_id IS NOT NEW.parent_id AND n.is_dir=0
        AND a.node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id);
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,'',m.random_key,NEW.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id
    WHERE OLD.name<>NEW.name AND NEW.is_dir=0 AND a.node_id=NEW.id;
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,s.extension,m.random_key,a.node_id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id JOIN app_suffixes s ON s.node_id=a.node_id
    WHERE OLD.parent_id IS NOT NEW.parent_id
        AND a.node_id IN(SELECT node_id FROM app_ancestors WHERE ancestor_id=NEW.id);
    INSERT OR IGNORE INTO app_random_index
    SELECT a.ancestor_id,s.extension,m.random_key,NEW.id FROM app_ancestors a JOIN app_node_meta m ON m.node_id=a.node_id JOIN app_suffixes s ON s.node_id=a.node_id
    WHERE OLD.name<>NEW.name AND a.node_id=NEW.id;
END;
