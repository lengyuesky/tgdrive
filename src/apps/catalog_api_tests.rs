use super::*;
use crate::apps::catalog::{CatalogEntry, CatalogPayload, CatalogSourceConfig, CatalogTransport};
use crate::proxy::ProxyConfig;
use sha2::{Digest, Sha256};
use std::time::Duration;

const REPOSITORY: &str = "lengyuesky/tgdrive-app";

fn entry(manifest: Manifest, bytes: &[u8]) -> CatalogEntry {
    CatalogEntry {
        url: format!(
            "https://raw.githubusercontent.com/{REPOSITORY}/main/apps/{}-{}.tgapp",
            manifest.id, manifest.version
        ),
        manifest,
        sha256: crate::engine::upload::to_hex(&Sha256::digest(bytes)),
        size: bytes.len(),
    }
}

fn payload(entry: CatalogEntry) -> Vec<u8> {
    serde_json::to_vec(&CatalogPayload {
        schema_version: 2,
        repository: REPOSITORY.into(),
        entries: vec![entry],
    })
    .unwrap()
}

struct RemotePackages {
    catalog: Vec<u8>,
    package: Vec<u8>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl CatalogTransport for RemotePackages {
    async fn fetch(
        &self,
        url: &str,
        repo: &str,
        _: &ProxyConfig,
        _: Duration,
        _: usize,
    ) -> AppResult<Vec<u8>> {
        assert_eq!(repo, REPOSITORY);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(if url.ends_with("catalog.json") {
            self.catalog.clone()
        } else {
            self.package.clone()
        })
    }
}

fn manager(root: &std::path::Path, transport: Arc<dyn CatalogTransport>) -> Arc<CatalogManager> {
    Arc::new(CatalogManager::new(
        root,
        CatalogSourceConfig::Github {
            repo: REPOSITORY.into(),
        },
        transport,
        Arc::new(ProxyConfig::default),
    ))
}

#[tokio::test]
async fn remote_download_and_install_reject_manifest_digest_or_length_mismatch_without_changing_old_install()
 {
    let (mut state, _, dir) = fixture().await;
    let first = add_app(&state, &manifest(), None).await;
    save_settings(
        &state,
        &first,
        json!({"source_dir":"/保留目录", "muted":false}),
    )
    .await
    .unwrap();
    let old_session = launch_app(&state, "shorts").await;
    let old_archive = state
        .vfs
        .db
        .app_package("shorts", &first.digest)
        .await
        .unwrap();
    let mut next = manifest();
    next.version = "1.1.0".into();
    let next_archive = archive(&next);
    let correct = entry(next, &next_archive);
    for warm in [false, true] {
        for mismatch in ["permissions", "settings", "author", "digest", "length"] {
            let root = dir.path().join(format!("catalog-{warm}-{mismatch}"));
            let mut wrong = correct.clone();
            match mismatch {
                "permissions" => wrong.manifest.permissions.clear(),
                "settings" => wrong.manifest.settings.clear(),
                "author" => wrong.manifest.author = "被修改的作者".into(),
                "digest" => wrong.sha256 = "0".repeat(64),
                "length" => wrong.size += 1,
                _ => unreachable!(),
            }
            let bytes = payload(wrong.clone());
            if warm {
                let valid = manager(
                    &root,
                    Arc::new(RemotePackages {
                        catalog: payload(correct.clone()),
                        package: next_archive.clone(),
                        calls: AtomicUsize::new(0),
                    }),
                );
                valid.refresh(true).await;
                valid.get_package("shorts", None).await.unwrap();
                // 重启时遇到结构有效但清单被改写的目录缓存，也不能信任已缓存安装包。
                std::fs::write(root.join("lengyuesky__tgdrive-app/catalog.json"), &bytes).unwrap();
            }
            state.apps_catalog = manager(
                &root,
                Arc::new(RemotePackages {
                    catalog: bytes,
                    package: next_archive.clone(),
                    calls: AtomicUsize::new(0),
                }),
            );
            state.apps_catalog.refresh(true).await;
            let (_, index) = call(&state, "GET", "/api/apps", json!({})).await;
            assert_eq!(index["available"].as_array().unwrap().len(), 1);
            assert_eq!(index["installed"][0]["revision"], first.revision);
            let (status, error) = call(
                &state,
                "GET",
                &format!("/api/apps/catalog/shorts/package?digest={}", wrong.sha256),
                json!({}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{warm}/{mismatch}: {error}"
            );
            let (status, error) = call(
                &state,
                "POST",
                "/api/apps/catalog/shorts/install",
                json!({"digest":wrong.sha256, "expected_revision":first.revision}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{warm}/{mismatch}: {error}"
            );
            let kept = installed(&state, "shorts").await.unwrap();
            assert_eq!(kept.digest, first.digest);
            assert_eq!(kept.revision, first.revision);
            assert_eq!(kept.manifest, first.manifest);
            assert_eq!(
                read_settings(&state, &kept).await.unwrap(),
                json!({"source_dir":"/保留目录", "muted":false})
            );
            assert_eq!(
                state
                    .vfs
                    .db
                    .app_package("shorts", &first.digest)
                    .await
                    .unwrap(),
                old_archive
            );
            assert_eq!(
                rpc_call(
                    &state,
                    "shorts",
                    old_session["session"].as_str().unwrap(),
                    "app.ping",
                    json!({})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
    }
}

#[tokio::test]
async fn old_confirmation_survives_history_removal_and_restart_without_granting_hidden_permissions()
{
    let (mut state, _, dir) = fixture().await;
    let mut old_manifest = manifest();
    old_manifest.permissions.clear();
    let first = add_app(&state, &old_manifest, None).await;
    save_settings(
        &state,
        &first,
        json!({"source_dir":"/保留目录", "muted":false}),
    )
    .await
    .unwrap();
    let session = launch_app(&state, "shorts").await;
    let token = session["session"].as_str().unwrap();
    let (status, saved) = rpc_call(
        &state,
        "shorts",
        token,
        "storage.set",
        json!({
            "key":"review:bookmark", "value":{"position":37}, "expected_revision":null,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let old_archive = state
        .vfs
        .db
        .app_package("shorts", &first.digest)
        .await
        .unwrap();
    let mut next = manifest();
    next.version = "1.1.0".into();
    let archive = archive(&next);
    let correct = entry(next, &archive);
    let mut hidden = correct.clone();
    hidden.manifest.permissions.clear();
    let initial = payload(hidden.clone());
    for filtered in [false, true] {
        let root = dir.path().join(format!("history-guard-{filtered}"));
        state.apps_catalog = manager(
            &root,
            Arc::new(RemotePackages {
                catalog: initial.clone(),
                package: archive.clone(),
                calls: AtomicUsize::new(0),
            }),
        );
        state.apps_catalog.refresh(true).await;
        let (_, preview) = call(&state, "GET", "/api/apps", json!({})).await;
        assert_eq!(
            preview["available"][0]["manifest"]["permissions"],
            json!([])
        );
        let mut rollback = hidden.clone();
        rollback.manifest.version = "1.0.0".into();
        rollback.url = format!(
            "https://raw.githubusercontent.com/{REPOSITORY}/main/apps/{}-{}.tgapp",
            rollback.manifest.id, rollback.manifest.version
        );
        let intermediate = if filtered {
            payload(rollback)
        } else {
            // 同版本元数据篡改
            payload(correct.clone())
        };
        for catalog in [intermediate] {
            // 每阶段重建管理器，同时验证持久目录不能被删除步骤覆盖。
            state.apps_catalog = manager(
                &root,
                Arc::new(RemotePackages {
                    catalog,
                    package: archive.clone(),
                    calls: AtomicUsize::new(0),
                }),
            );
            let (status, index) = call(
                &state,
                "POST",
                "/api/apps/catalog/refresh",
                json!({"force":true}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(index["catalog"]["state"], "stale");
            assert_eq!(
                std::fs::read(root.join("lengyuesky__tgdrive-app/catalog.json")).unwrap(),
                initial
            );
        }
        let (status, _) = call(
            &state,
            "POST",
            "/api/apps/catalog/shorts/install",
            json!({
                "digest":hidden.sha256, "expected_revision":first.revision,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let kept = installed(&state, "shorts").await.unwrap();
        assert_eq!(kept.manifest, first.manifest);
        assert_eq!(kept.revision, first.revision);
        assert_eq!(
            state
                .vfs
                .db
                .app_package("shorts", &kept.digest)
                .await
                .unwrap(),
            old_archive
        );
        assert_eq!(
            read_settings(&state, &kept).await.unwrap(),
            json!({"source_dir":"/保留目录", "muted":false})
        );
        let (status, record) = rpc_call(
            &state,
            "shorts",
            token,
            "storage.get",
            json!({"key":"review:bookmark"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(record, saved);
    }
}

#[tokio::test]
async fn catalog_refresh_download_and_install_all_require_login() {
    let (mut state, _, dir) = fixture().await;
    let transport = Arc::new(RemotePackages {
        catalog: vec![],
        package: vec![],
        calls: AtomicUsize::new(0),
    });
    state.apps_catalog = manager(dir.path(), transport.clone());
    for (method, uri) in [
        ("POST", "/api/apps/catalog/refresh"),
        ("GET", "/api/apps/catalog/shorts/package"),
        ("POST", "/api/apps/catalog/shorts/install"),
    ] {
        let response = router(&state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"force":true,"digest":"untrusted"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

struct DelayedRemote {
    catalog: Vec<u8>,
    package: Vec<u8>,
    delay_catalog: bool,
    fail: bool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl CatalogTransport for DelayedRemote {
    async fn fetch(
        &self,
        url: &str,
        _: &str,
        _: &ProxyConfig,
        _: Duration,
        _: usize,
    ) -> AppResult<Vec<u8>> {
        let catalog = url.ends_with("catalog.json");
        if catalog == self.delay_catalog {
            self.started.notify_one();
            self.release.notified().await;
        }
        if self.fail {
            return Err(AppError::BadRequest("模拟目录网络失败".into()));
        }
        Ok(if catalog {
            self.catalog.clone()
        } else {
            self.package.clone()
        })
    }
}

#[tokio::test]
async fn pending_or_failed_catalog_refresh_does_not_block_installed_index_or_launch() {
    let (mut state, _, dir) = fixture().await;
    let app = add_app(&state, &manifest(), None).await;
    let transport = Arc::new(DelayedRemote {
        catalog: vec![],
        package: vec![],
        delay_catalog: true,
        fail: true,
        started: Default::default(),
        release: Default::default(),
    });
    state.apps_catalog = manager(&dir.path().join("catalog"), transport.clone());
    let refresh_state = state.clone();
    let refresh = tokio::spawn(async move {
        call(
            &refresh_state,
            "POST",
            "/api/apps/catalog/refresh",
            json!({"force":true}),
        )
        .await
    });
    transport.started.notified().await;
    let (status, index) = tokio::time::timeout(
        Duration::from_secs(1),
        call(&state, "GET", "/api/apps", json!({})),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index["installed"][0]["digest"], app.digest);
    assert_eq!(index["catalog"]["state"], "unavailable");
    tokio::time::timeout(Duration::from_secs(1), launch_app(&state, "shorts"))
        .await
        .unwrap();
    transport.release.notify_one();
    let (status, index) = refresh.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index["installed"][0]["revision"], app.revision);
    assert!(
        index["catalog"]["error"]
            .as_str()
            .unwrap()
            .contains("模拟目录网络失败")
    );
}

#[tokio::test]
async fn remote_package_wait_does_not_hold_install_lock_or_lose_state_updates() {
    let (mut state, _, dir) = fixture().await;
    let app = add_app(&state, &manifest(), None).await;
    let mut next = manifest();
    next.version = "1.1.0".into();
    let archive = archive(&next);
    let entry = entry(next, &archive);
    let digest = entry.sha256.clone();
    let transport = Arc::new(DelayedRemote {
        catalog: payload(entry),
        package: archive,
        delay_catalog: false,
        fail: false,
        started: Default::default(),
        release: Default::default(),
    });
    state.apps_catalog = manager(&dir.path().join("catalog"), transport.clone());
    state.apps_catalog.refresh(true).await;
    let install_state = state.clone();
    let install = tokio::spawn(async move {
        call(
            &install_state,
            "POST",
            "/api/apps/catalog/shorts/install",
            json!({"digest":digest, "expected_revision":app.revision}),
        )
        .await
    });
    transport.started.notified().await;
    let (status, updated) = tokio::time::timeout(
        Duration::from_secs(1),
        call(
            &state,
            "POST",
            "/api/apps/shorts/state",
            json!({"pinned":true}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["pinned"], true);
    assert_eq!(updated["manifest"]["version"], "1.0.0");
    transport.release.notify_one();
    let (status, installed) = install.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(installed["manifest"]["version"], "1.1.0");
    assert_eq!(installed["pinned"], true);
}

#[tokio::test]
async fn concurrent_remote_updates_of_same_revision_have_one_winner() {
    let (mut state, _, dir) = fixture().await;
    let old = add_app(&state, &manifest(), None).await;
    let mut next = manifest();
    next.version = "1.1.0".into();
    let archive = archive(&next);
    let entry = entry(next, &archive);
    let options = json!({"digest":entry.sha256, "expected_revision":old.revision});
    state.apps_catalog = manager(
        &dir.path().join("catalog"),
        Arc::new(RemotePackages {
            catalog: payload(entry.clone()),
            package: archive,
            calls: AtomicUsize::new(0),
        }),
    );
    state.apps_catalog.refresh(true).await;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            call(
                &state,
                "POST",
                "/api/apps/catalog/shorts/install",
                options.clone()
            ),
            call(&state, "POST", "/api/apps/catalog/shorts/install", options)
        )
    })
    .await
    .unwrap();
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    let installed = installed(&state, "shorts").await.unwrap();
    assert_eq!(installed.digest, entry.sha256);
    assert_eq!(installed.manifest.version, "1.1.0");
    assert_ne!(installed.revision, old.revision);
    assert_eq!(state.vfs.db.apps().await.unwrap().len(), 1);
}
