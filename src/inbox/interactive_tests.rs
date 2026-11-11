//! 使用隔离 SQLite 与本地模拟 Bot 验证交互行为，不接触真实 Telegram。

use super::tests::{MockApi, test_vfs};
use super::*;
use crate::db::inbox::InboxRequest;
use crate::extras::{ExtrasPatch, InboxDirectory, InboxTypeDirectories};
use serde_json::json;
use std::sync::atomic::Ordering;

async fn configure(vfs: &Vfs) -> crate::extras::Extras {
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_bot_enabled: Some(true),
            inbox_channel_enabled: Some(true),
            inbox_allowed_chats: Some("42,43".into()),
            inbox_dir: Some("/收件".into()),
            inbox_video_dir: Some("/视频".into()),
            inbox_image_dir: Some("/图片".into()),
            inbox_book_dir: Some("/书籍".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    crate::extras::load(&vfs.db).await.unwrap()
}

async fn runtime(vfs: &Vfs, cfg: &RuntimeCfg) -> Arc<Runtime> {
    let mut cfg = cfg.clone();
    cfg.username = "inbox-test".into();
    cfg.password = "inbox-test-password".into();
    let env = Arc::new(crate::config::Config {
        proxy: cfg.proxy.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
        bot_token: cfg.bot_token.clone(),
        upload_bot_tokens: cfg.upload_bot_tokens.clone(),
        chat_id: cfg.chat_id.clone(),
        api_base: cfg.api_base.clone(),
        chunk_size: cfg.chunk_size,
        upload_concurrency: cfg.upload_concurrency,
        download_concurrency: cfg.download_concurrency,
        db_path: cfg.tmp_dir.join("inbox.db").to_string_lossy().into(),
        tmp_dir: cfg.tmp_dir.clone(),
        local_data_root: cfg.tmp_dir.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        secret: None,
        cookie_secure: false,
        trusted_proxies: Vec::new(),
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: cfg.send_video,
    });
    let auth = Arc::new(
        crate::auth::Auth::init(&cfg.username, &cfg.password, None, false, &vfs.db)
            .await
            .unwrap(),
    );
    let knobs = Arc::new(crate::engine::upload::UploadKnobs::new(
        cfg.chunk_size,
        cfg.tmp_dir.clone(),
        1,
        false,
    ));
    Arc::new(Runtime::new(
        vfs.db.clone(),
        env,
        cfg,
        vfs.tg.clone(),
        knobs,
        auth,
    ))
}

fn client(cfg: &RuntimeCfg) -> reqwest::Client {
    inbox_http_client(&cfg.proxy, Duration::from_secs(10)).unwrap()
}

async fn receive(vfs: &Vfs, cfg: &RuntimeCfg, message: i64, name: &str) -> InboxRequest {
    let update: Update = serde_json::from_value(json!({"update_id": message + 1000,"message": {
        "message_id": message,"chat": {"id": 42,"type": "private"},"from": {"id": 42,"is_bot": false},
        "document": {"file_id": format!("source-{message}"),"file_name": name,"file_size": 12}
    }})).unwrap();
    let ex = crate::extras::load(&vfs.db).await.unwrap();
    assert_eq!(
        handle_update(&client(cfg), vfs, cfg, &ex, &update)
            .await
            .unwrap(),
        0
    );
    vfs.db
        .inbox_source_request(&interactive::bot_id(cfg), 42, message)
        .await
        .unwrap()
        .unwrap()
}

async fn click(
    vfs: &Vfs,
    cfg: &RuntimeCfg,
    request: &InboxRequest,
    action: &str,
    value: Option<i64>,
    user: i64,
) {
    let data = match value {
        Some(value) => format!("ib:{}:{action}:{value}", request.id),
        None => format!("ib:{}:{action}", request.id),
    };
    let update: Update = serde_json::from_value(json!({"update_id": 5000,"callback_query": {
        "id": "callback-test","from": {"id": user},"data": data,
        "message": {"message_id": request.operation_message_id.unwrap(),"chat": {"id": user,"type": "private"}}
    }})).unwrap();
    let ex = crate::extras::load(&vfs.db).await.unwrap();
    handle_update(&client(cfg), vfs, cfg, &ex, &update)
        .await
        .unwrap();
}

fn media_calls(mock: &MockApi) -> usize {
    mock.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| {
            matches!(
                method.as_str(),
                "sendDocument" | "sendVideo" | "sendAudio" | "sendAnimation" | "sendPhoto"
            )
        })
        .count()
}

