//! 普通上传的占位节点与提交。

use super::*;

impl Db {
    pub async fn record_inbox_source(&self, node_id: i64, source: InboxSource) -> AppResult<()> {
        sqlx::query("INSERT INTO inbox_sources (node_id, source) VALUES (?, ?)")
            .bind(node_id)
            .bind(source.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 同一频道里其他上传 Bot 的消息也会推送给主 Bot，已有映射的消息不能重复收件。
    /// 同时接受配置中的频道名和 update 返回的数字 ID。
    pub async fn has_stored_message_in_chat(
        &self,
        configured_chat: &str,
        numeric_chat: i64,
        message_id: i64,
    ) -> AppResult<bool> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(
                SELECT 1 FROM chunks c JOIN storage_targets t ON t.id = c.storage_target_id
                WHERE c.tg_message_id = ? AND t.chat_id IN (?, ?)
             )",
        )
        .bind(message_id)
        .bind(configured_chat)
        .bind(numeric_chat.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// 创建 uploading 状态的文件占位节点(允许与同名 ready 文件并存,finalize 时替换)
    pub async fn create_uploading(&self, parent_id: i64, name: &str) -> AppResult<i64> {
        let parent = self.node(parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if let Some(existing) = self.child(parent_id, name).await?
            && existing.is_dir
        {
            return Err(AppError::IsDirectory); // 不允许文件覆盖目录
        }
        let ts = now_ts();
        // 条件插入在 SQLite 的单条写语句内完成，防止两个请求同时为同一路径建上传影子。
        let inserted = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at)
             SELECT ?, ?, 0, 0, 'uploading', ?, ?
             WHERE NOT EXISTS (
                 SELECT 1 FROM nodes
                 WHERE parent_id = ? AND name = ? AND status = 'uploading'
             )",
        )
        .bind(parent_id)
        .bind(name)
        .bind(ts)
        .bind(ts)
        .bind(parent_id)
        .bind(name)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(AppError::AlreadyExists);
        }
        Ok(inserted.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_chunk(
        &self,
        node_id: i64,
        idx: i64,
        size: i64,
        tg_file_id: &str,
        tg_message_id: i64,
        storage_target_id: i64,
        tg_thumbnail_file_id: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(node_id)
        .bind(idx)
        .bind(size)
        .bind(tg_file_id)
        .bind(tg_message_id)
        .bind(storage_target_id)
        .bind(tg_thumbnail_file_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 完成上传:置 ready 并原子替换同名旧文件;返回旧文件释放的 message_id(需在 TG 侧删除)
    pub async fn finalize_upload(
        &self,
        node_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> AppResult<Vec<StoredMessage>> {
        let (_, freed) = self
            .finalize_upload_with_node(node_id, size, sha256)
            .await?;
        Ok(freed)
    }

    /// 完成上传并在同一事务内返回最终节点，避免提交后再次查询造成“已成功却报失败”。
    pub async fn finalize_upload_with_node(
        &self,
        node_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> AppResult<(Node, Vec<StoredMessage>)> {
        let node = self.node(node_id).await?;
        let parent_id = node.parent_id.ok_or(AppError::NotFound)?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 旧同名 ready 节点(覆盖写场景)
        let old: Option<(i64, bool, bool)> = sqlx::query_as(
            "SELECT id, favorite, is_dir FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'ready' AND id != ?",
        )
        .bind(parent_id)
        .bind(&node.name)
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        // 上传期间可能新建或改名出同名目录；只能覆盖文件，否则会级联删掉整棵子树。
        if old.is_some_and(|(_, _, is_dir)| is_dir) {
            return Err(AppError::IsDirectory);
        }

        let mut candidates: Vec<StoredMessage> = Vec::new();
        let inherited_favorite = old.map(|(_, favorite, _)| favorite).unwrap_or(false);
        if let Some((old_id, _, _)) = old {
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
        // 上传会话只在 uploading 阶段存在；和最终节点切换在同一事务内完成。
        sqlx::query("DELETE FROM upload_sessions WHERE node_id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE nodes
             SET status = 'ready', size = ?, sha256 = ?, modified_at = ?,
                 favorite = CASE WHEN ? THEN 1 ELSE favorite END
             WHERE id = ?",
        )
        .bind(size)
        .bind(sha256)
        .bind(now_ts())
        .bind(inherited_favorite)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        let ready = sqlx::query_as::<_, Node>("SELECT * FROM nodes WHERE id = ?")
            .bind(node_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((ready, freed))
    }

    /// 放弃上传(失败/断开/启动清理):删节点行,返回已上传且未被共享的 message_id
    pub async fn abort_upload(&self, node_id: i64) -> AppResult<Vec<StoredMessage>> {
        self.abort_upload_inner(node_id, true).await
    }

    /// 收件箱登记的是用户原始频道消息；登记失败只能回滚元数据，不能删除原消息。
    pub async fn abort_upload_keep_messages(&self, node_id: i64) -> AppResult<()> {
        self.abort_upload_inner(node_id, false).await.map(|_| ())
    }

    pub(super) async fn abort_upload_inner(
        &self,
        node_id: i64,
        enqueue_delete: bool,
    ) -> AppResult<Vec<StoredMessage>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT storage_target_id, tg_message_id FROM chunks
             WHERE node_id = ? AND storage_target_id IS NOT NULL",
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|(storage_target_id, message_id)| StoredMessage {
                storage_target_id,
                message_id,
            })
            .collect();
        sqlx::query("DELETE FROM nodes WHERE id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        let freed = if enqueue_delete {
            Self::freed_messages(&mut tx, candidates).await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        Ok(freed)
    }

    /// 启动时清理旧上传管线的 uploading 残留；可恢复会话必须保留。
    pub async fn cleanup_stale_uploads(&self) -> AppResult<Vec<StoredMessage>> {
        let ids: Vec<(i64,)> = sqlx::query_as(
            "SELECT n.id FROM nodes n
             LEFT JOIN upload_sessions s ON s.node_id = n.id
             WHERE n.status = 'uploading' AND s.id IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            freed.extend(self.abort_upload(id).await?);
        }
        Ok(freed)
    }
}
