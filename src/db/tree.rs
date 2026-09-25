//! 目录创建、移动与重命名。

use super::*;

impl Db {
    pub async fn mkdir(&self, parent_id: i64, name: &str) -> AppResult<Node> {
        let parent = self.node(parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if self.child(parent_id, name).await?.is_some() {
            return Err(AppError::AlreadyExists);
        }
        let ts = now_ts();
        let id = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at)
             VALUES (?, ?, 1, 0, 'ready', ?, ?)",
        )
        .bind(parent_id)
        .bind(name)
        .bind(ts)
        .bind(ts)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        self.node(id).await
    }

    /// 检查 ancestor 是否为 node 的祖先(含相等),防止把目录移进自己子树
    pub async fn is_ancestor(&self, ancestor: i64, node: i64) -> AppResult<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "WITH RECURSIVE chain(id, depth) AS (
               SELECT ?, 0
               UNION ALL
               SELECT n.parent_id, c.depth + 1
               FROM nodes n JOIN chain c ON n.id = c.id
               WHERE n.parent_id IS NOT NULL AND c.depth < 4096
             )
             SELECT 1 FROM chain WHERE id = ? LIMIT 1",
        )
        .bind(node)
        .bind(ancestor)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// 移动/重命名(同为改 parent_id+name)
    pub async fn move_node(&self, id: i64, new_parent: i64, new_name: &str) -> AppResult<()> {
        let node = self.node(id).await?;
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能移动根目录".into()));
        }
        let parent = self.node(new_parent).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if node.is_dir && self.is_ancestor(id, new_parent).await? {
            return Err(AppError::BadRequest("不能把目录移动到自己内部".into()));
        }
        if let Some(existing) = self.child(new_parent, new_name).await?
            && existing.id != id
        {
            return Err(AppError::AlreadyExists);
        }
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, modified_at = ? WHERE id = ?")
            .bind(new_parent)
            .bind(new_name)
            .bind(now_ts())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
