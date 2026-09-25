//! 定期维护任务：回收站清理、断点续传残留清理与多渠道数据库备份。

use crate::db::now_ts;
use crate::error::AppResult;
use crate::settings::Runtime;
use crate::vfs::Vfs;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30 * 60);
const UPLOAD_SESSION_KEEP_SECS: i64 = 7 * 86_400;

async fn record_log(vfs: &Vfs, level: &str, category: &str, message: &str) {
    if let Err(error) = vfs.db.append_backend_log(level, category, message).await {
        tracing::warn!("记录维护日志失败: {error}");
    }
}

pub async fn run(vfs: Vfs, runtime: Arc<Runtime>, task: crate::tasks::TaskReporter) {
    loop {
        if task.is_cancelled() {
            return;
        }
        task.heartbeat();
        match tick(&vfs, &runtime).await {
            Ok(()) => task.clear_error(),
            Err(error) => {
                task.report_error(&error.to_string());
                tracing::warn!("维护任务出错（下轮重试）：{error}");
                record_log(
                    &vfs,
                    "error",
                    "维护",
                    &format!("维护任务失败，下轮重试：{error}"),
                )
                .await;
            }
        }
        if !task.sleep(TICK).await {
            return;
        }
    }
}

async fn tick(vfs: &Vfs, runtime: &Runtime) -> AppResult<()> {
    vfs.uploader.process_pending_deletes();
    let extras = crate::extras::load(&vfs.db).await?;

    let abandoned = vfs
        .db
        .cleanup_expired_upload_sessions(now_ts() - UPLOAD_SESSION_KEEP_SECS)
        .await?;
    if !abandoned.is_empty() {
        tracing::info!("清理过期断点续传任务：释放 {} 条消息", abandoned.len());
        record_log(
            vfs,
            "info",
            "维护",
            &format!("清理过期断点续传任务：释放 {} 条消息", abandoned.len()),
        )
        .await;
        vfs.uploader.delete_messages_best_effort(abandoned);
    }

    if extras.trash_retention_days > 0 {
        let freed = vfs
            .db
            .trash_purge_expired(extras.trash_retention_days)
            .await?;
        if !freed.is_empty() {
            tracing::info!("回收站超期清理：释放 {} 条消息", freed.len());
            record_log(
                vfs,
                "info",
                "维护",
                &format!("回收站超期清理：释放 {} 条消息", freed.len()),
            )
            .await;
            // 清理的是回收站里本就不可见的节点：可见树没有变化，不广播文件事件。
            vfs.uploader.delete_messages_best_effort(freed);
        }
    }

    if let Some(outcome) = crate::backup::automatic_if_due(vfs, runtime, &extras).await? {
        tracing::info!(
            file = %outcome.name,
            destinations = outcome.succeeded,
            "数据库自动备份完成"
        );
        let level = if outcome.failed > 0 { "warn" } else { "info" };
        record_log(
            vfs,
            level,
            "备份",
            &format!(
                "数据库自动备份{}：{}，成功 {} 个渠道，失败 {} 个",
                if outcome.failed > 0 {
                    "部分完成"
                } else {
                    "完成"
                },
                outcome.name,
                outcome.succeeded,
                outcome.failed
            ),
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use crate::config::Config;
    use crate::db::{BackendLogFilter, Db, ROOT_ID};
    use crate::engine::upload::{UploadKnobs, Uploader};
    use crate::error::AppError;
    use crate::settings::RuntimeCfg;
    use crate::telegram::{ByteReader, TgApi, TgMessage, TgRegistry, UploadBody};

    /// 维护任务只会删除消息，其余能力在测试中一律拒绝。
    #[derive(Default)]
    struct DeleteOnlyTg {
        deleted: std::sync::Mutex<Vec<i64>>,
    }

    #[async_trait::async_trait]
    impl TgApi for DeleteOnlyTg {
        async fn send_document(
            &self,
            _: &str,
            _: Option<String>,
            _: Option<&bytes::Bytes>,
            _: &UploadBody,
        ) -> AppResult<TgMessage> {
            Err(AppError::Telegram("维护测试不上传".into()))
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
        vfs: Vfs,
        runtime: Arc<Runtime>,
        target: i64,
        /// 直连同一个 SQLite 文件，用于把时间戳改到过去以模拟“超期”。
        raw: sqlx::SqlitePool,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hk.db");
        let db_path = path.to_str().unwrap().to_string();
        let db = Db::open(&db_path).await.unwrap();
        let env = Arc::new(Config {
            proxy: crate::proxy::ProxyConfig::default(),
            listen: "127.0.0.1:0".parse().unwrap(),
            bot_token: String::new(),
            upload_bot_tokens: vec![],
            chat_id: String::new(),
            api_base: "https://api.example.invalid".into(),
            chunk_size: 1024 * 1024,
            upload_concurrency: 1,
            download_concurrency: crate::config::DEFAULT_DOWNLOAD_CONCURRENCY,
            db_path: db_path.clone(),
            tmp_dir: dir.path().into(),
            local_data_root: dir.path().into(),
            username: "hk-test".into(),
            password: "hk-test-only".into(),
            secret: None,
            cookie_secure: false,
            trusted_proxies: Vec::new(),
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            send_video: false,
        });
        let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
        let target = db
            .ensure_storage_target("https://api.example.invalid", "1:hk", "-1")
            .await
            .unwrap()
            .id;
        let tg = Arc::new(TgRegistry::with_client(
            target,
            Arc::new(DeleteOnlyTg::default()),
        ));
        let knobs = Arc::new(UploadKnobs::new(1024 * 1024, env.tmp_dir.clone(), 1, false));
        let uploader = Uploader::new(tg.clone(), db.clone(), knobs.clone(), 1024 * 1024);
        let auth = Arc::new(
            Auth::init(&env.username, &env.password, None, false, &db)
                .await
                .unwrap(),
        );
        let runtime = Arc::new(Runtime::new(db.clone(), env, cfg, tg.clone(), knobs, auth));
        let raw = sqlx::SqlitePool::connect(&format!("sqlite://{db_path}"))
            .await
            .unwrap();
        Fixture {
            vfs: Vfs { db, tg, uploader },
            runtime,
            target,
            raw,
            _dir: dir,
        }
    }

    impl Fixture {
        /// 建一个就绪文件，单块，消息号为 message_id。
        async fn file(&self, name: &str, message_id: i64) -> i64 {
            let db = &self.vfs.db;
            let id = db.create_uploading(ROOT_ID, name).await.unwrap();
            db.add_chunk(
                id,
                0,
                4,
                &format!("f{message_id}"),
                message_id,
                self.target,
                None,
            )
            .await
            .unwrap();
            db.finalize_upload(id, 4, None).await.unwrap();
            id
        }

        async fn logs(&self) -> Vec<String> {
            self.vfs
                .db
                .backend_logs(100, None, &BackendLogFilter::default())
                .await
                .unwrap()
                .0
                .into_iter()
                .map(|log| format!("{}|{}|{}", log.level, log.category, log.message))
                .collect()
        }
    }

    #[tokio::test]
    async fn tick_purges_only_expired_trash_and_queues_message_deletes() {
        let f = fixture().await;
        let old = f.file("old.bin", 50).await;
        let fresh = f.file("fresh.bin", 51).await;
        f.vfs.delete("/old.bin").await.unwrap();
        f.vfs.delete("/fresh.bin").await.unwrap();
        f.vfs
            .db
            .setting_set("extra.trash_retention_days", "1")
            .await
            .unwrap();
        sqlx::query("UPDATE trash SET trashed_at = trashed_at - 2 * 86400 WHERE node_id = ?")
            .bind(old)
            .execute(&f.raw)
            .await
            .unwrap();
        let pending_before = f.vfs.db.pending_delete_count().await.unwrap();

        tick(&f.vfs, &f.runtime).await.unwrap();

        let trash = f.vfs.db.trash_list().await.unwrap();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].0.id, fresh);
        assert!(matches!(f.vfs.db.node(old).await, Err(AppError::NotFound)));
        // 默认手动清理模式：消息进入持久化删除队列，等待管理员执行。
        assert_eq!(
            f.vfs.db.pending_delete_count().await.unwrap(),
            pending_before + 1
        );
        assert!(
            f.logs()
                .await
                .iter()
                .any(|l| l == "info|维护|回收站超期清理：释放 1 条消息")
        );
    }

    #[tokio::test]
    async fn tick_keeps_trash_forever_when_retention_disabled() {
        let f = fixture().await;
        let id = f.file("keep.bin", 60).await;
        f.vfs.delete("/keep.bin").await.unwrap();
        f.vfs
            .db
            .setting_set("extra.trash_retention_days", "0")
            .await
            .unwrap();
        sqlx::query("UPDATE trash SET trashed_at = 0")
            .execute(&f.raw)
            .await
            .unwrap();

        tick(&f.vfs, &f.runtime).await.unwrap();

        assert_eq!(f.vfs.db.trash_list().await.unwrap()[0].0.id, id);
        assert!(f.logs().await.iter().all(|l| !l.contains("回收站超期清理")));
    }

    #[tokio::test]
    async fn tick_rolls_back_stale_resumable_uploads_but_keeps_recent_ones() {
        let f = fixture().await;
        let sha = "a".repeat(64);
        let stale = f
            .vfs
            .db
            .create_upload_session(ROOT_ID, "stale.bin", "/stale.bin", 8, &sha, 4, f.target)
            .await
            .unwrap();
        f.vfs
            .db
            .add_chunk(stale.session.node_id, 0, 4, "s0", 70, f.target, None)
            .await
            .unwrap();
        let recent = f
            .vfs
            .db
            .create_upload_session(ROOT_ID, "recent.bin", "/recent.bin", 8, &sha, 4, f.target)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_sessions SET updated_at = updated_at - 8 * 86400 WHERE id = ?")
            .bind(&stale.session.id)
            .execute(&f.raw)
            .await
            .unwrap();

        tick(&f.vfs, &f.runtime).await.unwrap();

        let sessions: Vec<String> = f
            .vfs
            .db
            .upload_sessions()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.session.id)
            .collect();
        assert_eq!(sessions, [recent.session.id]);
        assert!(matches!(
            f.vfs.db.node(stale.session.node_id).await,
            Err(AppError::NotFound)
        ));
        assert!(
            f.logs()
                .await
                .iter()
                .any(|l| l == "info|维护|清理过期断点续传任务：释放 1 条消息")
        );
    }

    #[tokio::test]
    async fn tick_is_quiet_when_nothing_to_do() {
        let f = fixture().await;
        f.file("live.bin", 80).await;
        tick(&f.vfs, &f.runtime).await.unwrap();
        assert!(f.logs().await.is_empty());
        assert_eq!(f.vfs.db.pending_delete_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn auto_backup_without_channels_fails_tick_and_run_reports_it() {
        let f = fixture().await;
        f.vfs
            .db
            .setting_set("extra.backup_auto", "1")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_telegram_enabled", "0")
            .await
            .unwrap();
        let error = tick(&f.vfs, &f.runtime).await.unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)), "{error}");

        // 循环体把错误写入任务状态与后台日志，并继续等待下一轮而不是退出。
        let tasks = crate::tasks::TaskManager::new();
        let (vfs, runtime) = (f.vfs.clone(), f.runtime.clone());
        tasks.supervise("housekeeping", move |reporter| {
            run(vfs.clone(), runtime.clone(), reporter)
        });
        let mut reported = None;
        for _ in 0..200 {
            reported = tasks
                .snapshots()
                .into_iter()
                .find(|s| s.name == "housekeeping")
                .and_then(|s| s.last_error);
            if reported.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tasks.cancel();
        assert!(reported.unwrap().contains("没有启用任何备份渠道"));
        let snapshot = tasks
            .snapshots()
            .into_iter()
            .find(|s| s.name == "housekeeping")
            .unwrap();
        assert_eq!(snapshot.restarts, 0);
        assert!(
            f.logs()
                .await
                .iter()
                .any(|l| l.starts_with("error|维护|维护任务失败，下轮重试"))
        );
    }
}
