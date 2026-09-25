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

/// 两次复制之间的间隔；测试中缩短以免每条消息都真实等待 3 秒。
#[cfg(not(test))]
const COPY_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(test)]
const COPY_INTERVAL: Duration = Duration::from_millis(10);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{BackendLogFilter, ROOT_ID};
    use crate::telegram::{ByteReader, TgApi, TgMessage, UploadBody};
    use std::sync::atomic::AtomicI64;

    /// 迁移专用的 Telegram 替身：记录复制与删除，可注入失败或挂起复制请求。
    #[derive(Default)]
    struct CopyTg {
        seq: AtomicI64,
        copies: Mutex<Vec<(String, i64)>>,
        deleted: Mutex<Vec<i64>>,
        fail_copy: AtomicBool,
        block_copy: AtomicBool,
        copy_started: tokio::sync::Notify,
        copy_release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl TgApi for CopyTg {
        async fn send_document(
            &self,
            _: &str,
            _: Option<String>,
            _: Option<&bytes::Bytes>,
            _: &UploadBody,
        ) -> AppResult<TgMessage> {
            Err(AppError::Telegram("迁移测试不上传".into()))
        }
        async fn copy_message_from(
            &self,
            from_chat_id: &str,
            message_id: i64,
        ) -> AppResult<TgMessage> {
            if self.block_copy.load(Ordering::SeqCst) {
                self.copy_started.notify_one();
                self.copy_release.notified().await;
            }
            if self.fail_copy.load(Ordering::SeqCst) {
                return Err(AppError::Telegram("mock: 复制被拒绝".into()));
            }
            self.copies
                .lock()
                .unwrap()
                .push((from_chat_id.to_string(), message_id));
            let n = self.seq.fetch_add(1, Ordering::SeqCst);
            Ok(TgMessage {
                message_id: 9000 + n,
                file_id: format!("new-{message_id}"),
                thumbnail_file_id: Some(format!("thumb-{message_id}")),
            })
        }
        async fn delete_message(&self, message_id: i64) -> AppResult<()> {
            self.deleted.lock().unwrap().push(message_id);
            Ok(())
        }
        async fn get_me(&self) -> AppResult<String> {
            Ok("mock".into())
        }
        async fn open_chunk_stream(
            &self,
            _: &str,
            _: i64,
            _: u64,
            _: u64,
        ) -> AppResult<ByteReader> {
            Err(AppError::NotFound)
        }
    }

    struct Fixture {
        db: Db,
        tg: Arc<CopyTg>,
        manager: StorageMigrationManager,
        main: i64,
        old: i64,
        _dir: tempfile::TempDir,
    }

    /// 建立主目标（已注册客户端）与旧目标：
    /// a.bin 有两块（消息 10、11），b.bin 与 a.bin 共享消息 10（去重引用）。
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("m.db").to_str().unwrap())
            .await
            .unwrap();
        let main = db
            .ensure_storage_target("https://api.example.invalid", "111:main", "-100main")
            .await
            .unwrap()
            .id;
        let old = db
            .ensure_storage_target("https://api.example.invalid", "222:old", "-100old")
            .await
            .unwrap()
            .id;
        let a = db.create_uploading(ROOT_ID, "a.bin").await.unwrap();
        db.add_chunk(a, 0, 4, "old-10", 10, old, None)
            .await
            .unwrap();
        db.add_chunk(a, 1, 4, "old-11", 11, old, None)
            .await
            .unwrap();
        db.finalize_upload(a, 8, None).await.unwrap();
        let b = db.create_uploading(ROOT_ID, "b.bin").await.unwrap();
        db.add_chunk(b, 0, 4, "old-10", 10, old, None)
            .await
            .unwrap();
        db.finalize_upload(b, 4, None).await.unwrap();
        let tg = Arc::new(CopyTg::default());
        let registry = Arc::new(TgRegistry::with_client(main, tg.clone()));
        let manager = StorageMigrationManager::new(db.clone(), registry);
        Fixture {
            db,
            tg,
            manager,
            main,
            old,
            _dir: dir,
        }
    }

    /// 等待后台任务退出（running 归位）。
    async fn wait_idle(manager: &StorageMigrationManager) {
        for _ in 0..500 {
            if !manager.running.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("迁移任务未在预期时间内结束");
    }

    async fn all_chunks(db: &Db) -> Vec<crate::db::Chunk> {
        let mut out = Vec::new();
        for name in ["a.bin", "b.bin"] {
            if let Some(node) = db.child(ROOT_ID, name).await.unwrap() {
                out.extend(db.chunks_of(node.id).await.unwrap());
            }
        }
        out
    }

    async fn log_messages(db: &Db) -> Vec<String> {
        db.backend_logs(100, None, &BackendLogFilter::default())
            .await
            .unwrap()
            .0
            .into_iter()
            .map(|log| format!("{}|{}|{}", log.level, log.category, log.message))
            .collect()
    }

    #[tokio::test]
    async fn status_before_start_counts_distinct_messages_and_shared_chunks() {
        let f = fixture().await;
        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "idle");
        assert!(!status.running);
        assert_eq!(status.main_bot_id, "111");
        assert_eq!(status.remaining_messages, 2);
        assert_eq!(status.remaining_chunks, 3);
        assert_eq!(status.total_messages, 2);
        assert_eq!(status.migrated_messages, 0);
        assert_eq!(status.sources.len(), 1);
        assert_eq!(status.sources[0].storage_target_id, f.old);
        assert_eq!(status.sources[0].bot_id, "222");
        assert_eq!(status.sources[0].bytes, 12);
        assert!(status.started_at.is_none());
    }

    #[tokio::test]
    async fn migration_copies_each_message_once_and_repoints_all_chunks() {
        let f = fixture().await;
        let started = f.manager.start(f.main).await.unwrap();
        assert_eq!(started.total_messages, 2);
        wait_idle(&f.manager).await;

        // 每条旧消息只复制一次，且从旧频道复制。
        assert_eq!(
            *f.tg.copies.lock().unwrap(),
            [("-100old".to_string(), 10), ("-100old".to_string(), 11)]
        );
        assert!(f.tg.deleted.lock().unwrap().is_empty());
        let chunks = all_chunks(&f.db).await;
        assert_eq!(chunks.len(), 3);
        for chunk in &chunks {
            assert_eq!(chunk.storage_target_id, Some(f.main));
            assert!(chunk.tg_file_id.starts_with("new-"), "{}", chunk.tg_file_id);
        }
        // 共享消息 10 的两个引用切到同一条新消息。
        let shared: Vec<_> = chunks.iter().filter(|c| c.tg_file_id == "new-10").collect();
        assert_eq!(shared.len(), 2);
        assert!(shared.iter().all(|c| c.tg_message_id == 9000));
        assert!(
            shared
                .iter()
                .all(|c| c.tg_thumbnail_file_id.as_deref() == Some("thumb-10"))
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.tg_file_id == "new-11" && c.tg_message_id == 9001)
        );

        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "completed");
        assert_eq!(status.migrated_messages, 2);
        assert_eq!(status.migrated_chunks, 3);
        assert_eq!(status.remaining_messages, 0);
        assert_eq!(status.total_chunks, 3);
        assert!(status.sources.is_empty());
        let job = f.db.storage_migration_job().await.unwrap().unwrap();
        assert_eq!(job.status, "completed");
        let logs = log_messages(&f.db).await;
        assert!(
            logs.iter()
                .any(|l| l.contains("所有历史消息已迁移到主 Bot"))
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("开始把 2 条 Telegram 消息（3 个分块引用）"))
        );
    }

    #[tokio::test]
    async fn start_with_nothing_to_migrate_completes_without_copying() {
        let f = fixture().await;
        // 先把全部分块迁完，再次启动时应直接判定完成。
        f.manager.start(f.main).await.unwrap();
        wait_idle(&f.manager).await;
        let copies = f.tg.copies.lock().unwrap().len();
        let status = f.manager.start(f.main).await.unwrap();
        assert_eq!(status.status, "completed");
        assert!(!status.running);
        assert_eq!(f.tg.copies.lock().unwrap().len(), copies);
        // 同一主目标保留历史进度。
        assert_eq!(status.migrated_messages, 2);
    }

    #[tokio::test]
    async fn start_rejects_unregistered_main_target() {
        let f = fixture().await;
        let error = f.manager.start(f.old).await.unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)), "{error}");
        assert!(f.db.storage_migration_job().await.unwrap().is_none());
        assert!(!f.manager.running.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn copy_failure_marks_failed_keeps_mapping_and_retry_resumes() {
        let f = fixture().await;
        f.tg.fail_copy.store(true, Ordering::SeqCst);
        f.manager.start(f.main).await.unwrap();
        wait_idle(&f.manager).await;

        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "failed");
        assert!(!status.running);
        assert!(
            status.last_error.as_deref().unwrap().contains("复制被拒绝"),
            "{:?}",
            status.last_error
        );
        assert_eq!(status.migrated_messages, 0);
        // 失败时原映射保持不变。
        assert!(
            all_chunks(&f.db)
                .await
                .iter()
                .all(|c| c.storage_target_id == Some(f.old) && c.tg_file_id.starts_with("old-"))
        );
        assert!(
            log_messages(&f.db)
                .await
                .iter()
                .any(|l| l.starts_with("error|存储迁移|迁移暂停"))
        );

        // 故障解除后重新启动，清除错误并完成剩余消息。
        f.tg.fail_copy.store(false, Ordering::SeqCst);
        let restarted = f.manager.start(f.main).await.unwrap();
        assert!(restarted.last_error.is_none());
        wait_idle(&f.manager).await;
        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "completed");
        assert_eq!(status.migrated_messages, 2);
        assert!(status.last_error.is_none());
    }

    #[tokio::test]
    async fn pause_lets_inflight_copy_land_then_resume_finishes() {
        let f = fixture().await;
        f.tg.block_copy.store(true, Ordering::SeqCst);
        f.manager.start(f.main).await.unwrap();
        f.tg.copy_started.notified().await;

        // 重复启动在运行期间不会再派生第二个任务。
        let again = f.manager.start(f.main).await.unwrap();
        assert!(again.running);
        assert_eq!(again.status, "running");

        let paused = f.manager.pause(f.main).await.unwrap();
        // 在途复制尚未落库时仍报告运行中。
        assert!(paused.running);
        f.tg.block_copy.store(false, Ordering::SeqCst);
        f.tg.copy_release.notify_one();
        wait_idle(&f.manager).await;

        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "paused");
        assert_eq!(status.migrated_messages, 1);
        assert_eq!(status.remaining_messages, 1);
        assert_eq!(status.total_messages, 2);
        assert_eq!(f.tg.copies.lock().unwrap().len(), 1);

        // 恢复后继续剩余消息，进度累计而非重置。
        let resumed = f.manager.start(f.main).await.unwrap();
        assert_eq!(resumed.total_messages, 2);
        wait_idle(&f.manager).await;
        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "completed");
        assert_eq!(status.migrated_messages, 2);
        assert_eq!(status.migrated_chunks, 3);
    }

    #[tokio::test]
    async fn source_deleted_during_copy_revokes_orphan_new_message() {
        let f = fixture().await;
        f.tg.block_copy.store(true, Ordering::SeqCst);
        f.manager.start(f.main).await.unwrap();
        f.tg.copy_started.notified().await;
        // 复制消息 10 期间用户删掉了引用它的两个文件。
        for name in ["a.bin", "b.bin"] {
            let node = f.db.child(ROOT_ID, name).await.unwrap().unwrap();
            f.db.delete_node(node.id).await.unwrap();
        }
        f.tg.block_copy.store(false, Ordering::SeqCst);
        f.tg.copy_release.notify_one();
        wait_idle(&f.manager).await;

        // 新复制出的消息没有任何引用，立即撤销；不计入进度。
        assert_eq!(*f.tg.deleted.lock().unwrap(), [9000]);
        let status = f.manager.status(f.main).await.unwrap();
        assert_eq!(status.status, "completed");
        assert_eq!(status.migrated_messages, 0);
    }

    #[tokio::test]
    async fn shutdown_stops_running_migration_as_paused() {
        let f = fixture().await;
        f.manager.start(f.main).await.unwrap();
        f.manager.shutdown().await;
        assert!(!f.manager.running.load(Ordering::Acquire));
        let job = f.db.storage_migration_job().await.unwrap().unwrap();
        assert!(
            matches!(job.status.as_str(), "paused" | "completed"),
            "{}",
            job.status
        );
    }

    #[test]
    fn primary_target_id_requires_configuration() {
        assert_eq!(primary_target_id(Some(7)).unwrap(), 7);
        assert!(matches!(
            primary_target_id(None),
            Err(AppError::BadRequest(_))
        ));
    }
}