#[tokio::test]
async fn selecting_directory_and_duplicate_confirmation_store_exactly_one_file() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    let mut ex = configure(&vfs).await;
    let movies = vfs.mkdir_p("/电影").await.unwrap();
    let series = vfs.mkdir_p("/电视剧").await.unwrap();
    ex.inbox_routes
        .iter_mut()
        .find(|route| route.key == "video")
        .unwrap()
        .directories = vec![
        InboxDirectory {
            node_id: Some(movies.id),
            path: "/电影".into(),
            automatic: true,
            missing: false,
        },
        InboxDirectory {
            node_id: Some(series.id),
            path: "/电视剧".into(),
            automatic: false,
            missing: false,
        },
    ];
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_routes: Some(ex.inbox_routes),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request = receive(&vfs, &cfg, 10, "movie.mkv").await;
    assert!(request.selected_dir_id.is_none());
    assert_eq!(media_calls(&mock), 0);
    click(&vfs, &cfg, &request, "d", Some(series.id), 42).await;
    assert_eq!(media_calls(&mock), 0);
    assert!(vfs.resolve("/电视剧/movie.mkv").await.is_err());
    let request = vfs.db.inbox_request(&request.id).await.unwrap();
    click(&vfs, &cfg, &request, "s", None, 42).await;
    click(&vfs, &cfg, &request, "s", None, 42).await;
    let rt = runtime(&vfs, &cfg).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    click(&vfs, &cfg, &request, "s", None, 42).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert_eq!(media_calls(&mock), 1);
    let node = vfs.resolve("/电视剧/movie.mkv").await.unwrap();
    assert!(vfs.resolve("/电影/movie.mkv").await.is_err());
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().node_id,
        Some(node.id)
    );
    assert!(request.id.len() + series.id.to_string().len() + 6 < 64);
    server.abort();
}

#[tokio::test]
async fn rename_reply_belongs_to_its_file_and_preserves_suffix_and_original_classification() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let first = receive(&vfs, &cfg, 10, "README").await;
    let second = receive(&vfs, &cfg, 11, "another.pdf").await;
    click(&vfs, &cfg, &first, "r", None, 42).await;
    let first = vfs.db.inbox_request(&first.id).await.unwrap();
    let prompt = first.rename_prompt_id.unwrap();
    let update: Update = serde_json::from_value(json!({"update_id": 2000,"message": {
        "message_id": 50,"chat": {"id": 42,"type": "private"},"from": {"id": 42},
        "text": "说明.pdf","reply_to_message": {"message_id": prompt}
    }}))
    .unwrap();
    handle_update(
        &client(&cfg),
        &vfs,
        &cfg,
        &crate::extras::load(&vfs.db).await.unwrap(),
        &update,
    )
    .await
    .unwrap();
    let renamed = vfs.db.inbox_request(&first.id).await.unwrap();
    assert_eq!(renamed.name, "说明.pdf");
    assert_eq!(
        vfs.db.inbox_request(&second.id).await.unwrap().name,
        "another.pdf"
    );
    // 同一回复重放不再匹配已结束的改名输入，也不会产生新收件。
    handle_update(
        &client(&cfg),
        &vfs,
        &cfg,
        &crate::extras::load(&vfs.db).await.unwrap(),
        &update,
    )
    .await
    .unwrap();
    click(&vfs, &cfg, &renamed, "s", None, 42).await;
    interactive::process_one(&vfs, runtime(&vfs, &cfg).await.as_ref(), None)
        .await
        .unwrap();
    assert!(vfs.resolve("/收件/说明.pdf").await.is_ok());
    assert!(vfs.resolve("/书籍/说明.pdf").await.is_err());
    let archive = receive(&vfs, &cfg, 12, "backup.tar.gz").await;
    click(&vfs, &cfg, &archive, "r", None, 42).await;
    let archive = vfs.db.inbox_request(&archive.id).await.unwrap();
    let update: Update = serde_json::from_value(json!({"update_id": 2001,"message": {
        "message_id": 51,"chat": {"id": 42,"type": "private"},"from": {"id": 42},
        "text": "数据库备份","reply_to_message": {"message_id": archive.rename_prompt_id.unwrap()}
    }}))
    .unwrap();
    handle_update(
        &client(&cfg),
        &vfs,
        &cfg,
        &crate::extras::load(&vfs.db).await.unwrap(),
        &update,
    )
    .await
    .unwrap();
    assert_eq!(
        vfs.db.inbox_request(&archive.id).await.unwrap().name,
        "数据库备份.tar.gz"
    );
    assert_eq!(media_calls(&mock), 1);
    let archive = vfs.db.inbox_request(&archive.id).await.unwrap();
    click(&vfs, &cfg, &archive, "s", None, 42).await;
    let duplicate = receive(&vfs, &cfg, 13, "数据库备份.tar.gz").await;
    click(&vfs, &cfg, &duplicate, "s", None, 42).await;
    let rt = runtime(&vfs, &cfg).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert!(vfs.resolve("/收件/数据库备份.tar.gz").await.is_ok());
    assert!(vfs.resolve("/收件/数据库备份 (1).tar.gz").await.is_ok());
    assert_eq!(media_calls(&mock), 3);
    server.abort();
}

