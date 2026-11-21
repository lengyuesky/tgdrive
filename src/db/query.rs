//! 节点与分块的基础查询。

use super::*;

impl Db {
    pub async fn node(&self, id: i64) -> AppResult<Node> {
        sqlx::query_as::<_, Node>("SELECT * FROM nodes WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    /// 按名字在目录下查可见子节点
    pub async fn child(&self, parent_id: i64, name: &str) -> AppResult<Option<Node>> {
        Ok(sqlx::query_as::<_, Node>(
            "SELECT * FROM nodes WHERE parent_id = ? AND name = ? AND status = 'ready'",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 同名文件是否仍在后台上传，用于客户端在响应丢失后等待最终结果。
    pub async fn uploading_child_exists(&self, parent_id: i64, name: &str) -> AppResult<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'uploading' LIMIT 1",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// 从根按路径段解析;段必须已通过 vfs 规范化
    ///
    /// 一条递归查询逐层匹配路径段，深路径也只有一次往返。
    pub async fn resolve(&self, segments: &[&str]) -> AppResult<Option<Node>> {
        if segments.is_empty() {
            return Ok(Some(self.node(ROOT_ID).await?));
        }
        let segments_json = serde_json::to_string(segments)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("路径编码失败: {e}")))?;
        Ok(sqlx::query_as::<_, Node>(
            "WITH RECURSIVE walk(id, depth) AS (
               SELECT ?, 0
               UNION ALL
               SELECT n.id, w.depth + 1
               FROM walk w
               JOIN json_each(?) seg ON seg.key = w.depth
               JOIN nodes n ON n.parent_id = w.id AND n.name = seg.value AND n.status = 'ready'
             )
             SELECT * FROM nodes WHERE id = (SELECT id FROM walk WHERE depth = ?)",
        )
        .bind(ROOT_ID)
        .bind(segments_json)
        .bind(segments.len() as i64)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_children(&self, parent_id: i64) -> AppResult<Vec<Node>> {
        Ok(sqlx::query_as::<_, Node>(
            "SELECT * FROM nodes WHERE parent_id = ? AND status = 'ready'
             ORDER BY is_dir DESC, name COLLATE NOCASE",
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn chunks_of(&self, node_id: i64) -> AppResult<Vec<Chunk>> {
        Ok(
            sqlx::query_as::<_, Chunk>("SELECT * FROM chunks WHERE node_id = ? ORDER BY idx")
                .bind(node_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn set_favorite(&self, node_id: i64, favorite: bool) -> AppResult<Node> {
        let changed =
            sqlx::query("UPDATE nodes SET favorite = ? WHERE id = ? AND status = 'ready'")
                .bind(favorite)
                .bind(node_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        if changed == 0 {
            return Err(AppError::NotFound);
        }
        self.node(node_id).await
    }

    /// 全局统计:文件数、目录数、逻辑总大小
    pub async fn stats(&self) -> AppResult<(i64, i64, i64)> {
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT
               COUNT(*) FILTER (WHERE is_dir = 0),
               COUNT(*) FILTER (WHERE is_dir = 1) - 1,
               COALESCE(SUM(size) FILTER (WHERE is_dir = 0), 0)
             FROM nodes WHERE status = 'ready'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// 统计一个可见节点。目录递归计算，目录本身不计入 dir_count。
    pub async fn node_usage(&self, node_id: i64) -> AppResult<NodeUsage> {
        sqlx::query_as::<_, NodeUsage>(
            "WITH RECURSIVE sub(id, is_dir, size) AS (
                 SELECT id, is_dir, size FROM nodes
                 WHERE id = ? AND status = 'ready'
                 UNION ALL
                 SELECT n.id, n.is_dir, n.size
                 FROM nodes n JOIN sub s ON n.parent_id = s.id
                 WHERE n.status = 'ready'
             )
             SELECT
                 COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0) AS file_count,
                 COALESCE(SUM(CASE WHEN is_dir = 1 AND id != ? THEN 1 ELSE 0 END), 0)
                     AS dir_count,
                 COALESCE(SUM(CASE WHEN is_dir = 0 THEN size ELSE 0 END), 0) AS total_size,
                 (SELECT COUNT(*) FROM chunks WHERE node_id IN (SELECT id FROM sub))
                     AS chunk_count
             FROM sub",
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }
}
