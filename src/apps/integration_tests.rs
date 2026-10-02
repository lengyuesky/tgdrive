use super::*;

fn integrated_manifest() -> Manifest {
    let mut m = manifest();
    m.min_host_version = "0.3.0".into();
    m.integration = Some(package::Integration {
        file_types: vec!["txt".into()],
        data_schema: 1,
        ..Default::default()
    });
    m
}

#[tokio::test]
async fn new_install_denies_files_until_host_authorizes_and_revokes_tickets() {
    let (state, media, _dir) = fixture().await;
    state.vfs.mkdir("/书").await.unwrap();
    seed_file(&state, &media, "/书/a.txt", b"abc", 2).await;
    let package = AppPackage::parse(archive(&integrated_manifest())).unwrap();
    let digest = package.digest.clone();
    let app = install_package(
        &state,
        package,
        InstallOptions {
            digest,
            expected_revision: None,
        },
        "import",
    )
    .await
    .unwrap();
    assert_eq!(app.scope_mode, "none");
    let session = launch_app(&state, "shorts").await["session"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            &session,
            "files.stat",
            json!({"path":"/书/a.txt"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/apps/shorts/authorize",
            json!({"path":"/书","expected_revision":app.revision})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, file) = call(
        &state,
        "POST",
        "/api/apps/shorts/open",
        json!({"path":"/书/a.txt"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(file["content_version"].as_str().unwrap().len() == 64);
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            &session,
            "files.list",
            json!({"path":"/"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, ticket) = rpc_call(
        &state,
        "shorts",
        &session,
        "media.url",
        json!({"id":file["id"],"content_version":file["content_version"]}),
    )
    .await;
    assert!(ticket["url"].is_string());
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/shorts/scope",
            json!({"mode":"none","paths":[]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            &session,
            "files.stat",
            json!({"id":file["id"]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(ticket["url"].as_str().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn recovery_restores_private_data_and_package_with_new_revision_and_current_scope() {
    let (state, _media, _dir) = fixture().await;
    let mut manifest = integrated_manifest();
    let first = add_app(&state, &manifest, None).await;
    state
        .vfs
        .db
        .app_data_write(
            "shorts",
            &first.revision,
            "progress:1",
            Some("{\"page\":3}"),
            None,
        )
        .await
        .unwrap();
    state
        .vfs
        .db
        .patch_app_settings("shorts", &first.revision, "{\"source_dir\":\"/书\"}")
        .await
        .unwrap();
    assert_eq!(
        state
            .vfs
            .db
            .app_migration("shorts", &first.revision, "begin", 1, "page-a", "")
            .await
            .unwrap()["needed"],
        true
    );
    assert!(
        state
            .vfs
            .db
            .app_migration("shorts", &first.revision, "begin", 1, "page-b", "")
            .await
            .is_err()
    );
    state
        .vfs
        .db
        .app_migration("shorts", &first.revision, "complete", 1, "page-a", "")
        .await
        .unwrap();
    manifest.version = "1.1.0".into();
    let second = add_app(&state, &manifest, Some(first.revision.clone())).await;
    let previous = state
        .vfs
        .db
        .app_data_get("shorts", "progress:1")
        .await
        .unwrap()
        .unwrap();
    state
        .vfs
        .db
        .app_data_write(
            "shorts",
            &second.revision,
            "progress:1",
            Some("{\"page\":9}"),
            Some(&previous.revision),
        )
        .await
        .unwrap();
    state
        .vfs
        .db
        .set_app_scope_mode("shorts", "none", "[]", &second.revision)
        .await
        .unwrap();
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/apps/shorts/recovery",
            json!({"expected_revision":first.revision})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, restored) = call(
        &state,
        "POST",
        "/api/apps/shorts/recovery",
        json!({"expected_revision":second.revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["manifest"]["version"], "1.0.0");
    assert_eq!(restored["scope_mode"], "none");
    assert_ne!(restored["revision"], first.revision);
    assert_ne!(restored["revision"], second.revision);
    assert_eq!(
        state
            .vfs
            .db
            .app_data_get("shorts", "progress:1")
            .await
            .unwrap()
            .unwrap()
            .value,
        "{\"page\":3}"
    );
    assert!(state.vfs.db.app_health("shorts").await.unwrap()["recovery"].is_null());
}

#[test]
fn journal_replays_changes_and_detects_restart_or_overflow() {
    let bus = crate::events::bus();
    let cursor = bus.file_changes(None).0;
    crate::events::emit(&bus, crate::events::HostEvent::files_at(vec!["/书".into()]));
    let (next, paths) = bus.file_changes(Some(&cursor));
    assert_eq!(paths.unwrap(), vec!["/书"]);
    assert_eq!(
        bus.file_changes(Some(&next)).1.unwrap(),
        Vec::<String>::new()
    );
    assert!(crate::events::bus().file_changes(Some(&next)).1.is_none());
    for _ in 0..257 {
        crate::events::emit(&bus, crate::events::HostEvent::files_at(vec!["/书".into()]));
    }
    assert!(bus.file_changes(Some(&cursor)).1.is_none());
}

#[tokio::test]
async fn uninstall_preserves_data_version_until_data_is_explicitly_purged() {
    let (state, _media, _dir) = fixture().await;
    let mut newer = integrated_manifest();
    newer.integration.as_mut().unwrap().data_schema = 2;
    let app = add_app(&state, &newer, None).await;
    state
        .vfs
        .db
        .app_migration("shorts", &app.revision, "begin", 2, "owner", "")
        .await
        .unwrap();
    state
        .vfs
        .db
        .app_migration("shorts", &app.revision, "complete", 2, "owner", "")
        .await
        .unwrap();
    state.vfs.db.uninstall_app("shorts", false).await.unwrap();
    let old = add_app(&state, &integrated_manifest(), None).await;
    assert!(
        state
            .vfs
            .db
            .app_migration("shorts", &old.revision, "begin", 1, "owner", "")
            .await
            .is_err()
    );
    state.vfs.db.uninstall_app("shorts", true).await.unwrap();
    let clean = add_app(&state, &integrated_manifest(), None).await;
    assert_eq!(
        state
            .vfs
            .db
            .app_migration("shorts", &clean.revision, "begin", 1, "owner", "")
            .await
            .unwrap()["needed"],
        true
    );
}

#[tokio::test]
async fn upgrade_removes_retired_settings_and_keeps_current_private_data() {
    let (state, _media, _dir) = fixture().await;
    let mut manifest = integrated_manifest();
    let first = add_app(&state, &manifest, None).await;
    state
        .vfs
        .db
        .patch_app_settings("shorts", &first.revision, "{\"source_dir\":\"/旧目录\"}")
        .await
        .unwrap();
    state
        .vfs
        .db
        .app_data_write(
            "shorts",
            &first.revision,
            "library:sources",
            Some("{\"schemaVersion\":1,\"sources\":[]}"),
            None,
        )
        .await
        .unwrap();
    manifest.version = "1.1.0".into();
    manifest.settings.clear();
    let app = add_app(&state, &manifest, Some(first.revision)).await;
    assert_eq!(state.vfs.db.app_settings("shorts").await.unwrap(), "{}");
    assert!(
        state
            .vfs
            .db
            .app_data_get("shorts", "library:sources")
            .await
            .unwrap()
            .is_some()
    );
    let (_, health) = call(&state, "GET", "/api/apps/shorts/health", json!({})).await;
    assert_eq!(health["recovery"]["version"], "1.0.0");
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let response = router(&state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/apps/shorts/scope")
                .header(header::AUTHORIZATION, format!("Basic {credentials}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"paths":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        installed(&state, &app.manifest.id)
            .await
            .unwrap()
            .scope_mode,
        "all"
    );
}
