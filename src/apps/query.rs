//! SQL 下推筛选、键集分页、直属匹配、索引随机环与持久变更游标。

use super::cursor::{CursorCodec, CursorData, binding};
use super::store::{self, Installation, NODE_COLUMNS, NodeRow};
use super::{AppPlatform, PlatformError, PlatformResult, now};
use rand::RngExt;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use std::collections::BTreeSet;
use tgdrive_contract::files::*;
use tgdrive_contract::validation::{
    effective_access, validate_directories_query, validate_file_query, validate_random_request,
};
use tgdrive_contract::{error_code, http::*};

fn ids(query: &mut QueryBuilder<Sqlite>, values: &[i64]) {
    let mut separated = query.separated(",");
    for value in values {
        separated.push_bind(*value);
    }
    if values.is_empty() {
        separated.push("NULL");
    }
}
fn visible(query: &mut QueryBuilder<Sqlite>, alias: &str, hidden: bool) {
    query.push(format!(" AND {alias}.status='ready' AND NOT EXISTS(SELECT 1 FROM app_ancestors va JOIN nodes vn ON vn.id=va.ancestor_id WHERE va.node_id={alias}.id AND vn.status<>'ready')"));
    if !hidden {
        query.push(format!(" AND NOT EXISTS(SELECT 1 FROM app_ancestors ha JOIN app_node_meta hm ON hm.node_id=ha.ancestor_id WHERE ha.node_id={alias}.id AND hm.hidden=1)"));
    }
}
fn filter(query: &mut QueryBuilder<Sqlite>, alias: &str, filter: &FileFilter) {
    match filter.kind {
        QueryKind::File => {
            query.push(format!(" AND {alias}.is_dir=0"));
        }
        QueryKind::Directory => {
            query.push(format!(" AND {alias}.is_dir=1"));
        }
        QueryKind::All => {}
    }
    visible(query, alias, filter.include_hidden);
    if let Some(name) = &filter.name_contains {
        query
            .push(format!(" AND instr(lower({alias}.name),"))
            .push_bind(name.to_ascii_lowercase())
            .push(")>0");
    }
    if !filter.extensions.is_empty() {
        query.push(format!(" AND ({alias}.is_dir=1 OR EXISTS(SELECT 1 FROM app_suffixes sx WHERE sx.node_id={alias}.id AND sx.extension IN("));
        let mut separated = query.separated(",");
        for extension in &filter.extensions {
            separated.push_bind(extension.clone());
        }
        separated.push_unseparated(")))");
    }
    if let Some(size) = &filter.size {
        if let Some(min) = size.min {
            query
                .push(format!(
                    " AND (CASE WHEN {alias}.is_dir=1 THEN 0 ELSE {alias}.size END)>="
                ))
                .push_bind(min.0 as i64);
        }
        if let Some(max) = size.max {
            query
                .push(format!(
                    " AND (CASE WHEN {alias}.is_dir=1 THEN 0 ELSE {alias}.size END)<="
                ))
                .push_bind(max.0 as i64);
        }
    }
    for (column, bounds) in [
        ("created_at", &filter.created_at),
        ("modified_at", &filter.modified_at),
    ] {
        if let Some(bounds) = bounds {
            if let Some(from) = bounds.from {
                query
                    .push(format!(" AND {alias}.{column}>="))
                    .push_bind(from.0 / 1000 + i64::from(from.0 % 1000 != 0));
            }
            if let Some(to) = bounds.to {
                query
                    .push(format!(" AND {alias}.{column}<="))
                    .push_bind(to.0 / 1000);
            }
        }
    }
}
fn root_condition(
    query: &mut QueryBuilder<Sqlite>,
    roots: &[i64],
    recursive: bool,
    include_roots: bool,
) {
    if recursive {
        query.push(" AND EXISTS(SELECT 1 FROM app_ancestors qr WHERE qr.node_id=n.id AND qr.ancestor_id IN(");
        ids(query, roots);
        query.push(if include_roots {
            "))"
        } else {
            ") AND qr.depth>0)"
        });
    } else {
        query.push(" AND n.parent_id IN(");
        ids(query, roots);
        query.push(")");
    }
}
fn index(sort: NodeSort, direct: bool) -> Option<&'static str> {
    match (sort, direct) {
        (NodeSort::Name, false) => Some("app_nodes_name"),
        (NodeSort::Size, false) => Some("app_nodes_size"),
        (NodeSort::CreatedAt, false) => Some("app_nodes_created"),
        (NodeSort::ModifiedAt, false) => Some("app_nodes_modified"),
        (NodeSort::Name, true) => Some("app_nodes_parent_name"),
        (NodeSort::Size, true) => Some("app_nodes_parent_size"),
        (NodeSort::CreatedAt, true) => Some("app_nodes_parent_created"),
        (NodeSort::ModifiedAt, true) => Some("app_nodes_parent_modified"),
        (NodeSort::Id, _) => None,
    }
}
fn directory_index(sort: NodeSort) -> Option<&'static str> {
    match sort {
        NodeSort::Name => Some("app_dirs_name"),
        NodeSort::CreatedAt => Some("app_dirs_created"),
        NodeSort::ModifiedAt => Some("app_dirs_modified"),
        NodeSort::Id => None,
        NodeSort::Size => Some("app_nodes_size"),
    }
}
fn sort_expression(sort: NodeSort) -> &'static str {
    match sort {
        NodeSort::Name => "lower(n.name) COLLATE BINARY,n.name COLLATE BINARY,n.id",
        NodeSort::Size => "n.size,n.id",
        NodeSort::CreatedAt => "n.created_at,n.id",
        NodeSort::ModifiedAt => "n.modified_at,n.id",
        NodeSort::Id => "n.id",
    }
}
fn last_key(row: &NodeRow, sort: NodeSort) -> serde_json::Value {
    match sort {
        NodeSort::Name => serde_json::json!([row.name.to_ascii_lowercase(), row.name, row.id]),
        NodeSort::Size => serde_json::json!([row.size, row.id]),
        NodeSort::CreatedAt => serde_json::json!([row.created_at, row.id]),
        NodeSort::ModifiedAt => serde_json::json!([row.modified_at, row.id]),
        NodeSort::Id => serde_json::json!([row.id]),
    }
}
fn order(
    query: &mut QueryBuilder<Sqlite>,
    sort: NodeSort,
    direction: SortOrder,
    key: Option<&serde_json::Value>,
) -> PlatformResult<()> {
    if let Some(key) = key {
        let values = key
            .as_array()
            .ok_or_else(|| PlatformError::new(error_code::INVALID_CURSOR, "游标排序键无效"))?;
        let expected = match sort {
            NodeSort::Name => 3,
            NodeSort::Id => 1,
            _ => 2,
        };
        if values.len() != expected {
            return Err(PlatformError::new(
                error_code::INVALID_CURSOR,
                "游标排序键无效",
            ));
        }
        if sort == NodeSort::Name {
            let folded = values[0].as_str().ok_or_else(|| {
                PlatformError::new(error_code::INVALID_CURSOR, "名称游标主键无效")
            })?;
            // SQLite 对表达式索引上的行值比较只做过滤；显式主键范围确保从游标位置寻址。
            query
                .push(" AND lower(n.name) COLLATE BINARY")
                .push(if direction == SortOrder::Asc {
                    ">="
                } else {
                    "<="
                })
                .push_bind(folded.to_owned());
        }
        query
            .push(" AND (")
            .push(sort_expression(sort))
            .push(if direction == SortOrder::Asc {
                ")>("
            } else {
                ")<("
            });
        for (position, value) in values.iter().enumerate() {
            if position > 0 {
                query.push(",");
            }
            if let Some(text) = value.as_str() {
                query.push_bind(text.to_owned());
            } else if let Some(number) = value.as_i64() {
                query.push_bind(number);
            } else {
                return Err(PlatformError::new(
                    error_code::INVALID_CURSOR,
                    "游标排序键无效",
                ));
            }
        }
        query.push(")");
    }
    query.push(" ORDER BY ");
    let suffix = if direction == SortOrder::Asc {
        " ASC"
    } else {
        " DESC"
    };
    for (position, expression) in sort_expression(sort).split(',').enumerate() {
        if position > 0 {
            query.push(",");
        }
        query.push(expression).push(suffix);
    }
    Ok(())
}

