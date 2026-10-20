use super::*;
use crate::db::apps::CoverLimits;

fn named_manifest(id: &str) -> Manifest {
    let mut m = manifest();
    m.id = id.into();
    m.name = format!("测试-{id}");
    m
}

async fn raw(state: &AppState) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite://{}", state.rt.env.db_path))
        .await
        .unwrap()
}

async fn db_install(state: &AppState, m: &Manifest, replace_legacy: bool) -> AppResult<()> {
    let replacement = state.vfs.db.app_replacement_preview(&m.id).await?;
    db_install_reviewed(
        state,
        m,
        replace_legacy,
        replacement.replacement_revision.as_deref(),
    )
    .await
}

async fn db_install_reviewed(
    state: &AppState,
    m: &Manifest,
    replace_legacy: bool,
    replacement_revision: Option<&str>,
) -> AppResult<()> {
    let p = AppPackage::parse(archive(m)).unwrap();
    let metadata = serde_json::to_string(m).unwrap();
    let previous = state.vfs.db.app(&m.id).await?;
    state
        .vfs
        .db
        .install_app(AppInstall {
            id: &m.id,
            manifest: &metadata,
            version: &m.version,
            digest: &p.digest,
            package: &p.archive,
            source: "import",
            expected_revision: previous.as_ref().map(|app| app.revision.as_str()),
            replace_legacy,
            replacement_revision,
        })
        .await
}

