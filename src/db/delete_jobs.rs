//! Telegram 消息的持久化删除队列。

use super::*;

impl Db {
    pub async fn enqueue_delete_message(&self, message: StoredMessage) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO delete_jobs
                (storage_target_id, tg_message_id, attempts, next_retry_at, created_at)
             VALUES (?, ?, 0, ?, ?)
             ON CONFLICT(storage_target_id, tg_message_id) DO NOTHING",
        )
        .bind(message.storage_target_id)
        .bind(message.message_id)
        .bind(now_ts() + DELETE_GRACE_SECS)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 取出到期的删除任务。`ignore_backoff` 为真时无视指数退避取出全部任务，
    /// 供后台「立即执行」使用——否则重试多次的任务退避已拉到一小时，手动点了会像没反应。
    pub async fn delete_jobs_due(
        &self,
        limit: i64,
        ignore_backoff: bool,
    ) -> AppResult<Vec<DeleteJob>> {
        // 极端情况下同一消息在入队后又被引用，安全起见取消对应删除任务。
        sqlx::query(
            "DELETE FROM delete_jobs
             WHERE EXISTS (
                 SELECT 1 FROM chunks
                 WHERE chunks.storage_target_id = delete_jobs.storage_target_id
                   AND chunks.tg_message_id = delete_jobs.tg_message_id
             )",
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_as(
            "SELECT id, storage_target_id, tg_message_id, attempts, created_at
             FROM delete_jobs
             WHERE next_retry_at <= ?
             ORDER BY next_retry_at, id
             LIMIT ?",
        )
        .bind(if ignore_backoff { i64::MAX } else { now_ts() })
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn delete_job_done(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM delete_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_job_failed(&self, id: i64, attempts: i64, error: &str) -> AppResult<()> {
        let exponent = attempts.clamp(0, 10) as u32;
        let delay = 5_i64
            .saturating_mul(2_i64.saturating_pow(exponent))
            .min(3600);
        let detail: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE delete_jobs
             SET attempts = ?, next_retry_at = ?, last_error = ?
             WHERE id = ?",
        )
        .bind(attempts + 1)
        .bind(now_ts() + delay)
        .bind(detail)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_delete_count(&self) -> AppResult<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM delete_jobs")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// 丢弃全部待清理消息。调用方需明白：对应 Telegram 消息会永久留在频道里。
    pub async fn clear_delete_jobs(&self) -> AppResult<u64> {
        Ok(sqlx::query("DELETE FROM delete_jobs")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// 删除队列概况，供后台任务分区展示“卡在哪”。
    pub async fn delete_queue_overview(&self) -> AppResult<QueueOverview> {
        let row: (i64, i64, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT COUNT(*),
                    COALESCE(SUM(attempts > 0), 0),
                    MAX(attempts),
                    (SELECT last_error FROM delete_jobs
                     WHERE last_error IS NOT NULL
                     ORDER BY attempts DESC, id LIMIT 1)
             FROM delete_jobs",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(QueueOverview {
            pending: row.0,
            retrying: row.1,
            max_attempts: row.2.unwrap_or(0),
            last_error: row.3,
        })
    }

    /// 候选消息中，删除元数据后已无任何分块引用的消息；在同一事务内先写入删除队列。
    pub(super) async fn freed_messages(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        candidates: Vec<StoredMessage>,
    ) -> AppResult<Vec<StoredMessage>> {
        let uniq: HashSet<StoredMessage> = candidates.into_iter().collect();
        let mut freed = Vec::new();
        for message in uniq {
            let still: Option<(i64,)> = sqlx::query_as(
                "SELECT 1 FROM chunks
                 WHERE storage_target_id = ? AND tg_message_id = ? LIMIT 1",
            )
            .bind(message.storage_target_id)
            .bind(message.message_id)
            .fetch_optional(&mut **tx)
            .await?;
            if still.is_none() {
                sqlx::query(
                    "INSERT INTO delete_jobs
                        (storage_target_id, tg_message_id, attempts, next_retry_at, created_at)
                     VALUES (?, ?, 0, ?, ?)
                     ON CONFLICT(storage_target_id, tg_message_id) DO NOTHING",
                )
                .bind(message.storage_target_id)
                .bind(message.message_id)
                .bind(now_ts() + DELETE_GRACE_SECS)
                .bind(now_ts())
                .execute(&mut **tx)
                .await?;
                freed.push(message);
            }
        }
        Ok(freed)
    }
}
