//! 删除与元数据级复制。

use super::*;

impl Db {
    /// 递归删除节点;返回引用归零、需要在 TG 删除的 message_id
    pub async fn delete_node(&self, id: i64) -> AppResult<Vec<StoredMessage>> {
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能删除根目录".into()));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 收集整棵子树的节点 id(SQLite 递归 CTE)
        let node_ids: Vec<(i64,)> = sqlx::query_as(
            "WITH RECURSIVE sub(id) AS (
               SELECT id FROM nodes WHERE id = ?
               UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             ) SELECT id FROM sub",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;

        let mut candidates: Vec<StoredMessage> = Vec::new();
        for (nid,) in &node_ids {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT storage_target_id, tg_message_id FROM chunks
                 WHERE node_id = ? AND storage_target_id IS NOT NULL",
            )
            .bind(nid)
            .fetch_all(&mut *tx)
            .await?;
            candidates.extend(rows.into_iter().map(|(storage_target_id, message_id)| {
                StoredMessage {
                    storage_target_id,
                    message_id,
                }
            }));
        }
        // 子节点行由 ON DELETE CASCADE 级联;显式删整棵树避免依赖外键深度
        for (nid,) in node_ids.iter().rev() {
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(nid)
                .execute(&mut *tx)
                .await?;
        }
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(freed)
    }

    /// 复制文件:仅复制元数据行,共享 Telegram 消息(零流量)
    pub async fn copy_file(&self, src_id: i64, dst_parent: i64, dst_name: &str) -> AppResult<i64> {
        let src = self.node(src_id).await?;
        if src.is_dir {
            return Err(AppError::IsDirectory);
        }
        if self.child(dst_parent, dst_name).await?.is_some() {
            return Err(AppError::AlreadyExists);
        }
        let ts = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let new_id = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at, sha256)
             VALUES (?, ?, 0, ?, 'ready', ?, ?, ?)",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .bind(src.size)
        .bind(ts)
        .bind(ts)
        .bind(&src.sha256)
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
        sqlx::query(
            "INSERT INTO inbox_sources (node_id, source)
             SELECT ?, source FROM inbox_sources WHERE node_id = ?",
        )
        .bind(new_id)
        .bind(src_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(new_id)
    }

    /// 在一个事务内复制整棵目录（元数据级）；中途失败整体回滚，不会留下半棵树。
    pub async fn copy_tree(&self, src_id: i64, dst_parent: i64, dst_name: &str) -> AppResult<()> {
        let ts = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let parent_is_dir: Option<(bool,)> =
            sqlx::query_as("SELECT is_dir FROM nodes WHERE id = ? AND status = 'ready'")
                .bind(dst_parent)
                .fetch_optional(&mut *tx)
                .await?;
        match parent_is_dir {
            None => return Err(AppError::NotFound),
            Some((false,)) => return Err(AppError::NotDirectory),
            Some((true,)) => {}
        }
        // (源目录, 新父目录, 新名字)
        let mut pending = vec![(src_id, dst_parent, dst_name.to_string())];
        while let Some((src_dir, parent, name)) = pending.pop() {
            let new_dir = sqlx::query(
                "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at)
                 VALUES (?, ?, 1, 0, 'ready', ?, ?)",
            )
            .bind(parent)
            .bind(&name)
            .bind(ts)
            .bind(ts)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
            let children: Vec<(i64, String, bool, i64, Option<String>)> = sqlx::query_as(
                "SELECT id, name, is_dir, size, sha256 FROM nodes
                 WHERE parent_id = ? AND status = 'ready'",
            )
            .bind(src_dir)
            .fetch_all(&mut *tx)
            .await?;
            for (child_id, child_name, is_dir, size, sha256) in children {
                if is_dir {
                    pending.push((child_id, new_dir, child_name));
                    continue;
                }
                let new_id = sqlx::query(
                    "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at, sha256)
                     VALUES (?, ?, 0, ?, 'ready', ?, ?, ?)",
                )
                .bind(new_dir)
                .bind(&child_name)
                .bind(size)
                .bind(ts)
                .bind(ts)
                .bind(&sha256)
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
                .bind(child_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO inbox_sources (node_id, source)
                     SELECT ?, source FROM inbox_sources WHERE node_id = ?",
                )
                .bind(new_id)
                .bind(child_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}