async fn import_request(
    state: &AppState,
    package: &AppPackage,
    route: &str,
) -> (StatusCode, Value) {
    let auth = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/apps/{route}"))
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(package.archive.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn import_replacement_rejects_legacy_scope_changes_after_inspect() {
    for change in [
        "install_comics",
        "reinstall_books",
        "uninstalled_residue",
        "initially_empty",
        "remove_books",
    ] {
        let (state, _, _dir) = fixture().await;
        if change != "initially_empty" {
            add_app(&state, &named_manifest("books"), None).await;
        }
        let reader = AppPackage::parse(archive(&named_manifest("reader"))).unwrap();
        let (status, review) = import_request(&state, &reader, "inspect").await;
        assert_eq!(status, StatusCode::OK);
        let token = review["replacement_revision"].as_str().unwrap();
        match change {
            "install_comics" | "initially_empty" => {
                add_app(&state, &named_manifest("comics"), None).await;
            }
            "reinstall_books" => {
                state.vfs.db.uninstall_app("books", true).await.unwrap();
                add_app(&state, &named_manifest("books"), None).await;
            }
            "uninstalled_residue" => {
                sqlx::query("INSERT INTO app_settings(app_id, settings) VALUES('comics', '{}')")
                    .execute(&raw(&state).await)
                    .await
                    .unwrap();
            }
            "remove_books" => {
                state.vfs.db.uninstall_app("books", true).await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = state.vfs.db.apps().await.unwrap();
        let (status, error) = import_request(
            &state,
            &reader,
            &format!(
                "install?digest={}&replace_legacy=true&replacement_revision={token}",
                reader.digest
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{change}: {error}");
        assert!(state.vfs.db.app("reader").await.unwrap().is_none());
        let after = state.vfs.db.apps().await.unwrap();
        assert_eq!(
            before
                .iter()
                .map(|r| (&r.id, &r.revision))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|r| (&r.id, &r.revision))
                .collect::<Vec<_>>()
        );
        assert!(jobs(&raw(&state).await).await.is_empty());
        if change == "initially_empty" {
            // 未勾替换确认时，即使旧预览范围为空也不能删掉后来出现的应用。
            assert_eq!(
                import_request(
                    &state,
                    &reader,
                    &format!(
                        "install?digest={}&replacement_revision={token}",
                        reader.digest
                    )
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        }
        let (_, refreshed) = import_request(&state, &reader, "inspect").await;
        assert_eq!(
            import_request(
                &state,
                &reader,
                &format!(
                    "install?digest={}&replace_legacy=true&replacement_revision={}",
                    reader.digest,
                    refreshed["replacement_revision"].as_str().unwrap()
                )
            )
            .await
            .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn replacement_fingerprint_is_stable_for_progress_and_survives_restart() {
    let (state, media, _dir) = fixture().await;
    let books = add_app(&state, &named_manifest("books"), None).await;
    let initial = state
        .vfs
        .db
        .app_data_write("books", &books.revision, "progress", Some("1"), None)
        .await
        .unwrap()
        .unwrap();
    let reviewed = state
        .vfs
        .db
        .app_replacement_preview("reader")
        .await
        .unwrap();
    state
        .vfs
        .db
        .app_data_write(
            "books",
            &books.revision,
            "progress",
            Some("2"),
            Some(&initial.revision),
        )
        .await
        .unwrap();
    save_settings(&state, &books, json!({"source_dir":"/更新目录"}))
        .await
        .unwrap();
    let updated = state
        .vfs
        .db
        .app_replacement_preview("reader")
        .await
        .unwrap();
    assert_eq!(reviewed.replacement_revision, updated.replacement_revision);
    let restarted = state_at(std::path::Path::new(&state.rt.env.db_path), media).await;
    assert_eq!(
        restarted
            .vfs
            .db
            .app_replacement_preview("reader")
            .await
            .unwrap()
            .replacement_revision,
        reviewed.replacement_revision
    );
    // 有旧数据时，勾选替换也不能跳过预览指纹。
    assert!(matches!(
        db_install_reviewed(&state, &named_manifest("reader"), true, None).await,
        Err(AppError::Conflict(_))
    ));
    db_install_reviewed(
        &state,
        &named_manifest("reader"),
        true,
        reviewed.replacement_revision.as_deref(),
    )
    .await
    .unwrap();
}

async fn jobs(pool: &sqlx::SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT app_id FROM app_cleanup_jobs ORDER BY app_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn put_cover(state: &AppState, id: &str, key: &str) {
    state
        .vfs
        .db
        .cover_put(id, key, "image/png", b"cover", None, CoverLimits::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_includes_installed_and_uninstalled_private_state_only_for_reader() {
    let (state, _, _dir) = fixture().await;
    let books = add_app(&state, &named_manifest("books"), None).await;
    let comics = add_app(&state, &named_manifest("comics"), None).await;
    save_settings(&state, &comics, json!({"source_dir":"/漫画"}))
        .await
        .unwrap();
    state.vfs.db.uninstall_app("comics", false).await.unwrap();
    let package = AppPackage::parse(archive(&named_manifest("reader"))).unwrap();
    let preview = preview(&state, &package).await.unwrap();
    assert_eq!(
        preview["replacements"],
        json!([
            {"id":"books", "name":books.manifest.name, "installed":true, "has_data":true},
            {"id":"comics", "name":"漫画", "installed":false, "has_data":true},
        ])
    );
    for field in ["manifest", "digest", "existing", "added_permissions"] {
        assert!(preview.get(field).is_some(), "缺少原预览字段 {field}");
    }
    let ordinary = AppPackage::parse(archive(&manifest())).unwrap();
    assert_eq!(
        super::super::preview(&state, &ordinary).await.unwrap()["replacements"],
        json!([])
    );
}

#[tokio::test]
async fn import_route_requires_explicit_replacement_and_invalidates_old_sessions() {
    let (state, _, _dir) = fixture().await;
    let books = add_app(&state, &named_manifest("books"), None).await;
    let session = launch_app(&state, "books").await;
    put_cover(&state, "books", "old").await;
    let reader = AppPackage::parse(archive(&named_manifest("reader"))).unwrap();
    let replacement_revision = preview(&state, &reader).await.unwrap()["replacement_revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    for (suffix, expected) in [
        ("", StatusCode::CONFLICT),
        ("&replace_legacy=false", StatusCode::CONFLICT),
        ("&replace_legacy=true", StatusCode::CONFLICT),
    ] {
        let response = router(&state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/apps/install?digest={}{suffix}",
                        reader.digest
                    ))
                    .header(header::AUTHORIZATION, format!("Basic {auth}"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(reader.archive.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::CONFLICT {
            assert_eq!(
                installed(&state, "books").await.unwrap().revision,
                books.revision
            );
            assert!(state.vfs.db.app("reader").await.unwrap().is_none());
        }
    }
    let response = router(&state).oneshot(Request::builder()
        .method("POST")
        .uri(format!("/api/apps/install?digest={}&replace_legacy=true&replacement_revision={replacement_revision}", reader.digest))
        .header(header::AUTHORIZATION, format!("Basic {auth}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(reader.archive.clone())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.vfs.db.app("books").await.unwrap().is_none());
    assert!(state.vfs.db.app("reader").await.unwrap().is_some());
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 0);
    assert!(!cache_root(&state).join("books").exists());
    let old = session["session"].as_str().unwrap();
    assert_ne!(
        rpc_call(&state, "books", old, "app.ping", json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let new_books = add_app(&state, &named_manifest("books"), None).await;
    assert_ne!(new_books.revision, books.revision);
    assert_eq!(
        rpc_call(&state, "books", old, "app.ping", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    // 连验证时已通过的迟到写入也必须重新检查旧修订，不得删除新应用的同名封面。
    put_cover(&state, "books", "same").await;
    assert!(
        covers::invoke(&state, &books, "covers.delete", json!({"keys":["same"]}))
            .await
            .is_err()
    );
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 1);
}

#[tokio::test]
async fn transaction_deletes_all_legacy_state_including_uninstalled_residue() {
    let (state, _, _dir) = fixture().await;
    let first = add_app(&state, &named_manifest("books"), None).await;
    let mut next = first.manifest.clone();
    next.version = "1.1.0".into();
    let books = add_app(&state, &next, Some(first.revision)).await;
    save_settings(&state, &books, json!({"source_dir":"/旧书"}))
        .await
        .unwrap();
    state
        .vfs
        .db
        .app_data_write("books", &books.revision, "progress", Some("1"), None)
        .await
        .unwrap();
    let comics = add_app(&state, &named_manifest("comics"), None).await;
    state
        .vfs
        .db
        .app_data_write("comics", &comics.revision, "progress", Some("2"), None)
        .await
        .unwrap();
    state.vfs.db.uninstall_app("comics", false).await.unwrap();
    let shorts = add_app(&state, &manifest(), None).await;
    save_settings(&state, &shorts, json!({"source_dir":"/短视频"}))
        .await
        .unwrap();
    let pool = raw(&state).await;
    db_install(&state, &named_manifest("reader"), true)
        .await
        .unwrap();
    assert_eq!(jobs(&pool).await, ["books", "comics"]);
    for sql in [
        "SELECT COUNT(*) FROM apps WHERE id IN ('books', 'comics')",
        "SELECT COUNT(*) FROM app_settings WHERE app_id IN ('books', 'comics')",
        "SELECT COUNT(*) FROM app_data WHERE app_id IN ('books', 'comics')",
        "SELECT COUNT(*) FROM app_lifecycle WHERE app_id IN ('books', 'comics')",
        "SELECT COUNT(*) FROM app_recovery WHERE app_id IN ('books', 'comics')",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    assert!(state.vfs.db.app("books").await.unwrap().is_none());
    assert!(state.vfs.db.app("comics").await.unwrap().is_none());
    assert_eq!(
        installed(&state, "shorts").await.unwrap().revision,
        shorts.revision
    );
    assert_eq!(
        read_settings(&state, &shorts).await.unwrap()["source_dir"],
        "/短视频"
    );
}

#[tokio::test]
async fn failed_replacement_rolls_back_reader_legacy_rows_and_cleanup_jobs() {
    let (state, _, _dir) = fixture().await;
    let books = add_app(&state, &named_manifest("books"), None).await;
    save_settings(&state, &books, json!({"source_dir":"/保留"}))
        .await
        .unwrap();
    state
        .vfs
        .db
        .app_data_write("books", &books.revision, "progress", Some("123"), None)
        .await
        .unwrap();
    put_cover(&state, "books", "keep").await;
    let pool = raw(&state).await;
    // books 已经删除并排队后，在第二项删除中失败，证明整批操作回滚。
    sqlx::raw_sql("CREATE TRIGGER fail_replacement BEFORE DELETE ON app_lifecycle WHEN OLD.app_id = 'comics' BEGIN SELECT RAISE(ABORT, 'test rollback'); END;")
        .execute(&pool).await.unwrap();
    add_app(&state, &named_manifest("comics"), None).await;
    assert!(
        db_install(&state, &named_manifest("reader"), true)
            .await
            .is_err()
    );
    assert!(state.vfs.db.app("reader").await.unwrap().is_none());
    assert_eq!(
        installed(&state, "books").await.unwrap().revision,
        books.revision
    );
    assert_eq!(
        read_settings(&state, &books).await.unwrap()["source_dir"],
        "/保留"
    );
    assert!(
        state
            .vfs
            .db
            .app_data_get("books", "progress")
            .await
            .unwrap()
            .is_some()
    );
    assert!(state.vfs.db.app("comics").await.unwrap().is_some());
    assert!(jobs(&pool).await.is_empty());
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 1);
    assert!(cache_root(&state).join("books").exists());
}

#[tokio::test]
async fn each_private_residue_requires_confirmation_and_fresh_reader_enqueues_both_jobs() {
    let (state, _, _dir) = fixture().await;
    let pool = raw(&state).await;
    for (insert, delete) in [
        (
            "INSERT INTO app_settings(app_id, settings) VALUES('books', '{}')",
            "DELETE FROM app_settings WHERE app_id = 'books'",
        ),
        (
            "INSERT INTO app_data(app_id, key, value, revision, updated_at) VALUES('books', 'k', '1', 'r', 0)",
            "DELETE FROM app_data WHERE app_id = 'books'",
        ),
        (
            "INSERT INTO app_lifecycle(app_id, updated_at) VALUES('books', 0)",
            "DELETE FROM app_lifecycle WHERE app_id = 'books'",
        ),
    ] {
        sqlx::query(insert).execute(&pool).await.unwrap();
        let replacements = state
            .vfs
            .db
            .app_replacement_preview("reader")
            .await
            .unwrap()
            .replacements;
        assert_eq!(replacements.len(), 1);
        assert!(!replacements[0].installed);
        assert!(replacements[0].has_data);
        assert!(matches!(
            db_install(&state, &named_manifest("reader"), false).await,
            Err(AppError::Conflict(_))
        ));
        sqlx::query(delete).execute(&pool).await.unwrap();
    }
    db_install(&state, &named_manifest("reader"), false)
        .await
        .unwrap();
    assert_eq!(jobs(&pool).await, ["books", "comics"]);
    let initial: String =
        sqlx::query_scalar("SELECT token FROM app_cleanup_jobs WHERE app_id = 'books'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut next = named_manifest("reader");
    next.version = "1.1.0".into();
    db_install(&state, &next, false).await.unwrap();
    let refreshed: String =
        sqlx::query_scalar("SELECT token FROM app_cleanup_jobs WHERE app_id = 'books'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(initial, refreshed);
    assert_eq!(jobs(&pool).await, ["books", "comics"]);
}

#[tokio::test]
async fn persisted_cleanup_retries_after_restart_and_keeps_reinstalled_app_caches() {
    let (state, media, _dir) = fixture().await;
    let pool = raw(&state).await;
    db_install(&state, &named_manifest("reader"), false)
        .await
        .unwrap();
    put_cover(&state, "books", "orphan").await;
    put_cover(&state, "comics", "orphan").await;
    let root = cache_root(&state);
    tokio::fs::create_dir_all(root.join("comics/old-digest"))
        .await
        .unwrap();
    // 路径为文件导致目录回收失败；另一项仍必须被处理。
    tokio::fs::write(root.join("books"), b"blocked")
        .await
        .unwrap();
    assert!(
        cleanup_caches(&state.vfs.db, &state.rt.env.db_path)
            .await
            .is_err()
    );
    assert_eq!(jobs(&pool).await, ["books"]);
    assert!(!root.join("comics").exists());
    assert_eq!(state.vfs.db.cover_stats("comics").await.unwrap().entries, 0);
    tokio::fs::remove_file(root.join("books")).await.unwrap();
    tokio::fs::create_dir_all(root.join("books/orphan"))
        .await
        .unwrap();
    let restarted = state_at(std::path::Path::new(&state.rt.env.db_path), media).await;
    cleanup_caches(&restarted.vfs.db, &restarted.rt.env.db_path)
        .await
        .unwrap();
    assert!(jobs(&pool).await.is_empty());
    assert!(!root.join("books").exists());
    let mut next = named_manifest("reader");
    next.version = "1.1.0".into();
    db_install(&restarted, &next, false).await.unwrap();
    let books = add_app(&restarted, &named_manifest("books"), None).await;
    put_cover(&restarted, "books", "new").await;
    assert_eq!(jobs(&pool).await, ["comics"]); // 重装已在自己的安装事务内撤销旧任务。
    cleanup_caches(&restarted.vfs.db, &restarted.rt.env.db_path)
        .await
        .unwrap();
    assert!(root.join("books").join(books.digest).exists());
    assert_eq!(
        restarted.vfs.db.cover_stats("books").await.unwrap().entries,
        1
    );
    assert!(jobs(&pool).await.is_empty());
}

#[tokio::test]
async fn cleanup_cannot_interleave_with_install_materialization() {
    let (state, _, _dir) = fixture().await;
    db_install(&state, &named_manifest("reader"), false)
        .await
        .unwrap();
    let install_guard = INSTALL_LOCK.lock().await;
    let worker_state = state.clone();
    let worker = tokio::spawn(async move {
        cleanup_caches(&worker_state.vfs.db, &worker_state.rt.env.db_path).await
    });
    let m = named_manifest("books");
    let package = AppPackage::parse(archive(&m)).unwrap();
    let digest = package.digest.clone();
    {
        let _cache_guard = CACHE_LOCK.lock().await;
        package.materialize(&cache_root(&state)).unwrap();
    }
    assert!(!worker.is_finished());
    db_install(&state, &m, false).await.unwrap();
    put_cover(&state, "books", "new").await;
    drop(install_guard);
    worker.await.unwrap().unwrap();
    assert!(cache_root(&state).join("books").join(digest).exists());
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 1);
}

#[tokio::test]
async fn cleanup_holds_database_write_lock_until_external_deletion_finishes() {
    let (state, _, _dir) = fixture().await;
    db_install(&state, &named_manifest("reader"), false)
        .await
        .unwrap();
    put_cover(&state, "books", "old").await;
    let covers = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        crate::db::apps::covers_path(&state.rt.env.db_path).display()
    ))
    .await
    .unwrap();
    let held_cover = covers.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let worker_state = state.clone();
    let worker = tokio::spawn(async move {
        cleanup_caches(&worker_state.vfs.db, &worker_state.rt.env.db_path).await
    });
    let probe = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&state.rt.env.db_path)
                .busy_timeout(std::time::Duration::ZERO),
        )
        .await
        .unwrap();
    // 封面锁让清理暂停在“主库已检查、外部删除未完成”，稳定制造危险窗口。
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match probe.begin_with("BEGIN IMMEDIATE").await {
                Ok(tx) => tx.rollback().await.unwrap(),
                Err(error)
                    if error
                        .as_database_error()
                        .is_some_and(|e| e.code().as_deref() == Some("5")) =>
                {
                    break;
                }
                Err(error) => panic!("写锁探测失败: {error}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let installer_state = state.clone();
    let mut installer =
        tokio::spawn(
            async move { db_install(&installer_state, &named_manifest("books"), false).await },
        );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut installer)
            .await
            .is_err()
    );
    held_cover.rollback().await.unwrap();
    worker.await.unwrap().unwrap();
    installer.await.unwrap().unwrap();
    put_cover(&state, "books", "new").await;
    let pool = raw(&state).await;
    // 即使恢复出一条落后的任务，已安装检查仍保护后来应用的缓存。
    sqlx::query(
        "INSERT INTO app_cleanup_jobs(app_id, token, created_at) VALUES('books', 'stale', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    cleanup_caches(&state.vfs.db, &state.rt.env.db_path)
        .await
        .unwrap();
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 1);
    assert!(jobs(&pool).await.is_empty());
}