#[tokio::test]
async fn stranger_cannot_operate_another_users_request_and_cancel_never_copies() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "secret.pdf").await;
    click(&vfs, &cfg, &request, "s", None, 43).await;
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "pending"
    );
    click(&vfs, &cfg, &request, "x", None, 42).await;
    interactive::process_one(&vfs, runtime(&vfs, &cfg).await.as_ref(), None)
        .await
        .unwrap();
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "cancelled"
    );
    assert_eq!(media_calls(&mock), 0);
    assert!(
        !mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "deleteMessage")
    );
    server.abort();
}

#[tokio::test]
async fn file_created_while_copying_is_numbered_without_overwrite() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "same.pdf").await;
    *mock.create_on_copy.lock().unwrap() = Some((
        vfs.db.clone(),
        request.selected_dir_id.unwrap(),
        "same.pdf".into(),
        cfg.storage_target_id.unwrap(),
    ));
    click(&vfs, &cfg, &request, "s", None, 42).await;
    interactive::process_one(&vfs, runtime(&vfs, &cfg).await.as_ref(), None)
        .await
        .unwrap();
    let old = vfs.resolve("/书籍/same.pdf").await.unwrap();
    assert_eq!(old.size, 5);
    assert_eq!(
        vfs.db.chunks_of(old.id).await.unwrap()[0].tg_file_id,
        "existing-file"
    );
    let stored = vfs.db.inbox_request(&request.id).await.unwrap();
    assert_eq!(stored.result_path.as_deref(), Some("/书籍/same (1).pdf"));
    assert!(vfs.resolve("/书籍/same (1).pdf").await.is_ok());
    server.abort();
}

#[tokio::test]
async fn directory_move_requires_confirmation_and_retry_reuses_the_known_copy() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    *mock.move_on_copy.lock().unwrap() = Some((
        vfs.db.clone(),
        request.selected_dir_id.unwrap(),
        "新书籍".into(),
    ));
    click(&vfs, &cfg, &request, "s", None, 42).await;
    let rt = runtime(&vfs, &cfg).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    let failed = vfs.db.inbox_request(&request.id).await.unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.copied_message_id.is_some());
    assert!(vfs.resolve("/新书籍/book.pdf").await.is_err());
    click(&vfs, &cfg, &failed, "s", None, 42).await;
    let refreshed = vfs.db.inbox_request(&request.id).await.unwrap();
    assert_eq!(refreshed.selected_path.as_deref(), Some("/新书籍"));
    assert_eq!(refreshed.status, "failed");
    click(&vfs, &cfg, &refreshed, "s", None, 42).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert!(vfs.resolve("/新书籍/book.pdf").await.is_ok());
    assert_eq!(media_calls(&mock), 1);
    assert!(vfs.resolve("/书籍").await.is_err());
    server.abort();
}

