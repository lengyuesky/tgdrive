//! 可恢复分块上传会话。

use super::*;

impl Db {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub async fn create_upload_session(
        &self,
        parent_id: i64,
        name: &str,
        display_path: &str,
        expected_size: i64,
        sha256: &str,
        chunk_size: i64,
        storage_target_id: i64,
    ) -> AppResult<UploadSessionInfo> {
        self.create_upload_session_with_policy(
            parent_id,
            name,
            display_path,
            expected_size,
            sha256,
            chunk_size,
            storage_target_id,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_upload_session_with_policy(
        &self,
        parent_id: i64,
        name: &str,
        display_path: &str,
        expected_size: i64,
        sha256: &str,
        chunk_size: i64,
        storage_target_id: i64,
        overwrite: bool,
    ) -> AppResult<UploadSessionInfo> {
        if expected_size < 0 || chunk_size <= 0 {
            return Err(AppError::BadRequest("上传大小或分块大小无效".into()));
        }
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::BadRequest("SHA-256 格式无效".into()));
        }

        let existing: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT s.id, s.expected_size, s.sha256
             FROM upload_sessions s JOIN nodes n ON n.id = s.node_id
             WHERE n.parent_id = ? AND n.name = ? AND n.status = 'uploading'",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((id, size, existing_sha)) = existing {
            if size == expected_size && existing_sha.eq_ignore_ascii_case(sha256) {
                let info = self.upload_session_info(&id).await?;
                if info.session.overwrite != overwrite {
                    return Err(AppError::Conflict(
                        "已有会话的覆盖策略不同，禁止静默修改".into(),
                    ));
                }
                return Ok(info);
            }
            return Err(AppError::BadRequest(
                "同一路径已有另一个未完成上传，请先在上传队列中取消它".into(),
            ));
        }

