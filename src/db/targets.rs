//! 存储目标（Telegram Bot/会话）登记与迁移任务。

use super::*;

impl Db {
    /// 取得或创建一个不可变的存储目标。敏感字段只留在服务端，不会直接序列化到 API。
    pub async fn ensure_storage_target(
        &self,
        api_base: &str,
        bot_token: &str,
        chat_id: &str,
    ) -> AppResult<StorageTarget> {
        sqlx::query(
            "INSERT INTO storage_targets (api_base, bot_token, chat_id, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(api_base, bot_token, chat_id) DO NOTHING",
        )
        .bind(api_base.trim_end_matches('/'))
        .bind(bot_token)
        .bind(chat_id)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        sqlx::query_as::<_, StorageTarget>(
            "SELECT * FROM storage_targets
             WHERE api_base = ? AND bot_token = ? AND chat_id = ?",
        )
        .bind(api_base.trim_end_matches('/'))
        .bind(bot_token)
        .bind(chat_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn storage_targets(&self) -> AppResult<Vec<StorageTarget>> {
        Ok(sqlx::query_as("SELECT * FROM storage_targets ORDER BY id")
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn storage_target(&self, id: i64) -> AppResult<StorageTarget> {
        sqlx::query_as::<_, StorageTarget>("SELECT * FROM storage_targets WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    /// 当前仍指向非主 Bot 的历史消息，按来源目标汇总。
    pub async fn storage_migration_sources(
        &self,
        main_target_id: i64,
    ) -> AppResult<Vec<StorageMigrationSource>> {
        Self::migration_sources(&self.pool, main_target_id).await
    }

    async fn migration_sources<'c>(
        executor: impl sqlx::SqliteExecutor<'c>,
        main_target_id: i64,
    ) -> AppResult<Vec<StorageMigrationSource>> {
        Ok(sqlx::query_as(
            "SELECT c.storage_target_id,
                    CASE WHEN instr(st.bot_token, ':') > 0
                         THEN substr(st.bot_token, 1, instr(st.bot_token, ':') - 1)
                         ELSE '未知' END AS bot_id,
                    st.chat_id,
                    COUNT(DISTINCT c.tg_message_id) AS messages,
                    COUNT(*) AS chunks,
                    COALESCE(SUM(c.size), 0) AS bytes
             FROM chunks c
             JOIN storage_targets st ON st.id = c.storage_target_id
             WHERE c.storage_target_id IS NOT NULL AND c.storage_target_id != ?
             GROUP BY c.storage_target_id, st.bot_token, st.chat_id
             ORDER BY c.storage_target_id",
        )
        .bind(main_target_id)
        .fetch_all(executor)
        .await?)
    }

    pub async fn storage_migration_candidate(
        &self,
        main_target_id: i64,
    ) -> AppResult<Option<StorageMigrationCandidate>> {
        Ok(sqlx::query_as(
            "SELECT c.storage_target_id AS source_target_id,
                    st.chat_id AS source_chat_id,
                    c.tg_message_id AS source_message_id
             FROM chunks c
             JOIN storage_targets st ON st.id = c.storage_target_id
             WHERE c.storage_target_id IS NOT NULL AND c.storage_target_id != ?
             GROUP BY c.storage_target_id, st.chat_id, c.tg_message_id
             ORDER BY c.storage_target_id, c.tg_message_id
             LIMIT 1",
        )
        .bind(main_target_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn storage_migration_job(&self) -> AppResult<Option<StorageMigrationJob>> {
        Self::migration_job(&self.pool).await
    }

    async fn migration_job<'c>(
        executor: impl sqlx::SqliteExecutor<'c>,
    ) -> AppResult<Option<StorageMigrationJob>> {
        Ok(sqlx::query_as(
            "SELECT main_target_id, status, total_messages, migrated_messages,
                    total_chunks, migrated_chunks, last_error, started_at, updated_at
             FROM storage_migration_jobs WHERE id = 1",
        )
        .fetch_optional(executor)
        .await?)
    }

    /// 剩余来源与任务进度取自同一读快照。迁移在一个事务里切换引用并推进进度，
    /// 分两次读取时若夹着一次提交，同一条消息会在已迁移和剩余里各算一次。
    pub async fn storage_migration_progress(
        &self,
        main_target_id: i64,
    ) -> AppResult<(Vec<StorageMigrationSource>, Option<StorageMigrationJob>)> {
        let mut tx = self.pool.begin().await?;
        let sources = Self::migration_sources(&mut *tx, main_target_id).await?;
        let job = Self::migration_job(&mut *tx).await?;
        tx.commit().await?;
        Ok((sources, job))
    }

    /// 开始或继续迁移；同一主目标会保留历史进度，切换主目标则重新计数。
    pub async fn storage_migration_begin(
        &self,
        main_target_id: i64,
        remaining_messages: i64,
        remaining_chunks: i64,
    ) -> AppResult<StorageMigrationJob> {
        let previous = self.storage_migration_job().await?;
        let same_target = previous
            .as_ref()
            .is_some_and(|job| job.main_target_id == main_target_id);
        let migrated_messages = previous
            .as_ref()
            .filter(|_| same_target)
            .map(|job| job.migrated_messages)
            .unwrap_or(0);
        let migrated_chunks = previous
            .as_ref()
            .filter(|_| same_target)
            .map(|job| job.migrated_chunks)
            .unwrap_or(0);
        let started_at = previous
            .as_ref()
            .filter(|_| same_target)
            .map(|job| job.started_at)
            .unwrap_or_else(now_ts);
        let ts = now_ts();
        sqlx::query(
            "INSERT INTO storage_migration_jobs
                (id, main_target_id, status, total_messages, migrated_messages,
                 total_chunks, migrated_chunks, last_error, started_at, updated_at)
             VALUES (1, ?, 'running', ?, ?, ?, ?, NULL, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                main_target_id = excluded.main_target_id,
                status = 'running',
                total_messages = excluded.total_messages,
                migrated_messages = excluded.migrated_messages,
                total_chunks = excluded.total_chunks,
                migrated_chunks = excluded.migrated_chunks,
                last_error = NULL,
                started_at = excluded.started_at,
                updated_at = excluded.updated_at",
        )
        .bind(main_target_id)
        .bind(migrated_messages.saturating_add(remaining_messages))
        .bind(migrated_messages)
        .bind(migrated_chunks.saturating_add(remaining_chunks))
        .bind(migrated_chunks)
        .bind(started_at)
        .bind(ts)
        .execute(&self.pool)
        .await?;
        self.storage_migration_job()
            .await?
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("迁移任务创建后无法读取")))
    }

    pub async fn storage_migration_set_status(
        &self,
        status: &str,
        error: Option<&str>,
    ) -> AppResult<()> {
        let detail = error.map(|value| value.chars().take(500).collect::<String>());
        sqlx::query(
            "UPDATE storage_migration_jobs
             SET status = ?, last_error = ?, updated_at = ? WHERE id = 1",
        )
        .bind(status)
        .bind(detail)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Telegram 复制成功后切换所有共享引用，并在同一事务内推进任务进度。
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_storage_message_migration(
        &self,
        source_target_id: i64,
        source_message_id: i64,
        main_target_id: i64,
        new_file_id: &str,
        new_message_id: i64,
        new_thumbnail_file_id: Option<&str>,
    ) -> AppResult<u64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE chunks
             SET storage_target_id = ?, tg_file_id = ?, tg_message_id = ?,
                 tg_thumbnail_file_id = ?
             WHERE storage_target_id = ? AND tg_message_id = ?",
        )
        .bind(main_target_id)
        .bind(new_file_id)
        .bind(new_message_id)
        .bind(new_thumbnail_file_id)
        .bind(source_target_id)
        .bind(source_message_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 {
            sqlx::query(
                "UPDATE storage_migration_jobs
                 SET migrated_messages = migrated_messages + 1,
                     migrated_chunks = migrated_chunks + ?,
                     updated_at = ?
                 WHERE id = 1 AND main_target_id = ?",
            )
            .bind(i64::try_from(updated).unwrap_or(i64::MAX))
            .bind(now_ts())
            .bind(main_target_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    pub async fn active_upload_claim_count(&self) -> AppResult<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM upload_part_claims")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// 服务重启时，上一轮运行中的任务改为暂停，管理员可继续。
    pub async fn recover_storage_migration(&self) -> AppResult<()> {
        sqlx::query(
            "UPDATE storage_migration_jobs
             SET status = 'paused', updated_at = ? WHERE id = 1 AND status = 'running'",
        )
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// v3 及更早版本的分块没有存储目标。迁移时只能采用当前生效配置，且只执行一次。
    pub async fn backfill_unassigned_chunks(&self, storage_target_id: i64) -> AppResult<u64> {
        Ok(
            sqlx::query("UPDATE chunks SET storage_target_id = ? WHERE storage_target_id IS NULL")
                .bind(storage_target_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub async fn unassigned_chunk_count(&self) -> AppResult<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM chunks WHERE storage_target_id IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}