#[tokio::test]
async fn interrupted_copy_is_uncertain_after_restart_and_is_not_sent_again() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    click(&vfs, &cfg, &request, "s", None, 42).await;
    mock.pause_copy.store(true, Ordering::SeqCst);
    let rt = runtime(&vfs, &cfg).await;
    let running_vfs = vfs.clone();
    let running_rt = rt.clone();
    let running =
        tokio::spawn(
            async move { interactive::process_one(&running_vfs, &running_rt, None).await },
        );
    tokio::time::timeout(Duration::from_secs(5), mock.copy_started.notified())
        .await
        .unwrap();
    let additional = tokio::time::timeout(
        Duration::from_secs(2),
        receive(&vfs, &cfg, 11, "second.pdf"),
    )
    .await
    .unwrap();
    assert_eq!(additional.status, "pending");
    assert!(!vfs.db.inbox_cancel_request(&request.id).await.unwrap());
    running.abort();
    let _ = running.await;
    mock.copy_release.notify_one();
    vfs.db.recover_inbox_requests().await.unwrap();
    let uncertain = vfs.db.inbox_request(&request.id).await.unwrap();
    assert_eq!(uncertain.status, "uncertain");
    assert!(!uncertain.view().can_retry);
    assert!(
        interactive::admin_retry(&vfs, &cfg, &request.id)
            .await
            .is_err()
    );
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert_eq!(media_calls(&mock), 1);
    assert!(vfs.resolve("/书籍/book.pdf").await.is_err());
    server.abort();
}

#[tokio::test]
async fn server_error_is_uncertain_and_expiry_never_automatically_stores() {
    let (vfs, cfg, mock, pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    *mock.copy_failure.lock().unwrap() =
        Some(json!({"ok": false,"error_code": 500,"description": "模拟上游结果丢失"}));
    click(&vfs, &cfg, &request, "s", None, 42).await;
    let rt = runtime(&vfs, &cfg).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "uncertain"
    );
    let pending = receive(&vfs, &cfg, 11, "expired.pdf").await;
    sqlx::query("UPDATE inbox_requests SET expires_at=? WHERE id=?")
        .bind(crate::db::now_ts() - 1)
        .bind(&pending.id)
        .execute(&pool)
        .await
        .unwrap();
    vfs.db
        .maintain_inbox_requests("100", &[42, 43])
        .await
        .unwrap();
    assert_eq!(
        vfs.db.inbox_request(&pending.id).await.unwrap().status,
        "expired"
    );
    // 结果未知的请求不会被普通等待期限丢弃，必须人工结束。
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "uncertain"
    );
    assert_eq!(media_calls(&mock), 1);
    assert!(vfs.resolve("/书籍/expired.pdf").await.is_err());
    server.abort();
}

#[tokio::test]
async fn normal_restart_keeps_choices_and_disabled_inbox_pauses_queued_work() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    click(&vfs, &cfg, &request, "s", None, 42).await;
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_bot_enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    vfs.db.recover_inbox_requests().await.unwrap();
    let rt = runtime(&vfs, &cfg).await;
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert_eq!(media_calls(&mock), 0);
    assert_eq!(
        vfs.db
            .inbox_request(&request.id)
            .await
            .unwrap()
            .selected_path,
        request.selected_path
    );
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_bot_enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    interactive::process_one(&vfs, &rt, None).await.unwrap();
    assert_eq!(media_calls(&mock), 1);
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "stored"
    );
    server.abort();
}