pub(crate) async fn checkpoint(
    connection: &mut SqliteConnection,
    codec: &CursorCodec,
    app: &Installation,
) -> PlatformResult<String> {
    let epoch: String = sqlx::query_scalar("SELECT epoch FROM app_change_state WHERE id=1")
        .fetch_one(&mut *connection)
        .await?;
    let seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='app_file_changes'),0)",
    )
    .fetch_one(connection)
    .await?;
    codec.encode(&CursorData {
        kind: "changes".into(),
        app: app.id.clone(),
        generation: app.generation.clone(),
        scope: Some(app.scope_revision.clone()),
        expires_at: Some(now().0 + FILE_CHANGE_RETENTION_SECONDS as i64 * 1000),
        binding: String::new(),
        position: serde_json::json!(seq),
        scan: None,
        epoch: Some(epoch),
        high_water: None,
    })
}

struct PageContext {
    limit: u16,
    sort: NodeSort,
    kind: &'static str,
    binding: String,
    scan: String,
}

impl AppPlatform {
    pub async fn file_query(
        &self,
        app_id: &str,
        generation: &str,
        mut request: FileQuery,
    ) -> PlatformResult<FilePage> {
        validate_file_query(&request)?;
        let token = request.cursor.take();
        let fingerprint = binding(&request)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let cursor = token
            .as_deref()
            .map(|token| self.cursors.decode(token))
            .transpose()?;
        if let Some(cursor) = &cursor {
            CursorCodec::check(cursor, "query", &app, &fingerprint, true)?;
        }
        let roots = store::query_roots(&mut tx, &app, &request.roots).await?;
        let scan = if let Some(scan) = cursor.as_ref().and_then(|c| c.scan.clone()) {
            scan
        } else {
            checkpoint(&mut tx, &self.cursors, &app).await?
        };
        let direct = !request.recursive && roots.len() == 1;
        let mut query = QueryBuilder::<Sqlite>::new(format!("SELECT {NODE_COLUMNS} FROM nodes n"));
        if let Some(index) = if request.filter.kind == QueryKind::Directory {
            directory_index(request.sort)
        } else {
            index(request.sort, direct)
        } {
            query.push(format!(" INDEXED BY {index}"));
        }
        query.push(" JOIN app_node_meta m ON m.node_id=n.id WHERE n.status='ready'");
        root_condition(&mut query, &roots, request.recursive, false);
        filter(&mut query, "n", &request.filter);
        order(
            &mut query,
            request.sort,
            request.order,
            cursor.as_ref().map(|c| &c.position),
        )?;
        query
            .push(" LIMIT ")
            .push_bind(i64::from(request.limit) + 1);
        #[cfg(test)]
        capture_plan(&mut query, &mut tx, "query").await?;
        let rows: Vec<NodeRow> = query.build_query_as().fetch_all(&mut *tx).await?;
        let page = self
            .page(
                &mut tx,
                &app,
                rows,
                PageContext {
                    limit: request.limit,
                    sort: request.sort,
                    kind: "query",
                    binding: fingerprint,
                    scan,
                },
            )
            .await?;
        tx.commit().await?;
        Ok(page)
    }
    async fn page(
        &self,
        connection: &mut SqliteConnection,
        app: &Installation,
        rows: Vec<NodeRow>,
        context: PageContext,
    ) -> PlatformResult<FilePage> {
        let PageContext {
            limit,
            sort,
            kind,
            binding,
            scan,
        } = context;
        let mut items = store::file_nodes(connection, rows.clone()).await?;
        let count = tgdrive_contract::validation::json_page_prefix_count(
            &items,
            usize::from(limit),
            MAX_JSON_PAGE_BYTES,
        )?;
        let more = items.len() > count;
        items.truncate(count);
        let next_cursor = if more {
            let row = &rows[count - 1];
            Some(self.cursors.encode(&CursorData {
                kind: kind.into(),
                app: app.id.clone(),
                generation: app.generation.clone(),
                scope: Some(app.scope_revision.clone()),
                expires_at: Some(now().0 + PAGE_CURSOR_SECONDS as i64 * 1000),
                binding,
                position: last_key(row, sort),
                scan: Some(scan.clone()),
                epoch: None,
                high_water: None,
            })?)
        } else {
            None
        };
        Ok(FilePage {
            items,
            next_cursor,
            scan_cursor: scan,
            scope_revision: app.scope_revision.clone(),
        })
    }
    pub async fn list_directory(
        &self,
        app_id: &str,
        generation: &str,
        request: ListDirectoryRequest,
    ) -> PlatformResult<DirectoryPage> {
        let mut connection = self.host.vfs.db.pool.acquire().await?;
        let app = store::installation(&mut connection, app_id).await?;
        app.check(generation)?;
        let directory = store::reference(&mut connection, &app, &request.directory).await?;
        if directory.kind != NodeKind::Directory {
            return Err(PlatformError::new(
                error_code::NOT_DIRECTORY,
                "节点不是目录",
            ));
        }
        drop(connection);
        let page = self
            .file_query(
                app_id,
                generation,
                FileQuery {
                    roots: vec![directory.id.clone()],
                    recursive: false,
                    filter: FileFilter {
                        kind: QueryKind::All,
                        include_hidden: true,
                        ..FileFilter::default()
                    },
                    sort: request.sort,
                    order: request.order,
                    limit: request.limit,
                    cursor: request.cursor,
                },
            )
            .await?;
        Ok(DirectoryPage {
            directory,
            items: page.items,
            next_cursor: page.next_cursor,
            scan_cursor: page.scan_cursor,
            scope_revision: page.scope_revision,
        })
    }
    pub async fn directories_with_files(
        &self,
        app_id: &str,
        generation: &str,
        mut request: DirectoriesWithFilesQuery,
    ) -> PlatformResult<FilePage> {
        validate_directories_query(&request)?;
        let token = request.cursor.take();
        let fingerprint = binding(&request)?;
        let sort = match request.sort {
            DirectorySort::Name => NodeSort::Name,
            DirectorySort::CreatedAt => NodeSort::CreatedAt,
            DirectorySort::ModifiedAt => NodeSort::ModifiedAt,
            DirectorySort::Id => NodeSort::Id,
        };
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let cursor = token
            .as_deref()
            .map(|token| self.cursors.decode(token))
            .transpose()?;
        if let Some(cursor) = &cursor {
            CursorCodec::check(cursor, "directories", &app, &fingerprint, true)?;
        }
        let roots = store::query_roots(&mut tx, &app, &request.roots).await?;
        let scan = if let Some(scan) = cursor.as_ref().and_then(|c| c.scan.clone()) {
            scan
        } else {
            checkpoint(&mut tx, &self.cursors, &app).await?
        };
        let mut query = QueryBuilder::<Sqlite>::new(format!("SELECT {NODE_COLUMNS} FROM nodes n"));
        if let Some(index) = directory_index(sort) {
            query.push(format!(" INDEXED BY {index}"));
        }
        query.push(" JOIN app_node_meta m ON m.node_id=n.id WHERE n.status='ready' AND n.is_dir=1");
        root_condition(&mut query, &roots, true, true);
        visible(&mut query, "n", request.file_filter.include_hidden);
        if let Some(name) = request.directory_name_contains {
            query
                .push(" AND instr(lower(n.name),")
                .push_bind(name.to_ascii_lowercase())
                .push(")>0");
        }
        query.push(" AND EXISTS(SELECT 1 FROM app_suffixes ds INDEXED BY app_suffixes_parent CROSS JOIN nodes f ON f.id=ds.node_id WHERE ds.parent_id=n.id AND ds.extension IN(");
        let mut extensions = query.separated(",");
        for extension in &request.file_filter.extensions {
            extensions.push_bind(extension.clone());
        }
        extensions.push_unseparated(") AND f.parent_id=n.id AND f.status='ready'");
        filter(&mut query, "f", &request.file_filter);
        query.push(")");
        order(
            &mut query,
            sort,
            request.order,
            cursor.as_ref().map(|c| &c.position),
        )?;
        query
            .push(" LIMIT ")
            .push_bind(i64::from(request.limit) + 1);
        #[cfg(test)]
        capture_plan(&mut query, &mut tx, "directories-with-files").await?;
        let rows: Vec<NodeRow> = query.build_query_as().fetch_all(&mut *tx).await?;
        let result = self
            .page(
                &mut tx,
                &app,
                rows,
                PageContext {
                    limit: request.limit,
                    sort,
                    kind: "directories",
                    binding: fingerprint,
                    scan,
                },
            )
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn random_files(
        &self,
        app_id: &str,
        generation: &str,
        request: RandomFilesRequest,
    ) -> PlatformResult<RandomFilesResponse> {
        validate_random_request(&request)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let roots = store::query_roots(&mut tx, &app, &request.roots).await?;
        let extensions = if request.filter.extensions.is_empty() {
            vec![String::new()]
        } else {
            request.filter.extensions.clone()
        };
        let seed = rand::rng().random_range(0..i64::MAX);
        let mut buckets = roots
            .iter()
            .flat_map(|root| {
                extensions
                    .iter()
                    .map(move |extension| (*root, extension.clone()))
            })
            .collect::<Vec<_>>();
        use rand::seq::SliceRandom;
        buckets.shuffle(&mut rand::rng());
        let mut chosen = BTreeSet::new();
        let mut budget = MAX_RANDOM_CANDIDATE_STEPS as usize;
        for wrap in [false, true] {
            for (root, extension) in &buckets {
                let mut last: Option<(i64, i64)> = None;
                loop {
                    if chosen.len() >= usize::from(request.limit) {
                        break;
                    }
                    if budget == 0 {
                        return Err(PlatformError::new(
                            error_code::QUOTA_EXCEEDED,
                            "随机查询候选预算已耗尽，请缩小筛选范围",
                        ));
                    }
                    let batch = budget.min((usize::from(request.limit) - chosen.len()).max(16) * 2);
                    let mut query = QueryBuilder::<Sqlite>::new(
                        "WITH candidates AS MATERIALIZED(SELECT node_id,random_key FROM app_random_index WHERE root_id=",
                    );
                    query
                        .push_bind(*root)
                        .push(" AND extension=")
                        .push_bind(extension.clone());
                    query
                        .push(if wrap {
                            " AND random_key<"
                        } else {
                            " AND random_key>="
                        })
                        .push_bind(seed);
                    if let Some((key, id)) = last {
                        query
                            .push(" AND (random_key,node_id)>(")
                            .push_bind(key)
                            .push(",")
                            .push_bind(id)
                            .push(")");
                    }
                    query.push(" ORDER BY random_key,node_id LIMIT ").push_bind(batch as i64).push(") SELECT (SELECT count(*) FROM candidates),COALESCE((SELECT random_key FROM candidates ORDER BY random_key DESC,node_id DESC LIMIT 1),0),COALESCE((SELECT node_id FROM candidates ORDER BY random_key DESC,node_id DESC LIMIT 1),0),(SELECT json_group_array(n.id) FROM candidates c CROSS JOIN nodes n ON n.id=c.node_id WHERE 1=1");
                    filter(&mut query, "n", &request.filter);
                    if !request.exclude_ids.is_empty() {
                        query.push(" AND n.id NOT IN(");
                        let exclude = request
                            .exclude_ids
                            .iter()
                            .map(|id| store::node_id(id))
                            .collect::<PlatformResult<Vec<_>>>()?;
                        ids(&mut query, &exclude);
                        query.push(")");
                    }
                    query.push(")");
                    #[cfg(test)]
                    capture_plan(&mut query, &mut tx, "random").await?;
                    let (steps, key, id, hits): (i64, i64, i64, String) =
                        query.build_query_as().fetch_one(&mut *tx).await?;
                    budget -= steps as usize;
                    let hits: Vec<i64> = serde_json::from_str(&hits)?;
                    for id in hits {
                        chosen.insert(id);
                        if chosen.len() == usize::from(request.limit) {
                            break;
                        }
                    }
                    if steps < (batch as i64) {
                        break;
                    }
                    last = Some((key, id));
                }
                if chosen.len() >= usize::from(request.limit) {
                    break;
                }
            }
            if chosen.len() >= usize::from(request.limit) {
                break;
            }
        }
        let mut rows = Vec::with_capacity(chosen.len());
        for id in chosen {
            rows.push(store::node_row(&mut tx, id).await?);
        }
        let result = RandomFilesResponse {
            items: store::file_nodes(&mut tx, rows).await?,
            scope_revision: app.scope_revision,
        };
        tx.commit().await?;
        Ok(result)
    }
    pub async fn file_changes(
        &self,
        app_id: &str,
        generation: &str,
        request: FileChangesRequest,
    ) -> PlatformResult<FileChangesPage> {
        if request.limit == 0 || u64::from(request.limit) > MAX_PAGE_LIMIT {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "变更页大小无效",
            ));
        }
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        store::query_roots(&mut tx, &app, &[]).await?;
        let current = checkpoint(&mut tx, &self.cursors, &app).await?;
        let Some(token) = request.cursor else {
            return Ok(FileChangesPage {
                changes: Vec::new(),
                next_cursor: current,
                has_more: false,
                reset_required: true,
                scope_revision: app.scope_revision,
            });
        };
        let mut cursor = self.cursors.decode(&token)?;
        let latest = self.cursors.decode(&current)?;
        if cursor.kind != "changes" || cursor.app != app.id || !cursor.binding.is_empty() {
            return Err(PlatformError::new(
                error_code::INVALID_CURSOR,
                "变更游标用途无效",
            ));
        }
        if cursor.epoch != latest.epoch {
            let mut error =
                PlatformError::new(error_code::CURSOR_EXPIRED, "恢复或日志清除使旧变更游标失效");
            error.details = Some(serde_json::json!({"checkpoint":current}));
            return Err(error);
        }
        CursorCodec::check(&cursor, "changes", &app, "", true)?;
        let position = cursor
            .position
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or_else(|| PlatformError::new(error_code::INVALID_CURSOR, "变更游标位置无效"))?;
        let minimum: Option<i64> = sqlx::query_scalar("SELECT min(seq) FROM app_file_changes")
            .fetch_one(&mut *tx)
            .await?;
        let maximum = latest.position.as_i64().expect("宿主签发整数位置");
        let trimmed: i64 =
            sqlx::query_scalar("SELECT trimmed_seq FROM app_change_state WHERE id=1")
                .fetch_one(&mut *tx)
                .await?;
        let expired:i64=sqlx::query_scalar("SELECT COALESCE(max(seq),0) FROM app_file_changes INDEXED BY app_file_changes_time WHERE occurred_at<?")
            .bind(now().0-(FILE_CHANGE_RETENTION_SECONDS*1000) as i64).fetch_one(&mut *tx).await?;
        if cursor.epoch != latest.epoch
            || position > maximum
            || position < trimmed.max(expired)
            || minimum.is_some_and(|minimum| position < minimum - 1)
        {
            let mut error = PlatformError::new(
                error_code::CURSOR_EXPIRED,
                "变更日志已过保留期或恢复，请重新完整核对",
            );
            error.details = Some(serde_json::json!({"checkpoint":current}));
            return Err(error);
        }
        let high_water = cursor.high_water.unwrap_or(maximum);
        let rows: Vec<ChangeRow> = sqlx::query_as(
            "SELECT * FROM app_file_changes WHERE seq>? AND seq<=? ORDER BY seq LIMIT ?",
        )
        .bind(position)
        .bind(high_water)
        .bind(i64::from(request.limit))
        .fetch_all(&mut *tx)
        .await?;
        let mut checked = position;
        let mut changes = Vec::new();
        let mut before_change = Vec::new();
        let scope = app.directory_scope()?;
        let permissions = app.manifest()?.files;
        for row in rows {
            let previous_checked = checked;
            let previous_count = changes.len();
            checked = row.seq;
            let previous: Vec<i64> = serde_json::from_str(&row.previous_ancestors)?;
            let previous = previous
                .into_iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>();
            let previous_refs = previous.iter().map(String::as_str).collect::<Vec<_>>();
            let was_visible = effective_access(&scope, &permissions, &previous_refs).is_some();
            let current = match store::node_row(&mut tx, row.node_id).await {
                Ok(node) if store::access(&mut tx, &app, row.node_id).await.is_ok() => {
                    store::file_nodes(&mut tx, vec![node]).await?.pop()
                }
                Ok(_) => None,
                Err(error) if error.code == error_code::NOT_FOUND => None,
                Err(error) => return Err(error),
            };
            if row.structural && row.is_dir {
                if was_visible || current.is_some() {
                    changes.push(FileChange::SubtreeInvalidated {
                        id: row.node_id.to_string(),
                        previous_path: if was_visible { row.previous_path } else { None },
                        current,
                    });
                }
            } else if let Some(node) = current {
                changes.push(FileChange::Upsert { node });
            } else if was_visible {
                changes.push(FileChange::Removed {
                    id: row.node_id.to_string(),
                });
            }
            if changes.len() > previous_count {
                before_change.push(previous_checked);
            }
        }
        let count = tgdrive_contract::validation::json_page_prefix_count(
            &changes,
            usize::from(request.limit),
            MAX_JSON_PAGE_BYTES,
        )?;
        if count < changes.len() {
            checked = before_change[count];
            changes.truncate(count);
        }
        // 无相关授权变化时仍推进已检查位置，持续写入不改变本轮高水位。
        if checked == position {
            checked = high_water;
        }
        let has_more = checked < high_water;
        cursor.position = serde_json::json!(checked);
        cursor.expires_at = Some(now().0 + FILE_CHANGE_RETENTION_SECONDS as i64 * 1000);
        cursor.high_water = if has_more { Some(high_water) } else { None };
        let next_cursor = self.cursors.encode(&cursor)?;
        tx.commit().await?;
        Ok(FileChangesPage {
            changes,
            next_cursor,
            has_more,
            reset_required: false,
            scope_revision: app.scope_revision,
        })
    }
}

