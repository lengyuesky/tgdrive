//! 应用上传复用网盘的分块映射与删除队列；这里没有内容解析或另一套存储后端。
use super::*;
use sqlx::SqliteConnection;

impl Db {
    pub(crate) async fn create_app_staging_in(
        connection: &mut SqliteConnection,
        parent: i64,
    ) -> AppResult<i64> {
        let ts = now_ts();
        let id = sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,0,0,'uploading',?,?)")
            .bind(parent).bind(format!("app-stage-{}", crate::auth::random_secret())).bind(ts).bind(ts).execute(connection).await?.last_insert_rowid();
        Ok(id)
    }
    pub(crate) async fn finish_app_staging(
        &self,
        node: i64,
        size: i64,
        sha: &str,
    ) -> AppResult<Node> {
        let updated = sqlx::query("UPDATE nodes SET size=?,sha256=?,modified_at=? WHERE id=? AND status='uploading' AND EXISTS(SELECT 1 FROM app_uploads WHERE staging_node_id=nodes.id AND state='receiving')")
            .bind(size).bind(sha).bind(now_ts()).bind(node).execute(&self.pool).await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict("上传已取消或过期".into()));
        }
        self.node(node).await
    }
    /// create 在最终提交时分配可见节点身份；replace 保留已有身份和创建时间。
    pub(crate) async fn commit_app_staging_in(
        connection: &mut SqliteConnection,
        staging: i64,
        parent: i64,
        name: &str,
        replace: Option<i64>,
    ) -> AppResult<i64> {
        let source = Self::node_in(connection, staging).await?;
        if source.status != "uploading" {
            return Err(AppError::Conflict("上传暂存状态已改变".into()));
        }
        let mut old = Vec::new();
        let node = if let Some(node) = replace {
            let chunks: Vec<(i64, i64)> = sqlx::query_as("SELECT storage_target_id,tg_message_id FROM chunks WHERE node_id=? AND storage_target_id IS NOT NULL").bind(node).fetch_all(&mut *connection).await?;
            old.extend(
                chunks
                    .into_iter()
                    .map(|(storage_target_id, message_id)| StoredMessage {
                        storage_target_id,
                        message_id,
                    }),
            );
            sqlx::query("DELETE FROM chunks WHERE node_id=?")
                .bind(node)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE nodes SET size=?,sha256=?,modified_at=? WHERE id=? AND status='ready'",
            )
            .bind(source.size)
            .bind(source.sha256.as_deref())
            .bind(now_ts())
            .bind(node)
            .execute(&mut *connection)
            .await?;
            // 即使相同大小和摘要，每次覆盖仍是新的内容版本，不能发生 ABA。
            sqlx::query("UPDATE app_node_meta SET content_version=?,revision=? WHERE node_id=?")
                .bind(crate::auth::random_secret())
                .bind(crate::auth::random_secret())
                .bind(node)
                .execute(&mut *connection)
                .await?;
            node
        } else {
            let ts = now_ts();
            sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at,sha256) VALUES(?,?,0,?,'ready',?,?,?)")
                .bind(parent).bind(name).bind(source.size).bind(ts).bind(ts).bind(source.sha256.as_deref()).execute(&mut *connection).await?.last_insert_rowid()
        };
        sqlx::query("INSERT INTO chunks(node_id,idx,size,tg_file_id,tg_message_id,storage_target_id,tg_thumbnail_file_id) SELECT ?,idx,size,tg_file_id,tg_message_id,storage_target_id,tg_thumbnail_file_id FROM chunks WHERE node_id=?")
            .bind(node).bind(staging).execute(&mut *connection).await?;
        sqlx::query("DELETE FROM nodes WHERE id=? AND status='uploading'")
            .bind(staging)
            .execute(&mut *connection)
            .await?;
        Self::freed_messages(connection, old).await?;
        Ok(node)
    }
}