#[tokio::test]
async fn bot_switch_isolated_offsets_and_invalidates_old_pending_requests() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "old.pdf").await;
    assert_eq!(vfs.db.inbox_offset("100").await.unwrap(), 0);
    vfs.db.set_inbox_offset("100", 1000).await.unwrap();
    assert_eq!(vfs.db.inbox_offset("200").await.unwrap(), 0);
    vfs.db.set_inbox_offset("200", 5).await.unwrap();
    assert_eq!(vfs.db.inbox_offset("100").await.unwrap(), 1000);
    vfs.db
        .maintain_inbox_requests("200", &[42, 43])
        .await
        .unwrap();
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "invalidated"
    );
    assert_eq!(media_calls(&mock), 0);
    server.abort();
}

#[tokio::test]
async fn deleted_directory_is_not_recreated_and_configuration_error_is_atomic() {
    let (vfs, cfg, _mock, _pool, server, _dir) = test_vfs().await;
    let ex = configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    vfs.delete("/书籍").await.unwrap();
    click(&vfs, &cfg, &request, "s", None, 42).await;
    let refreshed = crate::extras::load(&vfs.db).await.unwrap();
    assert!(refreshed.inbox_directories_for("book.pdf")[0].missing);
    assert!(vfs.resolve("/书籍").await.is_err());
    assert!(
        vfs.db
            .inbox_request(&request.id)
            .await
            .unwrap()
            .selected_dir_id
            .is_none()
    );
    let old = vfs.db.setting_get("extra.inbox_routes").await.unwrap();
    let mut invalid = ex.inbox_routes;
    let duplicate = invalid[0].directories[0].clone();
    invalid[0].directories.push(duplicate);
    let result = crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_routes: Some(invalid),
            inbox_bot_enabled: Some(false),
            ..Default::default()
        },
    )
    .await;
    assert!(result.is_err());
    assert!(
        crate::extras::load(&vfs.db)
            .await
            .unwrap()
            .inbox_bot_enabled
    );
    assert_eq!(vfs.db.setting_get("extra.inbox_routes").await.unwrap(), old);
    server.abort();
}

#[tokio::test]
async fn old_configuration_migrates_and_menu_paginates_without_exposing_upstream_ids() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    vfs.db
        .settings_apply(
            &[
                ("extra.inbox_enabled".into(), "1".into()),
                ("extra.inbox_allowed_chats".into(), "42".into()),
                ("extra.inbox_book_dir".into(), "/旧书籍".into()),
            ],
            &[],
        )
        .await
        .unwrap();
    let ex = crate::extras::inbox_dirs::ensure_bound(&vfs.db)
        .await
        .unwrap();
    assert_eq!(ex.inbox_directories_for("book.pdf")[0].path, "/旧书籍");
    assert!(ex.inbox_directories_for("book.pdf")[0].automatic);
    let mut routes = ex.inbox_routes;
    let mut directories = Vec::new();
    for index in 0..20 {
        let path = format!("/目录{index}");
        let node = vfs.mkdir_p(&path).await.unwrap();
        directories.push(InboxDirectory {
            node_id: Some(node.id),
            path,
            automatic: index == 0,
            missing: false,
        });
    }
    *routes.iter_mut().find(|route| route.key == "book").unwrap() = InboxTypeDirectories {
        key: "book".into(),
        directories,
    };
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_routes: Some(routes),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    click(&vfs, &cfg, &request, "p", Some(1), 42).await;
    let bodies = mock.bodies.lock().unwrap();
    let body = &bodies
        .iter()
        .rev()
        .find(|(method, _)| method == "editMessageText")
        .unwrap()
        .1;
    let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
    let directory_buttons = rows
        .iter()
        .flat_map(|row| row.as_array().unwrap())
        .filter(|button| {
            button["callback_data"]
                .as_str()
                .unwrap_or("")
                .contains(":d:")
        })
        .count();
    assert_eq!(directory_buttons, 6);
    assert!(body["text"].as_str().unwrap().contains("第 2/4 页"));
    let view = serde_json::to_string(&request.view()).unwrap();
    assert!(!view.contains("source-10"));
    assert!(!view.contains("100:main"));
    server.abort();
}