#[cfg(test)]
pub(crate) static CAPTURE_PLANS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
pub(crate) static PLANS: std::sync::Mutex<Vec<(String, Vec<String>)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
async fn capture_plan(
    query: &mut QueryBuilder<Sqlite>,
    connection: &mut SqliteConnection,
    label: &str,
) -> PlatformResult<()> {
    if !CAPTURE_PLANS.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    use sqlx::{Execute, Row};
    let sql = query.sql().as_str().to_owned();
    let mut built = query.build();
    let arguments = built
        .take_arguments()
        .map_err(|_| PlatformError::new(error_code::INTERNAL_ERROR, "执行计划参数无效"))?
        .unwrap_or_default();
    let rows = sqlx::query_with(
        sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")),
        arguments.clone(),
    )
    .fetch_all(connection)
    .await?;
    let plan = rows
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect();
    let captured = if sql.contains(" AND lower(n.name) COLLATE BINARY>=")
        || sql.contains(" AND lower(n.name) COLLATE BINARY<=")
    {
        format!("{label}.cursor")
    } else {
        label.into()
    };
    PLANS.lock().unwrap().push((captured, plan));
    *query = QueryBuilder::with_arguments(sql, arguments);
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ChangeRow {
    seq: i64,
    node_id: i64,
    is_dir: bool,
    structural: bool,
    previous_ancestors: String,
    previous_path: Option<String>,
}
