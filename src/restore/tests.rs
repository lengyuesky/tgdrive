use super::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::settings::{Runtime, RuntimeCfg};
use crate::vfs::Vfs;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

fn config(dir: &Path) -> Config {
    Config {
        proxy: crate::proxy::ProxyConfig::default(),
        listen: "127.0.0.1:0".parse().unwrap(),
        bot_token: String::new(),
        upload_bot_tokens: vec![],
        chat_id: String::new(),
        api_base: "http://127.0.0.1:1".into(),
        chunk_size: 1024 * 1024,
        upload_concurrency: 1,
        download_concurrency: 1,
        db_path: dir.join("main.db").to_string_lossy().to_string(),
        tmp_dir: dir.to_path_buf(),
        local_data_root: dir.to_path_buf(),
        username: "current-admin".into(),
        password: "current-password".into(),
        secret: None,
        cookie_secure: false,
        trusted_proxies: vec![],
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: false,
    }
}
async fn fixture(dir: &Path) -> AppState {
    fixture_config(dir, config(dir)).await
}
async fn fixture_config(dir: &Path, config: Config) -> AppState {
    let env = Arc::new(config);
    let db = Db::open(&env.db_path).await.unwrap();
    let mut cfg = RuntimeCfg::load(&db, &env).await.unwrap();
    if cfg.is_configured() {
        let target = db
            .ensure_storage_target(&cfg.api_base, &cfg.bot_token, &cfg.chat_id)
            .await
            .unwrap();
        cfg.storage_target_id = Some(target.id);
        cfg.upload_target_ids = vec![target.id];
    }
    let tg = Arc::new(TgRegistry::new(&[], None, vec![], None, cfg.proxy.clone()).unwrap());
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        dir.to_path_buf(),
        1,
        false,
    ));
    let auth = Arc::new(
        Auth::init(&cfg.username, &cfg.password, None, false, &db)
            .await
            .unwrap(),
    );
    let rt = Arc::new(Runtime::new(
        db.clone(),
        env.clone(),
        cfg,
        tg.clone(),
        knobs.clone(),
        auth.clone(),
    ));
    let vfs = Vfs {
        db: db.clone(),
        tg: tg.clone(),
        uploader: Uploader::new(tg.clone(), db.clone(), knobs, 1024 * 1024),
    };
    AppState {
        dav: crate::dav::build_handler(vfs.clone()),
        vfs,
        auth,
        rt,
        attempts: Default::default(),
        tasks: crate::tasks::TaskManager::new(),
        migration: crate::migration::StorageMigrationManager::new(db, tg),
        restore: RestoreManager::new(&env.db_path).unwrap(),
        apps_catalog: Arc::new(crate::apps::CatalogManager::disabled()),
        events: crate::events::bus(),
        range_cache: Default::default(),
    }
}
async fn candidate(root: &Path) -> PathBuf {
    let path = root.join("candidate.db");
    let db = Db::open(path.to_str().unwrap()).await.unwrap();
    db.setting_set("fixture", "candidate").await.unwrap();
    db.setting_set(
        "cfg.password",
        &crate::password::hash("candidate-password").unwrap(),
    )
    .await
    .unwrap();
    db.close().await;
    path
}
async fn ready(state: &AppState) -> String {
    let path = candidate(&state.restore.root).await;
    let hash = storage::digest(&path).await.unwrap();
    let id = format!("test-{hash}");
    let mut inner = state.restore.inner.lock().await;
    inner.journal.status.phase = "ready".into();
    inner.journal.status.id = Some(id.clone());
    inner.journal.stage = "ready".into();
    inner.journal.digest = Some(hash);
    storage::save(&state.restore.root, &inner.journal).unwrap();
    id
}
fn app(state: &AppState) -> axum::Router {
    axum::Router::new()
        .nest("/api", crate::api::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::api::restore::write_guard,
        ))
}
async fn request(
    state: &AppState,
    method: &str,
    route: &str,
    body: serde_json::Value,
    cookie: bool,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/admin/restore/{route}"))
        .header("content-type", "application/json");
    if cookie {
        request = request.header(
            "cookie",
            state
                .auth
                .cookie_header(&state.auth.issue_token())
                .split(';')
                .next()
                .unwrap(),
        );
    }
    let response = app(state)
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let data = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&data).unwrap())
}