#[tokio::test]
async fn per_user_quota_keeps_duplicate_updates_idempotent_and_history_cleanup_keeps_files() {
    let (vfs, cfg, _mock, pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "first.pdf").await;
    for message in 11..60 {
        vfs.db
            .create_inbox_request(crate::db::inbox::NewInboxRequest {
                origin_target_id: cfg.storage_target_id,
                bot_id: if message % 2 == 0 { "100" } else { "200" },
                chat_id: 42,
                message_id: message,
                media_kind: "document",
                name: "quota.pdf",
                suffix: ".pdf",
                file_id: "quota-file",
                size: 12,
                thumbnail: None,
                selected_dir_id: None,
                selected_path: None,
            })
            .await
            .unwrap();
    }
    let duplicate = receive(&vfs, &cfg, 10, "first.pdf").await;
    assert_eq!(duplicate.id, request.id);
    let rejected = vfs
        .db
        .create_inbox_request(crate::db::inbox::NewInboxRequest {
            origin_target_id: cfg.storage_target_id,
            bot_id: "300",
            chat_id: 42,
            message_id: 60,
            media_kind: "document",
            name: "extra.pdf",
            suffix: ".pdf",
            file_id: "extra-file",
            size: 12,
            thumbnail: None,
            selected_dir_id: None,
            selected_path: None,
        })
        .await;
    assert!(matches!(rejected, Err(AppError::Conflict(_))));
    click(&vfs, &cfg, &request, "s", None, 42).await;
    interactive::process_one(&vfs, runtime(&vfs, &cfg).await.as_ref(), None)
        .await
        .unwrap();
    sqlx::query("UPDATE inbox_requests SET ended_at=? WHERE id=?")
        .bind(crate::db::now_ts() - crate::db::inbox::INBOX_HISTORY_TTL - 1)
        .bind(&request.id)
        .execute(&pool)
        .await
        .unwrap();
    vfs.db
        .maintain_inbox_requests("100", &[42, 43])
        .await
        .unwrap();
    assert!(vfs.db.inbox_request(&request.id).await.is_err());
    assert!(vfs.resolve("/书籍/first.pdf").await.is_ok());
    server.abort();
}

#[tokio::test]
async fn video_round_trip_selects_highest_quality_on_receipt_and_storage_response() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    let update: Update = serde_json::from_value(json!({"update_id": 1010,"message": {
        "message_id": 10,"chat": {"id": 42,"type": "private"},"from": {"id": 42},
        "video": {"file_id": "source-720","file_name": "film.mp4","file_size": 12,"width": 1280,"height": 720,
            "qualities": [
                {"file_id": "source-2160","file_size": 60,"width": 3840,"height": 2160},
                {"file_id": "source-1080","file_size": 30,"width": 1920,"height": 1080}
            ]}
    }})).unwrap();
    handle_update(
        &client(&cfg),
        &vfs,
        &cfg,
        &crate::extras::load(&vfs.db).await.unwrap(),
        &update,
    )
    .await
    .unwrap();
    let request = vfs
        .db
        .inbox_source_request("100", 42, 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.file_id, "source-2160");
    assert_eq!(media_calls(&mock), 0);
    click(&vfs, &cfg, &request, "s", None, 42).await;
    interactive::process_one(&vfs, runtime(&vfs, &cfg).await.as_ref(), None)
        .await
        .unwrap();
    let node = vfs.resolve("/视频/film.mp4").await.unwrap();
    let chunk = vfs.db.chunks_of(node.id).await.unwrap().remove(0);
    assert_eq!(node.size, 60);
    assert_eq!(chunk.tg_file_id, "copied-video-2160");
    assert_eq!(
        chunk.tg_thumbnail_file_id.as_deref(),
        Some("copied-thumbnail")
    );
    let bodies = mock.bodies.lock().unwrap();
    let sent = &bodies
        .iter()
        .find(|(method, _)| method == "sendVideo")
        .unwrap()
        .1;
    assert_eq!(sent["video"], "source-2160");
    assert_eq!(media_calls(&mock), 1);
    server.abort();
}

