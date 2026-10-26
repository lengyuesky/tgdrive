//! 搜索与秒传。

use super::*;

impl Db {
    /// 兼容原有调用的按名称搜索。
    #[cfg(test)]
    pub async fn search(&self, query: &str, limit: usize) -> AppResult<Vec<(Node, String)>> {
        self.search_filtered(
            &SearchFilter {
                query: Some(query.to_string()),
                ..SearchFilter::default()
            },
            limit,
        )
        .await
    }

    /// 在 SQLite 内完成高级筛选、排序和路径拼接，排除回收站与上传中的子树。
    pub async fn search_filtered(
        &self,
        filter: &SearchFilter,
        limit: usize,
    ) -> AppResult<Vec<(Node, String)>> {
        // 只让 SQLite 遍历从 ready 根可达的节点并在库内拼路径，避免节点量大时
        // 每次搜索把整张 nodes 表及所有字符串复制进应用内存。
        // 限定目录时直接从该目录起递归，只遍历子树；目录不存在或不可见则无结果。
        let (seed_id, seed_path) = match filter.under.as_deref().filter(|d| *d != "/") {
            Some(dir) => {
                let segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
                match self.resolve(&segments).await? {
                    Some(node) if node.is_dir => (node.id, dir.to_string()),
                    _ => return Ok(Vec::new()),
                }
            }
            None => (ROOT_ID, String::new()),
        };
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "WITH RECURSIVE visible(
                id, parent_id, name, is_dir, size, status,
                created_at, modified_at, sha256, favorite, path
             ) AS (
                SELECT id, parent_id, name, is_dir, size, status,
                       created_at, modified_at, sha256, favorite, ",
        );
        query
            .push_bind(seed_path)
            .push(" FROM nodes WHERE id = ")
            .push_bind(seed_id)
            .push(
                " AND status = 'ready'
                UNION ALL
                SELECT n.id, n.parent_id, n.name, n.is_dir, n.size, n.status,
                       n.created_at, n.modified_at, n.sha256, n.favorite,
                       visible.path || '/' || n.name
                FROM nodes n JOIN visible ON n.parent_id = visible.id
                WHERE n.status = 'ready'
             )
             SELECT id, parent_id, name, is_dir, size, status,
                    created_at, modified_at, sha256, favorite, path
             FROM visible
             WHERE id != ",
            );
        query.push_bind(seed_id);
        if let Some(needle) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            query
                .push(" AND instr(lower(name), lower(")
                .push_bind(needle)
                .push(")) > 0");
        }
        if let Some(is_dir) = filter.is_dir {
            query.push(" AND is_dir = ").push_bind(is_dir);
        }
        if !filter.extensions.is_empty() {
            query.push(" AND is_dir = 0 AND (");
            for (index, extension) in filter.extensions.iter().enumerate() {
                if index > 0 {
                    query.push(" OR ");
                }
                query
                    .push("lower(name) GLOB ")
                    .push_bind(format!("*.{}", extension.to_ascii_lowercase()));
            }
            query.push(")");
        }
        if let Some(size) = filter.min_size {
            query.push(" AND is_dir = 0 AND size >= ").push_bind(size);
        }
        if let Some(size) = filter.max_size {
            query.push(" AND is_dir = 0 AND size <= ").push_bind(size);
        }
        if let Some(ts) = filter.modified_after {
            query.push(" AND modified_at >= ").push_bind(ts);
        }
        if let Some(ts) = filter.modified_before {
            query.push(" AND modified_at <= ").push_bind(ts);
        }
        if filter.favorite_only {
            query.push(" AND favorite = 1");
        }
        query.push(" ORDER BY ");
        match filter.sort {
            SearchSort::Name => query.push("name COLLATE NOCASE"),
            SearchSort::Size => query.push("size"),
            SearchSort::Modified => query.push("modified_at"),
            SearchSort::Created => query.push("created_at"),
        };
        query.push(if filter.descending { " DESC" } else { " ASC" });
        query
            .push(", id LIMIT ")
            .push_bind(limit.min(i64::MAX as usize) as i64);

        let rows: Vec<SearchRow> = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let path = row.path;
                (
                    Node {
                        id: row.id,
                        parent_id: row.parent_id,
                        name: row.name,
                        is_dir: row.is_dir,
                        size: row.size,
                        status: row.status,
                        created_at: row.created_at,
                        modified_at: row.modified_at,
                        sha256: row.sha256,
                        favorite: row.favorite,
                    },
                    path,
                )
            })
            .collect())
    }

    /// 秒传:找到内容相同(sha256+size)的既有文件则零流量链接到目标位置,
    /// 覆盖同名旧文件;返回 (新节点 id, 释放的 message_id)。找不到返回 None。
    #[cfg(test)]
    pub async fn instant_link(
        &self,
        sha256: &str,
        size: i64,
        dst_parent: i64,
        dst_name: &str,
    ) -> AppResult<Option<(i64, Vec<StoredMessage>)>> {
        self.instant_link_with_policy(sha256, size, dst_parent, dst_name, true)
            .await
    }

    /// 目标检查、同内容幂等和覆盖决策必须都在同一写事务，不能用事务外快照删除目录或源。
    pub async fn instant_link_with_policy(
        &self,
        sha256: &str,
        size: i64,
        dst_parent: i64,
        dst_name: &str,
        overwrite: bool,
    ) -> AppResult<Option<(i64, Vec<StoredMessage>)>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let parent: Option<bool> =
            sqlx::query_scalar("SELECT is_dir FROM nodes WHERE id = ? AND status = 'ready'")
                .bind(dst_parent)
                .fetch_optional(&mut *tx)
                .await?;
        if parent != Some(true) {
            return Err(AppError::NotDirectory);
        }
        let old: Option<Node> = sqlx::query_as(
            "SELECT * FROM nodes WHERE parent_id = ? AND name = ? AND status = 'ready'",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = &old {
            if existing.is_dir {
                return Err(AppError::IsDirectory);
            }
            if !overwrite {
                return Err(AppError::Conflict(
                    "同名条目已存在，本次秒传禁止覆盖".into(),
                ));
            }
            if existing.size == size && existing.sha256.as_deref() == Some(sha256) {
                let id = existing.id;
                tx.commit().await?;
                return Ok(Some((id, Vec::new())));
            }
        }
        // 源只需 chunks 有效:ready 即可(回收站子树整树置 trashed,ready ⇒ 可见)
        let src: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM nodes
             WHERE sha256 = ? AND size = ? AND is_dir = 0 AND status = 'ready' LIMIT 1",
        )
        .bind(sha256)
        .bind(size)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((src_id,)) = src else {
            return Ok(None);
        };
        let ts = now_ts();
        let mut candidates = Vec::new();
        let inherited_favorite = old.as_ref().is_some_and(|node| node.favorite);
        if let Some(old) = old {
            let old_id = old.id;
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT storage_target_id, tg_message_id FROM chunks
                 WHERE node_id = ? AND storage_target_id IS NOT NULL",
            )
            .bind(old_id)
            .fetch_all(&mut *tx)
            .await?;
            candidates = rows
                .into_iter()
                .map(|(storage_target_id, message_id)| StoredMessage {
                    storage_target_id,
                    message_id,
                })
                .collect();
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
        let new_id = sqlx::query(
            "INSERT INTO nodes
                (parent_id, name, is_dir, size, status, created_at, modified_at, sha256, favorite)
             VALUES (?, ?, 0, ?, 'ready', ?, ?, ?, ?)",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .bind(size)
        .bind(ts)
        .bind(ts)
        .bind(sha256)
        .bind(inherited_favorite)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             SELECT ?, idx, size, tg_file_id, tg_message_id, storage_target_id,
                    tg_thumbnail_file_id
             FROM chunks WHERE node_id = ?",
        )
        .bind(new_id)
        .bind(src_id)
        .execute(&mut *tx)
        .await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(Some((new_id, freed)))
    }
}