        if !overwrite && self.child(parent_id, name).await?.is_some() {
            return Err(AppError::Conflict(
                "同名条目已存在，本次上传禁止覆盖".into(),
            ));
        }
        let node_id = self.create_uploading(parent_id, name).await?;
        let id = random_token(32);
        let ts = now_ts();
        let inserted = sqlx::query(
            "INSERT INTO upload_sessions
                (id, node_id, display_path, expected_size, sha256, chunk_size,
                 storage_target_id, created_at, updated_at, overwrite)
             VALUES (?, ?, ?, ?, lower(?), ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(node_id)
        .bind(display_path)
        .bind(expected_size)
        .bind(sha256)
        .bind(chunk_size)
        .bind(storage_target_id)
        .bind(ts)
        .bind(ts)
        .bind(overwrite)
        .execute(&self.pool)
        .await;
        if let Err(error) = inserted {
            let _ = self.abort_upload(node_id).await;
            return Err(error.into());
        }
        self.upload_session_info(&id).await
    }

    pub(super) async fn upload_session_row(&self, id: &str) -> AppResult<UploadSession> {
        sqlx::query_as::<_, UploadSession>("SELECT * FROM upload_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub async fn upload_session_info(&self, id: &str) -> AppResult<UploadSessionInfo> {
        let mut tx = self.pool.begin().await?;
        let session: UploadSession = sqlx::query_as("SELECT * FROM upload_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let node: Node = sqlx::query_as("SELECT * FROM nodes WHERE id = ?")
            .bind(session.node_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT idx, size FROM chunks WHERE node_id = ? ORDER BY idx")
                .bind(session.node_id)
                .fetch_all(&mut *tx)
                .await?;
        let in_flight_parts = sqlx::query_scalar(
            "SELECT idx FROM upload_part_claims WHERE session_id = ? ORDER BY idx",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        let uncertain_parts = sqlx::query_scalar(
            "SELECT idx FROM upload_part_uncertainties WHERE session_id = ? ORDER BY idx",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(UploadSessionInfo {
            session,
            node,
            uploaded_parts: rows.iter().map(|(idx, _)| *idx).collect(),
            uploaded_bytes: rows.iter().map(|(_, size)| *size).sum(),
            in_flight_parts,
            uncertain_parts,
        })
    }

    pub async fn upload_sessions(&self) -> AppResult<Vec<UploadSessionInfo>> {
        let ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM upload_sessions ORDER BY updated_at, id")
                .fetch_all(&self.pool)
                .await?;
        let mut out = Vec::with_capacity(ids.len());
        for (id,) in ids {
            out.push(self.upload_session_info(&id).await?);
        }
        Ok(out)
    }

    pub async fn set_upload_thumbnail(&self, id: &str, thumbnail: &[u8]) -> AppResult<()> {
        let changed = sqlx::query(
            "UPDATE upload_sessions
             SET thumbnail = ?, updated_at = ?
             WHERE id = ? AND NOT EXISTS (
                 SELECT 1 FROM chunks c
                 WHERE c.node_id = upload_sessions.node_id AND c.idx = 0
             )",
        )
        .bind(thumbnail)
        .bind(now_ts())
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            // 已经完成第 0 块时上传缩略图是幂等无害的；不存在的会话才报错。
            self.upload_session_row(id).await?;
        }
        Ok(())
    }

    pub async fn claim_upload_part(&self, id: &str, idx: i64) -> AppResult<UploadPartClaim> {
        let info = self.upload_session_info(id).await?;
        if info.expected_part_size(idx).is_none() {
            return Err(AppError::BadRequest("分块下标超出范围".into()));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current_node: Option<i64> =
            sqlx::query_scalar("SELECT node_id FROM upload_sessions WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if current_node != Some(info.session.node_id) {
            return Err(AppError::NotFound);
        }
        // 状态只以写事务中的查询为准，旧快照的不确定标记可能刚被成功落库清除。
        let completed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE node_id = ? AND idx = ?")
                .bind(info.session.node_id)
                .bind(idx)
                .fetch_one(&mut *tx)
                .await?;
        if completed > 0 {
            return Ok(UploadPartClaim::Complete);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upload_part_claims WHERE session_id = ? AND idx = ?",
        )
        .bind(id)
        .bind(idx)
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            return Err(AppError::Conflict("该分块正在上传，请稍后核实状态".into()));
        }
        let uncertain: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upload_part_uncertainties WHERE session_id = ? AND idx = ?",
        )
        .bind(id)
        .bind(idx)
        .fetch_one(&mut *tx)
        .await?;
        if uncertain > 0 {
            return Err(AppError::TelegramUncertain(
                "存储结果不确定，禁止重复发送".into(),
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO upload_part_claims(session_id, idx, started_at)
             VALUES (?, ?, ?) ON CONFLICT(session_id, idx) DO NOTHING",
        )
        .bind(id)
        .bind(idx)
        .bind(now_ts())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(AppError::BadRequest("该分块正在上传，请稍后重试".into()));
        }
        // 与分块声明同一事务提交，崩溃窗口中不能出现“已发送但无不确定标记”。
        sqlx::query("INSERT INTO upload_part_uncertainties(session_id,idx,created_at) VALUES(?,?,?) ON CONFLICT(session_id,idx) DO NOTHING")
            .bind(id).bind(idx).bind(now_ts()).execute(&mut *tx).await?;
        sqlx::query("UPDATE upload_sessions SET updated_at = ? WHERE id = ?")
            .bind(now_ts())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(UploadPartClaim::Claimed(Box::new(info)))
    }

    pub async fn release_upload_part(&self, id: &str, idx: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_part_claims WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn finish_upload_part(
        &self,
        id: &str,
        idx: i64,
        size: i64,
        tg_file_id: &str,
        tg_message_id: i64,
        storage_target_id: i64,
        tg_thumbnail_file_id: Option<&str>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let session: UploadSession = sqlx::query_as("SELECT * FROM upload_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let part_count = if session.expected_size == 0 {
            0
        } else {
            session.expected_size / session.chunk_size
                + i64::from(session.expected_size % session.chunk_size != 0)
        };
        if idx < 0 || idx >= part_count {
            return Err(AppError::BadRequest("分块下标超出范围".into()));
        }
        let expected = session
            .chunk_size
            .min(session.expected_size - idx * session.chunk_size);
        if size != expected || storage_target_id != session.storage_target_id {
            return Err(AppError::BadRequest("分块大小或存储目标不匹配".into()));
        }
        let claimed: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM upload_part_claims WHERE session_id = ? AND idx = ?")
                .bind(id)
                .bind(idx)
                .fetch_optional(&mut *tx)
                .await?;
        if claimed.is_none() {
            return Err(AppError::BadRequest("分块上传声明已失效".into()));
        }
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session.node_id)
        .bind(idx)
        .bind(size)
        .bind(tg_file_id)
        .bind(tg_message_id)
        .bind(storage_target_id)
        .bind(tg_thumbnail_file_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM upload_part_uncertainties WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM upload_part_claims WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE upload_sessions
             SET updated_at = ?, thumbnail = CASE WHEN ? = 0 THEN NULL ELSE thumbnail END
             WHERE id = ?",
        )
        .bind(now_ts())
        .bind(idx)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn complete_upload_session(&self, id: &str) -> AppResult<Node> {
        if let Some(receipt) = self.upload_receipt(id).await? {
            return Self::receipt_node(&receipt);
        }
        let info = match self.upload_session_info(id).await {
            Ok(info) => info,
            Err(AppError::NotFound) => {
                return self
                    .upload_receipt(id)
                    .await?
                    .as_ref()
                    .ok_or(AppError::NotFound)
                    .and_then(Self::receipt_node);
            }
            Err(error) => return Err(error),
        };
        let expected: Vec<i64> = (0..info.part_count()).collect();
        if info.uploaded_parts != expected || info.uploaded_bytes != info.session.expected_size {
            return Err(AppError::BadRequest("仍有分块尚未上传完成".into()));
        }
        let (claims,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM upload_part_claims WHERE session_id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        if claims != 0 {
            return Err(AppError::BadRequest("仍有分块正在上传".into()));
        }
        self.finalize_upload_with_node(
            info.session.node_id,
            info.session.expected_size,
            Some(&info.session.sha256),
        )
        .await
        .map(|(node, _)| node)
    }

    pub async fn cancel_upload_session(&self, id: &str) -> AppResult<Vec<StoredMessage>> {
        if self.upload_receipt(id).await?.is_some() {
            return Ok(Vec::new());
        }
        let session = match self.upload_session_row(id).await {
            Ok(session) => session,
            Err(AppError::NotFound) if self.upload_receipt(id).await?.is_some() => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        self.abort_upload(session.node_id).await
    }

    pub async fn find_upload_session(
        &self,
        path: &str,
        size: i64,
        sha256: &str,
    ) -> AppResult<Option<String>> {
        Ok(sqlx::query_scalar("SELECT id FROM upload_sessions WHERE display_path = ? AND expected_size = ? AND sha256 = ? ORDER BY created_at DESC LIMIT 1")
            .bind(path).bind(size).bind(sha256).fetch_optional(&self.pool).await?)
    }

    pub async fn upload_receipt(&self, id: &str) -> AppResult<Option<UploadReceipt>> {
        Ok(
            sqlx::query_as("SELECT * FROM upload_receipts WHERE id = ? AND expires_at > ?")
                .bind(id)
                .bind(now_ts())
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    fn receipt_node(receipt: &UploadReceipt) -> AppResult<Node> {
        if receipt.status != "completed" {
            return Err(AppError::Conflict("上传会话已取消".into()));
        }
        serde_json::from_str(
            receipt
                .node_json
                .as_deref()
                .ok_or_else(|| AppError::Internal(anyhow::anyhow!("完成回执缺少节点")))?,
        )
        .map_err(|error| AppError::Internal(error.into()))
    }
    pub async fn cleanup_upload_receipts(&self) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_receipts WHERE expires_at <= ?")
            .bind(now_ts())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    #[cfg(test)]
    pub async fn mark_upload_part_uncertain(&self, id: &str, idx: i64) -> AppResult<()> {
        sqlx::query("INSERT INTO upload_part_uncertainties(session_id,idx,created_at) SELECT ?,?,? WHERE EXISTS(SELECT 1 FROM upload_sessions WHERE id = ?) ON CONFLICT(session_id,idx) DO NOTHING")
            .bind(id).bind(idx).bind(now_ts()).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn clear_upload_part_uncertainty(&self, id: &str, idx: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_part_uncertainties WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn reset_upload_part_claims(&self) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_part_claims")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_expired_upload_sessions(
        &self,
        cutoff: i64,
    ) -> AppResult<Vec<StoredMessage>> {
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM upload_sessions WHERE updated_at < ? ORDER BY updated_at",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            match self.cancel_upload_session(&id).await {
                Ok(messages) => freed.extend(messages),
                Err(AppError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(freed)
    }
}