#[derive(Default)]
struct FakeTelegram {
    history_pages:
        std::sync::Mutex<std::collections::VecDeque<AppResult<crate::telegram::TgHistoryPage>>>,
    history_cursors: std::sync::Mutex<Vec<Option<i32>>>,
    files: std::collections::HashMap<i64, (String, Vec<u8>, u64)>,
    deleted: std::sync::Mutex<Vec<i64>>,
    forward_to_source: bool,
    stall: bool,
}
#[async_trait::async_trait]
impl TgApi for FakeTelegram {
    async fn backup_history_page(
        &self,
        before: Option<i32>,
    ) -> AppResult<crate::telegram::TgHistoryPage> {
        self.history_cursors.lock().unwrap().push(before);
        self.history_pages.lock().unwrap().pop_front().unwrap()
    }
    async fn send_document(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<&bytes::Bytes>,
        _: &crate::telegram::UploadBody,
    ) -> AppResult<crate::telegram::TgMessage> {
        unreachable!()
    }
    async fn delete_message(&self, id: i64) -> AppResult<()> {
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }
    async fn get_me(&self) -> AppResult<String> {
        Ok("test".into())
    }
    async fn forward_backup_document(
        &self,
        _: &str,
        id: i64,
    ) -> AppResult<crate::telegram::TgBackupDocument> {
        let (name, _, size) = self.files.get(&id).unwrap();
        Ok(crate::telegram::TgBackupDocument {
            message_id: if self.forward_to_source {
                22
            } else {
                id + 1000
            },
            file_id: id.to_string(),
            file_name: Some(name.clone()),
            file_size: Some(*size),
        })
    }
    async fn open_chunk_stream(
        &self,
        file_id: &str,
        _: i64,
        _: u64,
        _: u64,
    ) -> AppResult<crate::telegram::ByteReader> {
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(Box::pin(std::io::Cursor::new(
            self.files[&file_id.parse::<i64>().unwrap()].1.clone(),
        )))
    }
}
#[tokio::test]
async fn discover_without_local_history_handles_split_pages_and_retry_then_resolves_source() {
    use crate::telegram::{TgHistoryDocument, TgHistoryPage};
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.bot_token = "test".into();
    cfg.chat_id = "-100123".into();
    let mut state = fixture_config(dir.path(), cfg).await;
    let target = state.rt.current().storage_target_id.unwrap();
    let fake = Arc::new(FakeTelegram::default());
    state.vfs.tg = Arc::new(TgRegistry::with_client(target, fake.clone()));
    let doc = |id, name: &str| TgHistoryDocument {
        message_id: id,
        file_name: name.into(),
    };
    fake.history_pages.lock().unwrap().extend([
        Ok(TgHistoryPage {
            documents: vec![doc(1500, "tgdrive-backup-test.db.age.part0002-of-0002")],
            next_before: Some(1001),
            scanned: 1000,
        }),
        Err(AppError::Telegram("模拟限流".into())),
        Ok(TgHistoryPage {
            documents: vec![
                doc(42, "tgdrive-backup-test.db.age.part0001-of-0002"),
                doc(3, "tgdrive-backup-older.db"),
            ],
            next_before: None,
            scanned: 1000,
        }),
    ]);
    assert!(history(&state).await.unwrap().is_empty());
    let (code, first) = request(
        &state,
        "POST",
        "discover",
        serde_json::json!({"reset":true}),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(first["backups"][0]["available"], false);
    assert_eq!(first["has_more"], true);
    let (code, failed) = request(&state, "POST", "discover", serde_json::json!({}), true).await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert_eq!(failed["error"], "上游存储暂时不可用");
    let (code, found) = request(&state, "POST", "discover", serde_json::json!({}), true).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(found["backups"][0]["available"], true);
    assert_eq!(found["backups"][0]["parts"], 2);
    assert_eq!(found["backups"].as_array().unwrap().len(), 2);
    assert_eq!(found["has_more"], false);
    assert_eq!(found["scanned"], 2000);
    assert_eq!(
        *fake.history_cursors.lock().unwrap(),
        vec![None, Some(1001), Some(1001)]
    );
    let source = resolve_source(
        &state,
        &PrepareRequest {
            backup_id: Some(found["backups"][0]["id"].as_str().unwrap().into()),
            messages: None,
            channel: None,
            password: None,
            acknowledge_forward: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(source.ids, vec![42, 1500]);
    assert_eq!(source.channel, "-100123");
    assert!(Arc::ptr_eq(&source.api, &(fake.clone() as Arc<dyn TgApi>)));
    assert!(
        history(&state).await.unwrap().is_empty(),
        "不能把探测结果写入滚动删除历史"
    );
    assert!(fake.deleted.lock().unwrap().is_empty());
    let (code, _) = request(&state, "POST", "discover", serde_json::json!({}), false).await;
    assert_ne!(code, StatusCode::OK);
    state.vfs.db.close().await;
}

#[tokio::test]
async fn discovery_stops_at_five_complete_backups_and_does_not_read_more_pages() {
    use crate::telegram::{TgHistoryDocument, TgHistoryPage};
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.bot_token = "test".into();
    cfg.chat_id = "-100123".into();
    let mut state = fixture_config(dir.path(), cfg).await;
    let fake = Arc::new(FakeTelegram::default());
    state.vfs.tg = Arc::new(TgRegistry::with_client(
        state.rt.current().storage_target_id.unwrap(),
        fake.clone(),
    ));
    let doc = |message_id, file_name: String| TgHistoryDocument {
        message_id,
        file_name,
    };
    let mut first: Vec<_> = (200..204)
        .map(|id| doc(id, format!("tgdrive-backup-{id}.db")))
        .collect();
    first.push(doc(220, "tgdrive-backup-newest.db.part0002-of-0002".into()));
    fake.history_pages.lock().unwrap().extend([
        Ok(TgHistoryPage {
            documents: first,
            next_before: Some(100),
            scanned: 100,
        }),
        Ok(TgHistoryPage {
            documents: vec![
                doc(42, "tgdrive-backup-newest.db.part0001-of-0002".into()),
                doc(20, "tgdrive-backup-older.db".into()),
                doc(18, "tgdrive-backup-oldest.db".into()),
            ],
            next_before: Some(1),
            scanned: 99,
        }),
    ]);
    let (_, first) = request(
        &state,
        "POST",
        "discover",
        serde_json::json!({"reset": true}),
        true,
    )
    .await;
    assert_eq!(first["has_more"], true, "只有四份完整备份时不能停止");
    assert_eq!(first["backups"].as_array().unwrap().len(), 5);
    let (_, result) = request(&state, "POST", "discover", serde_json::json!({}), true).await;
    assert_eq!(result["has_more"], false);
    let backups = result["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 5, "同一批返回更多备份时只保留最近五份");
    assert!(backups.iter().all(|b| b["available"] == true));
    assert_eq!(backups[0]["name"], "tgdrive-backup-newest.db");
    assert_eq!(backups[0]["parts"], 2, "多个分块只能计作一份备份");
    assert_eq!(backups[4]["name"], "tgdrive-backup-200.db");
    let (_, repeated) = request(&state, "POST", "discover", serde_json::json!({}), true).await;
    assert_eq!(result, repeated);
    assert_eq!(*fake.history_cursors.lock().unwrap(), vec![None, Some(100)]);
    assert_eq!(
        state
            .restore
            .discovered_records(&state)
            .await
            .unwrap()
            .len(),
        5
    );
    state.vfs.db.close().await;
}

#[tokio::test]
async fn download_reorders_parts_checks_lengths_and_never_deletes_sources() {
    let seed = tempfile::tempdir().unwrap();
    let path = candidate(seed.path()).await;
    let bytes = std::fs::read(path).unwrap();
    let split = bytes.len() / 2;
    for short in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let manager = RestoreManager::new(&config(dir.path()).db_path).unwrap();
        let fake = Arc::new(FakeTelegram {
            files: std::collections::HashMap::from([
                (
                    9,
                    (
                        "test.db.part0002-of-0002".into(),
                        bytes[split..].to_vec(),
                        (bytes.len() - split) as u64 + u64::from(short),
                    ),
                ),
                (
                    22,
                    (
                        "test.db.part0001-of-0002".into(),
                        bytes[..split].to_vec(),
                        split as u64,
                    ),
                ),
            ]),
            ..Default::default()
        });
        let source = Source {
            api: fake.clone(),
            channel: "-100123".into(),
            ids: vec![9, 22],
        };
        let result = manager.download_candidate(source, None).await;
        assert_eq!(result.is_ok(), !short, "{result:?}");
        let deleted = fake.deleted.lock().unwrap();
        assert!(deleted.contains(&1009));
        assert!(!deleted.contains(&9) && !deleted.contains(&22));
        if !short {
            assert!(deleted.contains(&1022));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let manager = RestoreManager::new(&config(dir.path()).db_path).unwrap();
    let fake = Arc::new(FakeTelegram {
        files: std::collections::HashMap::from([(
            9,
            ("test.db".into(), bytes.clone(), bytes.len() as u64),
        )]),
        forward_to_source: true,
        ..Default::default()
    });
    assert!(
        manager
            .download_candidate(
                Source {
                    api: fake.clone(),
                    channel: "-100123".into(),
                    ids: vec![9, 22]
                },
                None
            )
            .await
            .is_err()
    );
    assert!(fake.deleted.lock().unwrap().is_empty());
}
#[tokio::test]
async fn stalled_download_can_cancel_and_cleans_known_temporary_message() {
    let dir = tempfile::tempdir().unwrap();
    let manager = RestoreManager::new(&config(dir.path()).db_path).unwrap();
    let fake = Arc::new(FakeTelegram {
        files: std::collections::HashMap::from([(9, ("test.db".into(), vec![], 100))]),
        stall: true,
        ..Default::default()
    });
    let worker = manager.clone();
    let source = Source {
        api: fake.clone(),
        channel: "-100123".into(),
        ids: vec![9],
    };
    let task = tokio::spawn(async move { worker.download_candidate(source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.cancelled.store(true, Ordering::Release);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(*fake.deleted.lock().unwrap(), vec![1009]);
    assert!(!manager.root.join("candidate.db").exists());
}

#[test]
fn age_output_limit_wrong_password_and_existing_output_are_safe() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("backup.age");
    let dst = dir.path().join("plain.db");
    let mut recipient = age::scrypt::Recipient::new(age::secrecy::SecretString::from(
        "test-password".to_string(),
    ));
    recipient.set_work_factor(8);
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .unwrap();
    let mut output = encryptor
        .wrap_output(std::fs::File::create(&src).unwrap())
        .unwrap();
    output.write_all(&[42u8; 1024]).unwrap();
    output.finish().unwrap();
    assert!(crate::backup::decrypt_file_limited(&src, &dst, "wrong".into(), 4096).is_err());
    assert!(!dst.exists());
    assert!(crate::backup::decrypt_file_limited(&src, &dst, "test-password".into(), 512).is_err());
    assert!(!dst.exists());
    crate::backup::decrypt_file_limited(&src, &dst, "test-password".into(), 1024).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), vec![42u8; 1024]);
    assert!(crate::backup::decrypt_file_limited(&src, &dst, "test-password".into(), 1024).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), vec![42u8; 1024]);
}

#[tokio::test]
async fn prepare_is_single_flight_and_cancel_releases_backup_mutex() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = fixture(dir.path()).await;
    let target = state
        .vfs
        .db
        .ensure_storage_target("http://127.0.0.1:1", "test", "-100123")
        .await
        .unwrap();
    let fake = Arc::new(FakeTelegram {
        files: std::collections::HashMap::from([(9, ("test.db".into(), vec![], 100))]),
        stall: true,
        ..Default::default()
    });
    state.vfs.tg = Arc::new(TgRegistry::with_client(target.id, fake));
    let record = TelegramBackupRecord {
        name: "test.db".into(),
        messages: vec![crate::db::StoredMessage {
            storage_target_id: target.id,
            message_id: 9,
        }],
    };
    state
        .vfs
        .db
        .setting_set(
            "extra.backup_msg_groups",
            &serde_json::to_string(&vec![record.clone()]).unwrap(),
        )
        .await
        .unwrap();
    let req = || PrepareRequest {
        backup_id: Some(history_id(&record)),
        messages: None,
        channel: None,
        password: None,
        acknowledge_forward: true,
    };
    // 其他备份测试也用全局互斥，此测试等待它们结束后再开始。
    drop(BACKUP_LOCK.lock().await);
    let first = state.restore.prepare(state.clone(), req()).await.unwrap();
    assert!(BACKUP_LOCK.try_lock().is_err());
    assert!(state.restore.prepare(state.clone(), req()).await.is_err());
    state
        .restore
        .cancel(first.id.as_deref().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.restore.status().await.phase == "preparing" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.restore.status().await.phase, "cancelled");
    assert!(state.restore.inner.lock().await.backup_guard.is_none());
    state.vfs.db.close().await;
}

#[tokio::test]
async fn confirmation_requires_second_factor_and_preserves_consumed_recovery_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = fixture(dir.path()).await;
    let setup = state.auth.begin_totp_setup();
    let code = crate::totp::code_now(&setup.secret, now_ts());
    let codes = state.auth.enable_totp(&code, &state.vfs.db).await.unwrap();
    let id = ready(&state).await;
    let (status, _) = request(
        &state,
        "POST",
        "confirm",
        serde_json::json!({"id": id,"password":"current-password","confirmation":"恢复"}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(&state, "POST", "confirm", serde_json::json!({"id": id,"password":"current-password","totp":codes[0],"confirmation":"恢复"}), true).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let old_codes = state
        .vfs
        .db
        .setting_get(crate::auth::TOTP_RECOVERY_KEY)
        .await
        .unwrap();
    let candidate = Db::open(state.restore.root.join("candidate.db").to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        candidate
            .setting_get(crate::auth::TOTP_RECOVERY_KEY)
            .await
            .unwrap(),
        old_codes
    );
    assert_eq!(
        state.auth.security_status().recovery_remaining,
        codes.len() - 1
    );
    candidate.close().await;
    state.vfs.db.close().await;
}

#[test]
fn links_are_identifiers_not_fetch_urls() {
    assert_eq!(
        parse_messages("https://t.me/c/123/90, https://t.me/c/123/7", None).unwrap(),
        ("-100123".into(), vec![90, 7])
    );
    assert_eq!(
        parse_messages("t.me/MyChannel/8", None).unwrap(),
        ("@mychannel".into(), vec![8])
    );
    assert_eq!(
        parse_messages("9 2", Some("-100123")).unwrap().1,
        vec![9, 2]
    );
    for (input, channel) in [
        ("https://evil.test/a/1", None),
        ("https://t.me/c/123/1 https://t.me/c/124/2", None),
        ("1 1", Some("-100123")),
        ("1", None),
        ("0", Some("-100123")),
        ("https://t.me/c/123/2?url=http://x", None),
    ] {
        assert!(parse_messages(input, channel).is_err(), "{input}");
    }
    assert!(
        parse_messages(
            &(1..=129)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" "),
            Some("-100123")
        )
        .is_err()
    );
}
#[test]
fn parts_must_be_complete_unique_and_same_backup() {
    let parts =
        ["a.db.part0002-of-0002", "a.db.part0001-of-0002"].map(|name| parse_part(name).unwrap());
    validate_parts(&parts).unwrap();
    assert!(validate_parts(&parts[..1]).is_err());
    assert!(validate_parts(&[parts[0].clone(), parts[0].clone()]).is_err());
    assert!(
        validate_parts(&[
            parts[0].clone(),
            parse_part("b.db.part0001-of-0002").unwrap()
        ])
        .is_err()
    );
    for name in [
        "../x.db",
        "x.db.part0000-of-0002",
        "x.txt",
        "x.db.part1-of-2",
        "x.db.part0001-of-9999",
    ] {
        assert!(parse_part(name).is_err());
    }
}
#[tokio::test]
async fn offline_validation_rejects_corruption_future_migrations_and_custom_schema() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("candidate.db");
    let db = Db::open(path.to_str().unwrap()).await.unwrap();
    db.close().await;
    storage::validate_candidate(&path).await.unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
    let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
    sqlx::query("CREATE TRIGGER malicious AFTER INSERT ON settings BEGIN DELETE FROM nodes; END")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert!(
        storage::validate_candidate(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("触发器")
    );
    let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
    sqlx::query("DROP TRIGGER malicious")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET version=999999 WHERE version=17")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert!(storage::validate_candidate(&path).await.is_err());
    std::fs::write(&path, b"not a sqlite file").unwrap();
    assert!(storage::validate_candidate(&path).await.is_err());
}
#[tokio::test]
async fn offline_validation_rejects_same_named_modified_reading_objects() {
    use sqlx::Connection;
    for (drop_sql, replacement) in [
        (
            "DROP TRIGGER reading_tree_insert",
            "CREATE TRIGGER reading_tree_insert AFTER INSERT ON nodes BEGIN DELETE FROM settings; END",
        ),
        (
            "DROP VIEW reading_files",
            "CREATE VIEW reading_files AS SELECT k AS id, v AS format FROM settings",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.db");
        let db = Db::open(path.to_str().unwrap()).await.unwrap();
        db.setting_set("must_survive", "yes").await.unwrap();
        db.close().await;
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        sqlx::query(drop_sql)
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query(replacement).execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let error = storage::validate_candidate(&path)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("被篡改"), "{error}");
        let mut conn = sqlx::SqliteConnection::connect_with(&opts.read_only(true))
            .await
            .unwrap();
        let value: String = sqlx::query_scalar("SELECT v FROM settings WHERE k='must_survive'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(value, "yes");
        conn.close().await.unwrap();
    }
}

#[test]
fn instance_lock_prevents_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    let first = InstanceLock::acquire(db.to_str().unwrap()).unwrap();
    assert!(InstanceLock::acquire(db.to_str().unwrap()).is_err());
    drop(first);
    InstanceLock::acquire(db.to_str().unwrap()).unwrap();
}

#[tokio::test]
async fn confirm_preserves_security_and_only_next_start_switches_database() {
    let dir = tempfile::tempdir().unwrap();
    let state = fixture(dir.path()).await;
    state
        .vfs
        .db
        .setting_set("fixture", "original")
        .await
        .unwrap();
    let cookie = state.auth.issue_token();
    let id = ready(&state).await;
    let candidate = state.restore.root.join("candidate.db");
    let old = Db::open(candidate.to_str().unwrap()).await.unwrap();
    old.setting_set(crate::auth::TOTP_SECRET_KEY, "old-secret")
        .await
        .unwrap();
    old.setting_set(crate::auth::WEBDAV_PASSWORD_KEY, "old-app-password")
        .await
        .unwrap();
    old.setting_set("extra.backup_s3_secret_key", "old-s3-secret")
        .await
        .unwrap();
    old.close().await;
    // 变更候选之后旧确认令牌不能使用。
    assert!(state.restore.confirm(&state, &id).await.is_err());
    let new_hash = storage::digest(&candidate).await.unwrap();
    state.restore.inner.lock().await.journal.digest = Some(new_hash);
    let (status, _) = request(
        &state,
        "POST",
        "confirm",
        serde_json::json!({"id":id,"password":"wrong","confirmation":"恢复"}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, data) = request(
        &state,
        "POST",
        "confirm",
        serde_json::json!({"id":id,"password":"current-password","confirmation":"恢复"}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{data}");
    assert_eq!(
        state
            .vfs
            .db
            .setting_get("fixture")
            .await
            .unwrap()
            .as_deref(),
        Some("original")
    );
    assert!(state.restore.maintenance());
    assert!(state.vfs.db.background_blocked());
    assert!(state.restore.confirm(&state, &id).await.is_err());
    state.vfs.db.close().await;
    assert!(storage::startup(&state.rt.env).await.unwrap());
    let restored = Db::open(&state.rt.env.db_path).await.unwrap();
    assert_eq!(
        restored.setting_get("fixture").await.unwrap().as_deref(),
        Some("candidate")
    );
    assert_eq!(
        restored
            .setting_get("cfg.username")
            .await
            .unwrap()
            .as_deref(),
        Some("current-admin")
    );
    let hash = restored.setting_get("cfg.password").await.unwrap().unwrap();
    assert!(crate::password::verify(&hash, "current-password"));
    assert!(
        restored
            .setting_get(crate::auth::TOTP_SECRET_KEY)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        restored
            .setting_get(crate::auth::WEBDAV_PASSWORD_KEY)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        restored
            .setting_get("extra.backup_s3_secret_key")
            .await
            .unwrap()
            .as_deref(),
        Some("")
    );
    let auth = Auth::init("current-admin", &hash, None, false, &restored)
        .await
        .unwrap();
    assert!(!auth.verify_token(&cookie));
    restored.close().await;
    assert!(storage::startup(&state.rt.env).await.unwrap());
    assert_eq!(
        storage::load(&state.restore.root).unwrap().status.phase,
        "succeeded"
    );
}

#[tokio::test]
async fn cookie_only_history_and_safe_mode_entry_guards() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let state = fixture(dir.path()).await;
    let basic = base64::engine::general_purpose::STANDARD.encode("current-admin:current-password");
    for route in ["history", "status"] {
        let response = app(&state)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/admin/restore/{route}"))
                    .header("authorization", format!("Basic {basic}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let (status, data) = request(&state, "GET", "history", serde_json::json!({}), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["backups"], serde_json::json!([]));
    state
        .vfs
        .db
        .setting_set("extra.backup_msg_groups", "broken")
        .await
        .unwrap();
    let (_, data) = request(&state, "GET", "history", serde_json::json!({}), true).await;
    assert_eq!(data["backups"][0]["available"], false);
    state.vfs.db.set_background_blocked(true);
    assert!(
        state
            .vfs
            .uploader
            .run_delete_batch(10, crate::engine::upload::QueueTrigger::Manual)
            .await
            .is_err()
    );
    assert!(state.migration.start(1).await.is_err());
    assert!(
        crate::backup::backup_now(&state.vfs, &state.rt)
            .await
            .is_err()
    );
    let response = app(&state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/fs/mkdir")
                .header("content-type", "application/json")
                .body(Body::from("{\"path\":\"/bad\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    state.vfs.db.close().await;
}

// 子进程直接退出，留下已提交但未 checkpoint 的 WAL；不接触真实数据。
#[test]
fn wal_writer_subprocess() {
    let Some(dir) = std::env::var_os("TGDRIVE_TEST_WAL_DIR") else {
        return;
    };
    let cfg = config(Path::new(&dir));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = Db::open(&cfg.db_path).await.unwrap();
        db.setting_set("wal-only", "committed").await.unwrap();
        std::process::exit(0);
    });
}
#[tokio::test]
async fn startup_snapshot_contains_committed_wal_and_failed_config_rolls_back() {
    for invalid_config in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "restore::tests::wal_writer_subprocess",
                "--nocapture",
            ])
            .env("TGDRIVE_TEST_WAL_DIR", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            std::fs::metadata(format!("{}-wal", cfg.db_path))
                .unwrap()
                .len()
                > 0
        );
        let root = storage::root(&cfg.db_path).unwrap();
        let path = candidate(&root).await;
        if invalid_config {
            let db = Db::open(path.to_str().unwrap()).await.unwrap();
            db.setting_set("cfg.proxy_enabled", "invalid")
                .await
                .unwrap();
            db.close().await;
        }
        let mut journal = Journal {
            stage: "pending".into(),
            digest: Some(storage::digest(&path).await.unwrap()),
            ..Default::default()
        };
        journal.status.phase = "restarting".into();
        storage::save(&root, &journal).unwrap();
        let _lock = InstanceLock::acquire(&cfg.db_path).unwrap();
        assert_eq!(storage::startup(&cfg).await.unwrap(), !invalid_config);
        let result = storage::load(&root).unwrap();
        assert_eq!(
            result.status.phase,
            if invalid_config {
                "rolled_back"
            } else {
                "succeeded"
            }
        );
        let rollback = Db::open(result.status.rollback_path.as_deref().unwrap())
            .await
            .unwrap();
        assert_eq!(
            rollback.setting_get("wal-only").await.unwrap().as_deref(),
            Some("committed")
        );
        rollback.close().await;
        let db = Db::open(&cfg.db_path).await.unwrap();
        assert_eq!(
            db.setting_get("wal-only").await.unwrap().is_some(),
            invalid_config
        );
        db.close().await;
    }
}
#[tokio::test]
async fn interrupted_switch_stages_deterministically_rollback_once() {
    for stage in ["switching", "verifying", "rollback"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let root = storage::root(&cfg.db_path).unwrap();
        let rollback_path = root.join("rollback-test.db");
        let original = Db::open(&cfg.db_path).await.unwrap();
        original.setting_set("fixture", "original").await.unwrap();
        original
            .vacuum_into(rollback_path.to_str().unwrap())
            .await
            .unwrap();
        original.close().await;
        let path = candidate(&root).await;
        storage::install_copy(&path, Path::new(&cfg.db_path)).unwrap();
        let mut journal = Journal {
            stage: stage.into(),
            ..Default::default()
        };
        journal.status.rollback_path = Some(rollback_path.to_string_lossy().to_string());
        storage::save(&root, &journal).unwrap();
        assert!(!storage::startup(&cfg).await.unwrap());
        assert_eq!(storage::load(&root).unwrap().status.phase, "rolled_back");
        let db = Db::open(&cfg.db_path).await.unwrap();
        assert_eq!(
            db.setting_get("fixture").await.unwrap().as_deref(),
            Some("original")
        );
        db.setting_set("after-rollback", "keep").await.unwrap();
        db.close().await;
        storage::startup(&cfg).await.unwrap();
        let db = Db::open(&cfg.db_path).await.unwrap();
        assert_eq!(
            db.setting_get("after-rollback").await.unwrap().as_deref(),
            Some("keep")
        );
        db.close().await;
    }
}
