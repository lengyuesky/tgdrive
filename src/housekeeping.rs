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
    if vfs.db.background_blocked() {
        return Ok(());
    }
    let cleanup_error = crate::apps::cleanup_caches(&vfs.db, &runtime.env.db_path)
        .await
        .err();
    if let Some(error) = &cleanup_error {
        tracing::warn!(%error, "旧应用缓存清理失败，继续执行其他维护任务");
        record_log(
            vfs,
            "error",
            "维护",
            &format!("旧应用缓存清理失败，将重试：{error}"),
        )
        .await;
    }
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
    cleanup_error.map_or(Ok(()), Err)
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

    // 自动备份使用进程级 try_lock；真正投递备份的维护测试须串行。
    static BACKUP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
            // report_error 先于异步落库；不能在日志事务完成前 cancel worker。
            if reported.is_some()
                && f.logs()
                    .await
                    .iter()
                    .any(|l| l.starts_with("error|维护|维护任务失败，下轮重试"))
            {
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

    /// 本地 mock S3：只计数收到的 PUT，验证 tick 是否真的投递了快照。
    async fn mock_s3_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<()>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::body::to_bytes;
        use axum::extract::Request;
        use axum::http::StatusCode;
        let puts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = puts.clone();
        let app = axum::Router::new().fallback(move |request: Request| {
            let state = state.clone();
            async move {
                let (parts, body) = request.into_parts();
                let _ = to_bytes(body, usize::MAX).await.unwrap();
                if parts.method == axum::http::Method::PUT {
                    state.lock().unwrap().push(());
                }
                StatusCode::OK
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), puts, task)
    }

    async fn enable_skip_backup(f: &Fixture, s3_endpoint: &str) {
        // 只用 S3 渠道验证，显式关闭默认开启的 Telegram 渠道（测试环境未配置存储）。
        f.vfs
            .db
            .setting_set("extra.backup_telegram_enabled", "0")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_auto", "1")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_skip_if_unchanged", "1")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_s3_enabled", "1")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_s3_endpoint", s3_endpoint)
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_s3_bucket", "backup-bucket")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_s3_access_key", "AKIDEXAMPLE")
            .await
            .unwrap();
        f.vfs
            .db
            .setting_set("extra.backup_s3_secret_key", "secret")
            .await
            .unwrap();
    }

    /// 把 S3 渠道的上次成功时间改到 25 小时前，模拟「该投递了」。
    async fn expire_s3_last_ts(f: &Fixture) {
        sqlx::query(
            "INSERT INTO settings (k, v) VALUES ('extra.backup_s3_last_ts', ?)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        )
        .bind(crate::db::now_ts() - 25 * 3600)
        .execute(&f.raw)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn auto_backup_skips_when_data_revision_unchanged() {
        let _guard = BACKUP_TEST_LOCK.lock().await;
        let f = fixture().await;
        let (endpoint, puts, server) = mock_s3_server().await;
        enable_skip_backup(&f, &endpoint).await;

        // 首次：从未成功备份（基准为 None），视为已变化，正常投递并记录基准。
        expire_s3_last_ts(&f).await;
        tick(&f.vfs, &f.runtime).await.unwrap();
        assert_eq!(puts.lock().unwrap().len(), 1);
        let stored = f
            .vfs
            .db
            .setting_get("extra.backup_s3_data_revision")
            .await
            .unwrap();
        assert!(stored.is_some(), "成功后应记录修订号基准");

        // 数据无变化：同样到期，但基准一致，本轮跳过。
        let logs_before = f.logs().await;
        expire_s3_last_ts(&f).await;
        tick(&f.vfs, &f.runtime).await.unwrap();
        assert_eq!(puts.lock().unwrap().len(), 1, "数据库无变化时不应重复投递");
        assert_eq!(f.logs().await, logs_before, "跳过的轮次不应新增备份日志");

        // 数据变化（新建目录）：基准失效，恢复投递并更新基准。
        expire_s3_last_ts(&f).await;
        f.vfs.mkdir("/新目录").await.unwrap();
        tick(&f.vfs, &f.runtime).await.unwrap();
        assert_eq!(puts.lock().unwrap().len(), 2);
        let after = f.vfs.db.data_revision().await.unwrap().to_string();
        assert_eq!(
            f.vfs
                .db
                .setting_get("extra.backup_s3_data_revision")
                .await
                .unwrap()
                .as_deref(),
            Some(after.as_str()),
            "变化后应把基准更新到最新修订号"
        );

        // 无基准的渠道（新启用或从未成功）即使数据无变化也必须拿到第一份备份：
        // 直接清掉基准键模拟（该键在排除清单里，删除不改变修订号）。
        f.vfs
            .db
            .setting_del("extra.backup_s3_data_revision")
            .await
            .unwrap();
        expire_s3_last_ts(&f).await;
        tick(&f.vfs, &f.runtime).await.unwrap();
        assert_eq!(
            puts.lock().unwrap().len(),
            3,
            "基准缺失的渠道视为已变化，必须投递"
        );
        server.abort();
    }

    #[tokio::test]
    async fn cleanup_failure_still_reclaims_uploads_and_trash_and_backs_up() {
        let _guard = BACKUP_TEST_LOCK.lock().await;
        let f = fixture().await;
        let old = f.file("old.bin", 90).await;
        f.vfs.delete("/old.bin").await.unwrap();
        f.vfs
            .db
            .setting_set("extra.trash_retention_days", "1")
            .await
            .unwrap();
        sqlx::query("UPDATE trash SET trashed_at = 0 WHERE node_id = ?")
            .bind(old)
            .execute(&f.raw)
            .await
            .unwrap();
        let stale = f
            .vfs
            .db
            .create_upload_session(
                ROOT_ID,
                "stale.bin",
                "/stale.bin",
                8,
                &"a".repeat(64),
                4,
                f.target,
            )
            .await
            .unwrap();
        f.vfs
            .db
            .add_chunk(stale.session.node_id, 0, 4, "s0", 91, f.target, None)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_sessions SET updated_at = 0 WHERE id = ?")
            .bind(&stale.session.id)
            .execute(&f.raw)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app_cleanup_jobs(app_id, token, created_at) VALUES('books', 'failed-cleanup', 0)")
            .execute(&f.raw).await.unwrap();
        let root = f._dir.path().join("apps");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("books"), b"blocked")
            .await
            .unwrap();
        let (endpoint, puts, server) = mock_s3_server().await;
        enable_skip_backup(&f, &endpoint).await;
        expire_s3_last_ts(&f).await;

        let error = tick(&f.vfs, &f.runtime).await.unwrap_err();
        server.abort();
        assert!(
            matches!(error, AppError::Io(_)),
            "应保留清理失败而非清除任务错误: {error}"
        );
        assert!(f.vfs.db.upload_sessions().await.unwrap().is_empty());
        assert!(f.vfs.db.trash_list().await.unwrap().is_empty());
        assert!(matches!(f.vfs.db.node(old).await, Err(AppError::NotFound)));
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "封面清理失败仍应投递数据库备份"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM app_cleanup_jobs")
                .fetch_one(&f.raw)
                .await
                .unwrap(),
            1
        );
        let logs = f.logs().await;
        assert!(
            logs.iter()
                .any(|log| log.starts_with("error|维护|旧应用缓存清理失败"))
        );
        assert!(logs.iter().any(|log| log.contains("清理过期断点续传任务")));
        assert!(logs.iter().any(|log| log.contains("回收站超期清理")));
        assert!(logs.iter().any(|log| log.contains("数据库自动备份完成")));
    }
}
