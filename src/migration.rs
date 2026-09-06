//! 历史 Telegram 分块迁移：由主 Bot 在 Telegram 服务端复制消息，
//! 再原子切换 SQLite 中的 file_id/message_id/storage_target_id 映射。

use crate::db::{Db, StorageMigrationSource};
use crate::error::{AppError, AppResult};
use crate::telegram::TgRegistry;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const COPY_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize)]
pub struct StorageMigrationStatus {
    pub status: String,
    pub running: bool,
    pub main_target_id: i64,
    pub main_bot_id: String,
    pub total_messages: i64,
    pub migrated_messages: i64,
    pub remaining_messages: i64,
    pub total_chunks: i64,
    pub migrated_chunks: i64,
    pub remaining_chunks: i64,
    pub last_error: Option<String>,
    pub started_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub sources: Vec<StorageMigrationSource>,
}

#[derive(Clone)]
pub struct StorageMigrationManager {
    db: Db,
    tg: Arc<TgRegistry>,
    running: Arc<AtomicBool>,
    cancel: Arc<Mutex<Option<CancellationToken>>>,
}

impl StorageMigrationManager {
    pub fn new(db: Db, tg: Arc<TgRegistry>) -> Self {
        Self {
            db,
            tg,
            running: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn status(&self, main_target_id: i64) -> AppResult<StorageMigrationStatus> {
        let target = self.db.storage_target(main_target_id).await?;
        let main_bot_id = target
            .bot_token
            .split_once(':')
            .map(|(id, _)| id)
            .unwrap_or("未知")
            .to_string();
        let sources = self.db.storage_migration_sources(main_target_id).await?;
        let remaining_messages = sources.iter().map(|source| source.messages).sum::<i64>();
        let remaining_chunks = sources.iter().map(|source| source.chunks).sum::<i64>();
        let job = self.db.storage_migration_job().await?;
        let matching_job = job.filter(|job| job.main_target_id == main_target_id);
        let migrated_messages = matching_job
            .as_ref()
            .map(|job| job.migrated_messages)
            .unwrap_or(0);
        let migrated_chunks = matching_job
            .as_ref()
            .map(|job| job.migrated_chunks)
            .unwrap_or(0);
        let is_running = self.running.load(Ordering::Acquire);
        let status = if is_running {
            "running".to_string()
        } else if remaining_messages == 0 {
            "completed".to_string()
        } else {
            matching_job
                .as_ref()
                .map(|job| job.status.clone())
                .unwrap_or_else(|| "idle".to_string())
        };
        Ok(StorageMigrationStatus {
            status,
            running: is_running,
            main_target_id,
            main_bot_id,
            total_messages: migrated_messages.saturating_add(remaining_messages),
            migrated_messages,
            remaining_messages,
            total_chunks: migrated_chunks.saturating_add(remaining_chunks),
            migrated_chunks,
            remaining_chunks,
            last_error: matching_job.as_ref().and_then(|job| job.last_error.clone()),
            started_at: matching_job.as_ref().map(|job| job.started_at),
            updated_at: matching_job.as_ref().map(|job| job.updated_at),
            sources,
        })
    }

    pub async fn start(&self, main_target_id: i64) -> AppResult<StorageMigrationStatus> {
        // 在启动后台任务前验证主目标客户端仍已注册。
        self.tg.binding(main_target_id)?;
        let sources = self.db.storage_migration_sources(main_target_id).await?;
        let remaining_messages = sources.iter().map(|source| source.messages).sum::<i64>();
        let remaining_chunks = sources.iter().map(|source| source.chunks).sum::<i64>();
        if remaining_messages == 0 {
            self.db
                .storage_migration_begin(main_target_id, 0, 0)
                .await?;
            self.db
                .storage_migration_set_status("completed", None)
                .await?;
            return self.status(main_target_id).await;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.status(main_target_id).await;
        }
        if let Err(error) = self
            .db
            .storage_migration_begin(main_target_id, remaining_messages, remaining_chunks)
            .await
        {
            self.running.store(false, Ordering::Release);
            return Err(error);
        }
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap() = Some(token.clone());
        let this = self.clone();
        tokio::spawn(async move {
            this.run(main_target_id, token).await;
        });
        self.db
            .append_backend_log(
                "info",
                "存储迁移",
                &format!(
                    "开始把 {remaining_messages} 条 Telegram 消息（{remaining_chunks} 个分块引用）迁移到主 Bot"
                ),
            )
            .await
            .ok();
        self.status(main_target_id).await
    }

    pub async fn pause(&self, main_target_id: i64) -> AppResult<StorageMigrationStatus> {
        if let Some(token) = self.cancel.lock().unwrap().as_ref() {
            token.cancel();
        }
        self.db.storage_migration_set_status("paused", None).await?;
        self.status(main_target_id).await
    }

    /// 服务退出前停止领取新消息，并给当前 Telegram 请求留出落库时间。
    pub async fn shutdown(&self) {
        if let Some(token) = self.cancel.lock().unwrap().as_ref() {
            token.cancel();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.running.load(Ordering::Acquire) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.running.load(Ordering::Acquire) {
            self.db
                .storage_migration_set_status(
                    "paused",
                    Some("服务退出时等待当前复制请求超时，请检查频道是否有未登记的复制消息"),
                )
                .await
                .ok();
        }
    }

    async fn run(&self, main_target_id: i64, token: CancellationToken) {
        let result = self.run_inner(main_target_id, &token).await;
        if let Err(error) = result {
            let detail = error.to_string();
            tracing::error!("历史存储迁移失败: {detail}");
            self.db
                .storage_migration_set_status("failed", Some(&detail))
                .await
                .ok();
            self.db
                .append_backend_log("error", "存储迁移", &format!("迁移暂停：{detail}"))
                .await
                .ok();
        }
        self.running.store(false, Ordering::Release);
        *self.cancel.lock().unwrap() = None;
    }

    async fn run_inner(&self, main_target_id: i64, token: &CancellationToken) -> AppResult<()> {
        let main = self.tg.binding(main_target_id)?;
        let mut completed_this_run = 0i64;
        loop {
            if token.is_cancelled() {
                self.db.storage_migration_set_status("paused", None).await?;
                return Ok(());
            }
            // 用户上传优先；有浏览器分块正在发送时不启动新的历史复制消息。
            if self.db.active_upload_claim_count().await? > 0 {
                if !sleep_or_cancel(token, Duration::from_secs(2)).await {
                    self.db.storage_migration_set_status("paused", None).await?;
                    return Ok(());
                }
                continue;
            }
            let Some(candidate) = self.db.storage_migration_candidate(main_target_id).await? else {
                self.db
                    .storage_migration_set_status("completed", None)
                    .await?;
                self.db
                    .append_backend_log("info", "存储迁移", "所有历史消息已迁移到主 Bot")
                    .await
                    .ok();
                return Ok(());
            };

            main.api.wait_upload_turn().await;
            let copied = main
                .api
                .copy_message_from(&candidate.source_chat_id, candidate.source_message_id)
                .await?;
            let updated = match self
                .db
                .finish_storage_message_migration(
                    candidate.source_target_id,
                    candidate.source_message_id,
                    main_target_id,
                    &copied.file_id,
                    copied.message_id,
                    copied.thumbnail_file_id.as_deref(),
                )
                .await
            {
                Ok(updated) => updated,
                Err(error) => {
                    if let Err(cleanup) = main.api.delete_message(copied.message_id).await {
                        tracing::error!(
                            "迁移落库失败({error})，且清理新消息 {} 失败: {cleanup}",
                            copied.message_id
                        );
                    }
                    return Err(error);
                }
            };
            if updated == 0 {
                // 原引用可能在复制期间被用户删除；新消息没有任何引用，立即撤销。
                main.api.delete_message(copied.message_id).await?;
            } else {
                completed_this_run += 1;
                if completed_this_run % 100 == 0 {
                    let status = self.status(main_target_id).await?;
                    self.db
                        .append_backend_log(
                            "info",
                            "存储迁移",
                            &format!(
                                "迁移进度：已完成 {} / {} 条消息，剩余 {} 条",
                                status.migrated_messages,
                                status.total_messages,
                                status.remaining_messages
                            ),
                        )
                        .await
                        .ok();
                }
            }
            if !sleep_or_cancel(token, COPY_INTERVAL).await {
                self.db.storage_migration_set_status("paused", None).await?;
                return Ok(());
            }
        }
    }
}

async fn sleep_or_cancel(token: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        _ = token.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

pub fn primary_target_id(value: Option<i64>) -> AppResult<i64> {
    value.ok_or_else(|| AppError::BadRequest("尚未配置主 Bot 存储目标".into()))
}
