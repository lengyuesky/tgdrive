//! 回收站：软删除、恢复与过期清理。

use super::*;

impl Db {
    /// 软删除:整棵子树置 trashed(保持 ready ⇒ 可见的不变式),该子树的分享一并删除
    pub async fn trash_node(&self, id: i64, orig_path: &str) -> AppResult<()> {
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能删除根目录".into()));
        }
        let node = self.node(id).await?;
        let parent_id = node.parent_id.ok_or(AppError::NotFound)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE nodes SET status = 'trashed' WHERE id IN (SELECT id FROM sub) AND status = 'ready'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             DELETE FROM shares WHERE node_id IN (SELECT id FROM sub)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO trash (node_id, orig_parent_id, orig_path, trashed_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(parent_id)
        .bind(orig_path)
        .bind(now_ts())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 回收站列表:(子树根节点, 原路径, 删除时间)
    pub async fn trash_list(&self) -> AppResult<Vec<(Node, String, i64)>> {
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT node_id, orig_path, trashed_at FROM trash ORDER BY trashed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let nodes: Vec<Node> =
            sqlx::query_as("SELECT * FROM nodes WHERE id IN (SELECT node_id FROM trash)")
                .fetch_all(&self.pool)
                .await?;
        let mut by_id: std::collections::HashMap<i64, Node> =
            nodes.into_iter().map(|n| (n.id, n)).collect();
        rows.into_iter()
            .map(|(nid, path, at)| Ok((by_id.remove(&nid).ok_or(AppError::NotFound)?, path, at)))
            .collect()
    }

    /// 恢复:优先回原目录(仍可见时),否则回根目录;同名冲突自动改名。返回恢复后的完整路径
    pub async fn trash_restore(&self, node_id: i64) -> AppResult<String> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT orig_parent_id FROM trash WHERE node_id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await?;
        let (orig_parent,) = row.ok_or(AppError::NotFound)?;
        let node = self.node(node_id).await?;

        let parent_visible = matches!(
            self.node(orig_parent).await,
            Ok(p) if p.is_dir && (p.id == ROOT_ID || self.full_path_if_visible(p.id).await?.is_some())
        );
        let target_parent = if parent_visible { orig_parent } else { ROOT_ID };

        // 同名冲突 → "name (恢复)"、"name (恢复 2)" ...
        let mut name = node.name.clone();
        let mut k = 0;
        while self.child(target_parent, &name).await?.is_some() {
            k += 1;
            name = if k == 1 {
                format!("{} (恢复)", node.name)
            } else {
                format!("{} (恢复 {k})", node.name)
            };
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 先改名/挪位置:此时根仍是 trashed,不受 ready 部分唯一索引约束;
        // 随后整树置 ready 时根已用新名字入索引,不会与占名者冲突
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, modified_at = ? WHERE id = ?")
            .bind(target_parent)
            .bind(&name)
            .bind(now_ts())
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE nodes SET status = 'ready' WHERE id IN (SELECT id FROM sub) AND status = 'trashed'",
        )
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM trash WHERE node_id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(self
            .full_path_if_visible(node_id)
            .await?
            .unwrap_or_else(|| format!("/{name}")))
    }

    /// 彻底删除回收站中超期的项,返回引用归零的 message_id
    pub async fn trash_purge_expired(&self, retention_days: i64) -> AppResult<Vec<StoredMessage>> {
        let cutoff = now_ts() - retention_days * 86400;
        let ids: Vec<(i64,)> = sqlx::query_as("SELECT node_id FROM trash WHERE trashed_at <= ?")
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            freed.extend(self.delete_node(id).await?);
        }
        Ok(freed)
    }
}