#[tokio::test]
async fn rename_wait_blocks_confirmation_and_cancellation_preserves_previous_name() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    let ex = configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "book.pdf").await;
    vfs.db
        .inbox_set_rename_prompt(&request.id, 800)
        .await
        .unwrap();
    vfs.db
        .inbox_set_name(&request.id, 800, "已改名.pdf")
        .await
        .unwrap();
    let before_rename = vfs.db.inbox_request(&request.id).await.unwrap();
    click(&vfs, &cfg, &before_rename, "r", None, 42).await;
    let waiting = vfs.db.inbox_request(&request.id).await.unwrap();
    click(&vfs, &cfg, &waiting, "s", None, 42).await;
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().status,
        "renaming"
    );
    assert!(
        !vfs.db
            .inbox_queue_request(
                &request.id,
                before_rename.revision,
                ex.inbox_routes_revision
            )
            .await
            .unwrap()
    );
    let waiting = vfs.db.inbox_request(&request.id).await.unwrap();
    click(&vfs, &cfg, &waiting, "n", None, 42).await;
    let cancelled_rename = vfs.db.inbox_request(&request.id).await.unwrap();
    assert_eq!(cancelled_rename.status, "pending");
    assert_eq!(cancelled_rename.name, "已改名.pdf");
    assert!(cancelled_rename.rename_prompt_id.is_none());
    assert_eq!(media_calls(&mock), 0);
    server.abort();
}

#[tokio::test]
async fn custom_multi_part_suffix_stays_fixed_when_rules_change_during_rename() {
    let (vfs, cfg, _mock, _pool, server, _dir) = test_vfs().await;
    configure(&vfs).await;
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_rules: Some(vec![crate::extras::InboxRouteRule {
                name: "归档".into(),
                extensions: vec!["part.tar.gz".into()],
                dir: "/归档".into(),
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request = receive(&vfs, &cfg, 10, "data.part.tar.gz").await;
    assert_eq!(request.original_suffix, ".part.tar.gz");
    click(&vfs, &cfg, &request, "r", None, 42).await;
    let request = vfs.db.inbox_request(&request.id).await.unwrap();
    crate::extras::save(
        &vfs.db,
        &ExtrasPatch {
            inbox_rules: Some(Vec::new()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let update: Update = serde_json::from_value(json!({"update_id": 2001,"message": {
        "message_id": 51,"chat": {"id": 42,"type": "private"},"from": {"id": 42},
        "text": "新备份.part.tar.gz","reply_to_message": {"message_id": request.rename_prompt_id.unwrap()}
    }})).unwrap();
    handle_update(
        &client(&cfg),
        &vfs,
        &cfg,
        &crate::extras::load(&vfs.db).await.unwrap(),
        &update,
    )
    .await
    .unwrap();
    assert_eq!(
        vfs.db.inbox_request(&request.id).await.unwrap().name,
        "新备份.part.tar.gz"
    );
    server.abort();
}

#[tokio::test]
async fn replaced_bot_uses_origin_credentials_for_an_end_notice_without_buttons() {
    let (vfs, cfg, mock, _pool, server, _dir) = test_vfs().await;
    let ex = configure(&vfs).await;
    let request = receive(&vfs, &cfg, 10, "old.pdf").await;
    let mut new_cfg = cfg.clone();
    new_cfg.bot_token = "200:new".into();
    vfs.db
        .maintain_inbox_requests("200", &[42, 43])
        .await
        .unwrap();
    let old = vfs.db.inbox_retired_notices("200").await.unwrap().remove(0);
    assert_eq!(old.id, request.id);
    interactive::deliver_retired_notice(&client(&new_cfg), &vfs, &new_cfg, &old, &ex)
        .await
        .unwrap();
    assert_eq!(
        mock.paths.lock().unwrap().last().unwrap(),
        "bot100:main/editMessageText"
    );
    let bodies = mock.bodies.lock().unwrap();
    let notice = &bodies.last().unwrap().1;
    assert_eq!(
        notice["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(notice["text"].as_str().unwrap().contains("Bot 已更换"));
    assert!(
        vfs.db
            .inbox_retired_notices("200")
            .await
            .unwrap()
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn total_quota_is_atomic_and_duplicate_receipt_survives_a_full_instance() {
    let (vfs, cfg, _mock, _pool, server, _dir) = test_vfs().await;
    let mut first_id = String::new();
    for index in 0..500 {
        let request = vfs
            .db
            .create_inbox_request(crate::db::inbox::NewInboxRequest {
                origin_target_id: cfg.storage_target_id,
                bot_id: "100",
                chat_id: 42 + index / 50,
                message_id: index,
                media_kind: "document",
                name: "quota.pdf",
                suffix: ".pdf",
                file_id: "quota-file",
                size: 12,
                thumbnail: None,
                selected_dir_id: None,
                selected_path: None,
            })
            .await
            .unwrap();
        if index == 0 {
            first_id = request.id;
        }
    }
    let duplicate = vfs
        .db
        .create_inbox_request(crate::db::inbox::NewInboxRequest {
            origin_target_id: cfg.storage_target_id,
            bot_id: "100",
            chat_id: 42,
            message_id: 0,
            media_kind: "document",
            name: "quota.pdf",
            suffix: ".pdf",
            file_id: "quota-file",
            size: 12,
            thumbnail: None,
            selected_dir_id: None,
            selected_path: None,
        })
        .await
        .unwrap();
    assert_eq!(duplicate.id, first_id);
    let rejected = vfs
        .db
        .create_inbox_request(crate::db::inbox::NewInboxRequest {
            origin_target_id: cfg.storage_target_id,
            bot_id: "200",
            chat_id: 999,
            message_id: 501,
            media_kind: "document",
            name: "quota.pdf",
            suffix: ".pdf",
            file_id: "quota-file",
            size: 12,
            thumbnail: None,
            selected_dir_id: None,
            selected_path: None,
        })
        .await;
    assert!(matches!(rejected, Err(AppError::Conflict(_))));
    server.abort();
}

#[tokio::test]
async fn concurrent_settings_commit_rejects_stale_revision_without_partial_fields() {
    let (vfs, _cfg, _mock, _pool, server, _dir) = test_vfs().await;
    let ex = configure(&vfs).await;
    let mut reversed = ex.inbox_routes.clone();
    reversed.reverse();
    let left = vec![
        (
            "extra.inbox_routes".into(),
            serde_json::to_string(&reversed).unwrap(),
        ),
        ("extra.trash_retention_days".into(), "11".into()),
    ];
    let mut rotated = ex.inbox_routes;
    rotated.rotate_left(1);
    let right = vec![
        (
            "extra.inbox_routes".into(),
            serde_json::to_string(&rotated).unwrap(),
        ),
        ("extra.trash_retention_days".into(), "22".into()),
    ];
    let (a, b) = tokio::join!(
        vfs.db
            .apply_inbox_settings(&left, &[], Some(ex.inbox_routes_revision)),
        vfs.db
            .apply_inbox_settings(&right, &[], Some(ex.inbox_routes_revision)),
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let winning = if a.is_ok() { &left } else { &right };
    for (key, value) in winning {
        assert_eq!(vfs.db.setting_get(key).await.unwrap().as_ref(), Some(value));
    }
    server.abort();
}

#[tokio::test]
async fn same_update_id_in_different_bots_has_independent_failure_attempts() {
    let (vfs, _cfg, _mock, _pool, server, _dir) = test_vfs().await;
    let update: Update = serde_json::from_value(json!({"update_id": 31})).unwrap();
    assert_eq!(
        remember_update_failure(&vfs.db, "100", &update, "旧 Bot 失败")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        remember_update_failure(&vfs.db, "100", &update, "旧 Bot 失败")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        remember_update_failure(&vfs.db, "200", &update, "新 Bot 失败")
            .await
            .unwrap(),
        1
    );
    assert!(
        vfs.db
            .setting_get("extra.inbox_failure.100.31")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        vfs.db
            .setting_get("extra.inbox_failure.200.31")
            .await
            .unwrap()
            .is_some()
    );
    server.abort();
}
