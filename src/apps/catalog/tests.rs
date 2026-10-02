use super::*;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use zip::write::SimpleFileOptions;

type MockHandler = Box<dyn Fn(&str, &str) -> AppResult<Vec<u8>> + Send + Sync>;

struct MockTransport {
    handler: MockHandler,
    calls: AtomicUsize,
}

impl MockTransport {
    fn new(handler: impl Fn(&str, &str) -> AppResult<Vec<u8>> + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl CatalogTransport for MockTransport {
    async fn fetch(
        &self,
        url: &str,
        expected_repo: &str,
        _proxy: &ProxyConfig,
        _timeout: Duration,
        _max_bytes: usize,
    ) -> AppResult<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.handler)(url, expected_repo)
    }
}

fn test_manifest(id: &str, version: &str, api_version: u32, min_host: &str) -> Manifest {
    Manifest {
        integration: None,
        id: id.into(),
        name: format!("测试应用 {id}"),
        version: version.into(),
        api_version,
        min_host_version: min_host.into(),
        description: "测试应用描述".into(),
        author: "测试作者".into(),
        entry: "index.html".into(),
        icon: None,
        permissions: vec!["files.read".into()],
        settings: vec![],
    }
}

fn test_archive(manifest: &Manifest) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let metadata = serde_json::to_vec(manifest).unwrap();
    zip.start_file(
        "app.json",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(&metadata).unwrap();
    zip.start_file(
        "index.html",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(b"<!DOCTYPE html><html><body>Test</body></html>")
        .unwrap();
    zip.finish().unwrap().into_inner()
}

#[test]
fn illegal_redirects_and_urls() {
    let repo = "lengyuesky/tgdrive-app";

    // 非 HTTPS
    assert!(
        validate_redirect_url(
            &Url::parse(
                "http://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 包含用户凭据
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://user:pass@raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 非 443 端口
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com:8443/lengyuesky/tgdrive-app/main/catalog.json"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 包含查询参数
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json?query=1"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 包含片段
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json#section"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 非法域名（仅允许 raw.githubusercontent.com）
    assert!(
        validate_redirect_url(
            &Url::parse("https://evil.com/lengyuesky/tgdrive-app/main/catalog.json").unwrap(),
            repo
        )
        .is_err()
    );
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://github.com/lengyuesky/tgdrive-app/releases/download/v1.0.0/app-1.0.0.tgapp"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://objects.githubusercontent.com/github-production-release-asset/xyz"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    assert!(
        validate_redirect_url(
            &Url::parse("https://release-assets.githubusercontent.com/xyz").unwrap(),
            repo
        )
        .is_err()
    );
    // 属于其他仓库
    assert!(
        validate_redirect_url(
            &Url::parse("https://raw.githubusercontent.com/other-org/other-repo/main/catalog.json")
                .unwrap(),
            repo
        )
        .is_err()
    );
    // 非 main 分支
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/dev/catalog.json"
            )
            .unwrap(),
            repo
        )
        .is_err()
    );
    // 合法目录链接
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json"
            )
            .unwrap(),
            repo
        )
        .is_ok()
    );
    // 合法安装包链接
    assert!(
        validate_redirect_url(
            &Url::parse(
                "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/app-1.0.0.tgapp"
            )
            .unwrap(),
            repo
        )
        .is_ok()
    );
}

#[test]
fn bad_index_and_cache_validation() {
    let repo = "lengyuesky/tgdrive-app";

    // 空内容
    assert!(CatalogManager::parse_catalog_bytes(b"", repo).is_err());
    // 超过 1 MiB
    let oversized = vec![b' '; MAX_CATALOG_SIZE + 1];
    assert!(CatalogManager::parse_catalog_bytes(&oversized, repo).is_err());
    // 错误协议版本（旧版本 schema 1 或未知版本）
    let bad_schema = serde_json::json!({
        "schema_version": 1,
        "repository": repo,
        "entries": []
    });
    assert!(
        CatalogManager::parse_catalog_bytes(&serde_json::to_vec(&bad_schema).unwrap(), repo)
            .is_err()
    );
    // 仓库不匹配
    let bad_repo = serde_json::json!({
        "schema_version": 2,
        "repository": "someone-else/app",
        "entries": []
    });
    assert!(
        CatalogManager::parse_catalog_bytes(&serde_json::to_vec(&bad_repo).unwrap(), repo).is_err()
    );

    // 相同应用 ID 重复出现必须拒绝（即使版本或摘要不同，最新协议每个 ID 仅限一条）
    let m = test_manifest("sample", "1.0.0", 2, "0.1.0");
    let conflicting_entries = serde_json::json!({
        "schema_version": 2,
        "repository": repo,
        "entries": [
            {
                "manifest": m,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 100,
                "url": format!("https://raw.githubusercontent.com/{repo}/main/apps/sample-1.0.0.tgapp"),
            },
            {
                "manifest": m,
                "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 100,
                "url": format!("https://raw.githubusercontent.com/{repo}/main/apps/sample-1.0.0.tgapp"),
            }
        ]
    });
    assert!(
        CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&conflicting_entries).unwrap(),
            repo
        )
        .is_err()
    );

    // 相同应用即使版本不同在同一目录也必须拒绝重复记录
    let mut m_next = m.clone();
    m_next.version = "1.1.0".into();
    let duplicate_entries = serde_json::json!({
        "schema_version": 2,
        "repository": repo,
        "entries": [
            {
                "manifest": m,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 100,
                "url": format!("https://raw.githubusercontent.com/{repo}/main/apps/sample-1.0.0.tgapp"),
            },
            {
                "manifest": m_next,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 100,
                "url": format!("https://raw.githubusercontent.com/{repo}/main/apps/sample-1.1.0.tgapp"),
            }
        ]
    });
    assert!(
        CatalogManager::parse_catalog_bytes(&serde_json::to_vec(&duplicate_entries).unwrap(), repo)
            .is_err()
    );
}

#[tokio::test]
async fn remote_timeout_and_error_handling() {
    let repo = "lengyuesky/tgdrive-app";
    let url = format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
    let calls = AtomicUsize::new(0);
    let result = fetch_http(
        &url,
        repo,
        Duration::from_millis(50),
        MAX_CATALOG_SIZE,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(http_response(200, None, catalog_bytes(vec![])))
            }
        },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("总超时"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_403_and_404_and_429() {
    for status in [403, 404, 429] {
        let tmp = tempfile::tempdir().unwrap();
        let transport = Arc::new(ScriptedTransport::new(vec![http_response(
            status,
            None,
            "不可信的上游错误正文".as_bytes().to_vec(),
        )]));
        let manager = CatalogManager::new(
            tmp.path(),
            CatalogSourceConfig::Github {
                repo: "lengyuesky/tgdrive-app".into(),
            },
            transport.clone(),
            Arc::new(ProxyConfig::default),
        );
        manager.refresh(true).await;
        let result = manager.status().await;
        assert_eq!(result.state, "unavailable");
        assert!(result.error.as_ref().unwrap().contains(&status.to_string()));
        assert!(!result.error.unwrap().contains("上游错误正文"));
        assert_eq!(transport.urls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn compatibility_filtering() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m_compat_v1 = test_manifest("reader", "1.0.0", 2, "0.1.0");
    let m_incompat_api = test_manifest("future", "1.0.0", 99, "0.1.0");
    let m_incompat_host = test_manifest("other", "1.0.0", 2, "999.0.0");

    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![
            CatalogEntry {
                manifest: m_compat_v1.clone(),
                sha256: "1111111111111111111111111111111111111111111111111111111111111111".into(),
                size: 100,
                url: format!(
                    "https://raw.githubusercontent.com/{repo}/main/apps/reader-1.0.0.tgapp"
                ),
            },
            CatalogEntry {
                manifest: m_incompat_api,
                sha256: "2222222222222222222222222222222222222222222222222222222222222222".into(),
                size: 100,
                url: format!(
                    "https://raw.githubusercontent.com/{repo}/main/apps/future-1.0.0.tgapp"
                ),
            },
            CatalogEntry {
                manifest: m_incompat_host,
                sha256: "3333333333333333333333333333333333333333333333333333333333333333".into(),
                size: 100,
                url: format!(
                    "https://raw.githubusercontent.com/{repo}/main/apps/other-1.0.0.tgapp"
                ),
            },
        ],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    let transport = Arc::new(MockTransport::new(move |_, _| Ok(payload_bytes.clone())));
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;
    let (_status, available) = manager.available_apps(&[]).await;

    // reader 兼容应显示；future 和 other 不兼容，不显示
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.id, "reader");
    assert_eq!(available[0].manifest.version, "1.0.0");
}

#[tokio::test]
async fn persistent_restart_and_recovery() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m = test_manifest("demo", "1.0.0", 2, "0.1.0");
    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![CatalogEntry {
            manifest: m.clone(),
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            size: 100,
            url: format!("https://raw.githubusercontent.com/{repo}/main/apps/demo-1.0.0.tgapp"),
        }],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    let transport = Arc::new(MockTransport::new(move |_, _| Ok(payload_bytes.clone())));
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport.clone(),
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");

    // 重启：构造新的 manager 指向相同缓存目录，无需联网即可恢复
    let restarted = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport.clone(),
        Arc::new(ProxyConfig::default),
    );
    assert_eq!(restarted.status().await.state, "ready");
    let (_status, available) = restarted.available_apps(&[]).await;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.id, "demo");

    // 若持久缓存被篡改损坏，启动时不崩溃，恢复为 unavailable 状态
    let repo_dir = tmp.path().join(sanitize_repo(repo));
    std::fs::write(repo_dir.join("catalog.json"), b"corrupted json data").unwrap();
    let restarted_corrupt = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );
    assert_eq!(restarted_corrupt.status().await.state, "unavailable");
}

#[tokio::test]
async fn digest_and_manifest_mismatch_rejection() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m = test_manifest("test-app", "1.0.0", 2, "0.1.0");
    let archive_bytes = test_archive(&m);
    let _real_digest = crate::engine::upload::to_hex(&Sha256::digest(&archive_bytes));

    // 目录中声明了错误的 SHA256
    let fake_digest = "0000000000000000000000000000000000000000000000000000000000000000";
    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![CatalogEntry {
            manifest: m.clone(),
            sha256: fake_digest.into(),
            size: archive_bytes.len(),
            url: format!("https://raw.githubusercontent.com/{repo}/main/apps/test-app-1.0.0.tgapp"),
        }],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();
    let bytes_clone = archive_bytes.clone();

    let transport = Arc::new(MockTransport::new(move |url, _| {
        if url.ends_with("catalog.json") {
            Ok(payload_bytes.clone())
        } else {
            Ok(bytes_clone.clone())
        }
    }));

    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;

    // 获取包时因为摘要不匹配而拒绝
    let err = manager
        .get_package("test-app", Some(fake_digest))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("校验失败"));
}

#[tokio::test]
async fn stale_status_when_network_fails_after_cached() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m = test_manifest("cached-app", "1.0.0", 2, "0.1.0");
    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![CatalogEntry {
            manifest: m.clone(),
            sha256: "1111111111111111111111111111111111111111111111111111111111111111".into(),
            size: 100,
            url: format!(
                "https://raw.githubusercontent.com/{repo}/main/apps/cached-app-1.0.0.tgapp"
            ),
        }],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    let succeed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let succeed_clone = succeed.clone();
    let transport = Arc::new(MockTransport::new(move |_, _| {
        if succeed_clone.load(Ordering::SeqCst) {
            Ok(payload_bytes.clone())
        } else {
            Err(AppError::BadRequest("网络连接被重置".into()))
        }
    }));

    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");

    // 网络断开后刷新：标记为 stale 并保留上次条目
    succeed.store(false, Ordering::SeqCst);
    // 强制刷新并跳过 30s 限制（重置 last_network_attempt）
    {
        let mut snap = manager.snapshot.write().await;
        snap.last_network_attempt = None;
    }
    manager.refresh(true).await;

    let status = manager.status().await;
    assert_eq!(status.state, "stale");
    assert!(status.error.unwrap().contains("网络连接被重置"));

    // 条目仍可读取
    let (_status, available) = manager.available_apps(&[]).await;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.id, "cached-app");
}

#[tokio::test]
async fn confirm_then_catalog_changed_returns_409() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m = test_manifest("sample-app", "1.0.0", 2, "0.1.0");
    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![CatalogEntry {
            manifest: m.clone(),
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            size: 100,
            url: format!(
                "https://raw.githubusercontent.com/{repo}/main/apps/sample-app-1.0.0.tgapp"
            ),
        }],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    let transport = Arc::new(MockTransport::new(move |_, _| Ok(payload_bytes.clone())));
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;

    // 用户确认安装时的 digest 是旧的或不存在的
    let old_digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let err = manager
        .get_package("sample-app", Some(old_digest))
        .await
        .unwrap_err();
    match err {
        AppError::Conflict(msg) => {
            assert!(msg.contains("插件包已经变化"));
        }
        other => panic!("期望 Conflict 错误，实际得到: {other}"),
    }
}

#[tokio::test]
async fn package_cache_and_lru_pruning() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();

    let m = test_manifest("lru-app", "1.0.0", 2, "0.1.0");
    let archive_bytes = test_archive(&m);
    let digest = crate::engine::upload::to_hex(&Sha256::digest(&archive_bytes));

    let payload = CatalogPayload {
        schema_version: 2,
        repository: repo.into(),
        entries: vec![CatalogEntry {
            manifest: m.clone(),
            sha256: digest.clone(),
            size: archive_bytes.len(),
            url: format!("https://raw.githubusercontent.com/{repo}/main/apps/lru-app-1.0.0.tgapp"),
        }],
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap();
    let archive_clone = archive_bytes.clone();

    let transport = Arc::new(MockTransport::new(move |url, _| {
        if url.ends_with("catalog.json") {
            Ok(payload_bytes.clone())
        } else {
            Ok(archive_clone.clone())
        }
    }));

    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport.clone(),
        Arc::new(ProxyConfig::default),
    );

    manager.refresh(true).await;

    // 首次获取：从网络下载并写入持久缓存
    let (_manifest, pkg_digest, bytes) =
        manager.get_package("lru-app", Some(&digest)).await.unwrap();
    assert_eq!(pkg_digest, digest);
    assert_eq!(bytes, archive_bytes);

    let cached_file = tmp
        .path()
        .join(sanitize_repo(repo))
        .join("packages")
        .join(format!("{digest}.tgapp"));
    assert!(cached_file.is_file());

    // 再次获取：直接从持久缓存读取，不触发额外网络请求
    let calls_before = transport.calls.load(Ordering::SeqCst);
    let (_m2, d2, b2) = manager.get_package("lru-app", Some(&digest)).await.unwrap();
    assert_eq!(d2, digest);
    assert_eq!(b2, archive_bytes);
    assert_eq!(transport.calls.load(Ordering::SeqCst), calls_before);
}

fn package_entry(manifest: Manifest, bytes: &[u8]) -> CatalogEntry {
    CatalogEntry {
        url: format!(
            "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/{}-{}.tgapp",
            manifest.id, manifest.version
        ),
        manifest,
        sha256: crate::engine::upload::to_hex(&Sha256::digest(bytes)),
        size: bytes.len(),
    }
}

fn catalog_bytes(entries: Vec<CatalogEntry>) -> Vec<u8> {
    serde_json::to_vec(&CatalogPayload {
        schema_version: 2,
        repository: "lengyuesky/tgdrive-app".into(),
        entries,
    })
    .unwrap()
}

fn remote_manager(
    root: &Path,
    catalog: Vec<u8>,
    package: Vec<u8>,
) -> (CatalogManager, Arc<MockTransport>) {
    let transport = Arc::new(MockTransport::new(move |url, _| {
        Ok(if url.ends_with("catalog.json") {
            catalog.clone()
        } else {
            package.clone()
        })
    }));
    let manager = CatalogManager::new(
        root,
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into(),
        },
        transport.clone(),
        Arc::new(ProxyConfig::default),
    );
    (manager, transport)
}

#[tokio::test]
async fn rejects_hidden_permissions_on_cold_and_hot_package_cache() {
    let actual = test_manifest("permissions", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&actual);
    for warm_cache in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut entry = package_entry(actual.clone(), &archive);
        if !warm_cache {
            entry.manifest.permissions.clear();
        }
        let digest = entry.sha256.clone();
        let (manager, _) = remote_manager(tmp.path(), catalog_bytes(vec![entry]), archive.clone());
        manager.refresh(true).await;
        assert_eq!(manager.status().await.state, "ready");
        if warm_cache {
            manager
                .get_package("permissions", Some(&digest))
                .await
                .unwrap();
            manager.snapshot.write().await.entries[0]
                .manifest
                .permissions
                .clear();
        }
        let error = manager
            .get_package("permissions", Some(&digest))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("清单不一致"), "{error}");
    }
}

#[tokio::test]
async fn rejects_all_manifest_differences_not_only_id_and_version() {
    let actual = test_manifest("metadata", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&actual);
    for field in [
        "author",
        "name",
        "description",
        "min_host_version",
        "api_version",
        "entry",
        "settings",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut advertised = actual.clone();
        match field {
            "author" => advertised.author = "另一个作者".into(),
            "name" => advertised.name = "另一个名称".into(),
            "description" => advertised.description = "不同简介".into(),
            "min_host_version" => advertised.min_host_version = "0.0.1".into(),
            "api_version" => advertised.api_version = 1,
            "entry" => advertised.entry = "other.html".into(),
            "settings" => advertised
                .settings
                .push(crate::apps::package::SettingField {
                    key: "note".into(),
                    label: "说明".into(),
                    description: String::new(),
                    kind: "string".into(),
                    default: serde_json::json!("目录默认值"),
                }),
            _ => unreachable!(),
        }
        let entry = package_entry(advertised, &archive);
        let (manager, _) = remote_manager(tmp.path(), catalog_bytes(vec![entry]), archive.clone());
        manager.refresh(true).await;
        assert_eq!(manager.status().await.state, "ready", "{field}");
        let error = manager.get_package("metadata", None).await.unwrap_err();
        assert!(error.to_string().contains("清单不一致"), "{field}: {error}");
    }
}

#[tokio::test]
async fn local_exact_digest_keeps_historical_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let first = test_archive(&test_manifest("history", "1.0.0", 2, "0.1.0"));
    let second = test_archive(&test_manifest("history", "1.1.0", 2, "0.1.0"));
    let digest = crate::engine::upload::to_hex(&Sha256::digest(&first));
    std::fs::write(tmp.path().join("history-1.0.0.tgapp"), &first).unwrap();
    let manager = CatalogManager::new(
        tmp.path().join("cache"),
        CatalogSourceConfig::Local {
            path: tmp.path().into(),
        },
        Arc::new(MockTransport::new(|_, _| panic!("本地目录不应联网"))),
        Arc::new(ProxyConfig::default),
    );
    assert_eq!(manager.available_apps(&[]).await.1[0].digest, digest);
    std::fs::write(tmp.path().join("history-1.1.0.tgapp"), &second).unwrap();
    assert_eq!(
        manager.available_apps(&[]).await.1[0].manifest.version,
        "1.1.0"
    );
    assert_eq!(
        manager
            .get_package("history", Some(&digest))
            .await
            .unwrap()
            .2,
        first
    );
    assert_eq!(
        manager.get_package("history", None).await.unwrap().2,
        second
    );
}

#[tokio::test]
async fn expired_persistent_catalog_is_stale_and_empty_success_stays_stale_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = remote_manager(tmp.path(), catalog_bytes(vec![]), vec![]);
    manager.refresh(true).await;
    let path = tmp.path().join("lengyuesky__tgdrive-app/catalog.json");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - CATALOG_CACHE_TTL - Duration::from_secs(1))
        .unwrap();
    let restarted = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into(),
        },
        Arc::new(MockTransport::new(|_, _| {
            Err(AppError::BadRequest("模拟断网".into()))
        })),
        Arc::new(ProxyConfig::default),
    );
    assert_eq!(restarted.status().await.state, "stale");
    restarted.refresh(false).await;
    let status = restarted.status().await;
    assert_eq!(status.state, "stale");
    assert!(status.last_success_at.is_some());
    assert!(status.error.unwrap().contains("模拟断网"));
}

#[test]
fn rejects_unstable_tags_versions_and_noncanonical_asset_urls() {
    let manifest = test_manifest("strict", "1.0.0", 2, "0.1.0");
    let entry = package_entry(manifest, b"package");
    let base: serde_json::Value = serde_json::from_slice(&catalog_bytes(vec![entry])).unwrap();
    // 拒绝未定义字段（如 release_tag, branch, ref）
    for field in ["release_tag", "branch", "ref"] {
        let mut top_unknown = base.clone();
        top_unknown[field] = serde_json::json!("v1.0.0");
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&top_unknown).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err(),
            "top level {field}"
        );
        let mut entry_unknown = base.clone();
        entry_unknown["entries"][0][field] = serde_json::json!("v1.0.0");
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&entry_unknown).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err(),
            "entry {field}"
        );
    }
    for version in ["banana", "01.0.0", "1.0.0-rc.1", "1.0.0+build"] {
        let mut value = base.clone();
        value["entries"][0]["manifest"]["version"] = serde_json::json!(version);
        value["entries"][0]["url"] = serde_json::json!(format!(
            "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/strict-{version}.tgapp"
        ));
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&value).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err(),
            "{version}"
        );
    }
    for url in [
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/strict-1.0.1.tgapp",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/other-1.0.0.tgapp",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/strict-1.0.0.tgapp?secret=credential",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/strict-1.0.0.tgapp#fragment",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/dev/apps/strict-1.0.0.tgapp",
        "https://github.com/lengyuesky/tgdrive-app/releases/download/v1.0.0/strict-1.0.0.tgapp",
        "https://objects.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/strict-1.0.0.tgapp",
    ] {
        let mut value = base.clone();
        value["entries"][0]["url"] = serde_json::json!(url);
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&value).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err(),
            "{url}"
        );
    }
}

#[tokio::test]
async fn unknown_future_manifest_does_not_hide_compatible_history() {
    let compatible = package_entry(test_manifest("history", "1.0.0", 2, "0.1.0"), b"old");
    let future = package_entry(test_manifest("future-app", "2.0.0", 99, "0.1.0"), b"new");
    let mut value: serde_json::Value =
        serde_json::from_slice(&catalog_bytes(vec![compatible, future])).unwrap();
    value["entries"][1]["manifest"]["future_feature"] = serde_json::json!(true);
    value["entries"][1]["manifest"]["permissions"] = serde_json::json!(["future.permission"]);
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = remote_manager(tmp.path(), serde_json::to_vec(&value).unwrap(), vec![]);
    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");
    let available = manager.available_apps(&[]).await.1;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.id, "history");
    assert_eq!(available[0].manifest.version, "1.0.0");
    assert!(
        manager
            .get_package("history", Some(&"0".repeat(64)))
            .await
            .is_err()
    );
}

#[test]
fn rejects_invalid_repository_names_without_trimming() {
    for repo in [
        "../repo",
        "owner/..",
        "owner/.",
        " owner/repo",
        "owner/repo ",
        "owner name/repo",
        "owner/repo/extra",
        "https://github.com/owner/repo",
    ] {
        assert!(!is_valid_repo(repo), "{repo}");
    }
    assert!(is_valid_repo("lengyuesky/tgdrive-app"));
}

fn http_response(status: u16, location: Option<&str>, body: Vec<u8>) -> reqwest::Response {
    let mut response = http::Response::builder().status(status);
    if let Some(location) = location {
        response = response.header(http::header::LOCATION, location);
    }
    response.body(reqwest::Body::from(body)).unwrap().into()
}

struct ScriptedTransport {
    responses: std::sync::Mutex<std::collections::VecDeque<reqwest::Response>>,
    urls: std::sync::Mutex<Vec<String>>,
    settings: std::sync::Mutex<Vec<(ProxyConfig, Duration, usize)>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<reqwest::Response>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            urls: Default::default(),
            settings: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl CatalogTransport for ScriptedTransport {
    async fn fetch(
        &self,
        url: &str,
        repo: &str,
        proxy: &ProxyConfig,
        timeout: Duration,
        max_bytes: usize,
    ) -> AppResult<Vec<u8>> {
        self.settings
            .lock()
            .unwrap()
            .push((proxy.clone(), timeout, max_bytes));
        fetch_http(url, repo, timeout, max_bytes, |url| {
            self.urls.lock().unwrap().push(url.to_string());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("不能发起未声明的网络请求");
            async move { Ok(response) }
        })
        .await
    }
}

#[test]
fn catalog_configuration_uses_explicit_values_without_changing_environment() {
    let github = CatalogSourceConfig::Github {
        repo: "lengyuesky/tgdrive-app".into(),
    };
    for directory in [None, Some(PathBuf::from("")), Some(PathBuf::from("  "))] {
        assert_eq!(
            CatalogSourceConfig::from_values(directory, None, None, || panic!(
                "默认远程模式不扫描本地"
            )),
            github
        );
    }
    assert_eq!(
        CatalogSourceConfig::from_values(
            Some(PathBuf::from("/fixture")),
            Some("false"),
            Some("invalid"),
            || None
        ),
        CatalogSourceConfig::Local {
            path: "/fixture".into()
        }
    );
    assert_eq!(
        CatalogSourceConfig::from_values(None, Some("false"), None, || Some("/legacy".into())),
        CatalogSourceConfig::Local {
            path: "/legacy".into()
        }
    );
    assert_eq!(
        CatalogSourceConfig::from_values(None, Some("off"), None, || None),
        CatalogSourceConfig::Disabled
    );
    for repo in [
        "",
        " ",
        "./..",
        "owner/..",
        "owner/repo ",
        "owner/repo/extra",
        "owner/repo@tag",
        "owner/repo~name",
    ] {
        assert_eq!(
            CatalogSourceConfig::from_values(None, None, Some(repo), || None),
            CatalogSourceConfig::Disabled,
            "{repo}"
        );
    }
    assert!(is_valid_repo(&format!("a/{}", "b".repeat(98))));
    assert!(!is_valid_repo(&format!("a/{}", "b".repeat(99))));
    for repo in [
        "Valid-owner/repo_name.git",
        "owner_/repo",
        "-owner/repo",
        "owner-/repo",
        "two--parts/repo",
        "owner.name/repo",
    ] {
        assert!(is_valid_repo(repo), "{repo}");
    }
    // ASCII 名称边界与发布工具保持一致，缓存编码不得因下划线产生碰撞。
    assert_ne!(sanitize_repo("a_/b"), sanitize_repo("a/_b"));
    assert_ne!(sanitize_repo("a__b/c"), sanitize_repo("a/b__c"));
}

#[test]
fn compatible_catalog_manifests_keep_strict_schema_permissions_and_settings() {
    let entry = package_entry(test_manifest("strict", "1.0.0", 2, "0.1.0"), b"package");
    let base: serde_json::Value = serde_json::from_slice(&catalog_bytes(vec![entry])).unwrap();
    for field in [
        "author",
        "min_host_version",
        "permissions",
        "settings",
        "name",
        "entry",
    ] {
        let mut value = base.clone();
        value["entries"][0]["manifest"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&value).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err(),
            "缺少 {field}"
        );
    }
    for (field, replacement) in [
        (
            "unknown",
            serde_json::json!("https://signed.invalid/?secret=not-for-errors"),
        ),
        (
            "permissions",
            serde_json::json!(["files.read", "files.read"]),
        ),
        ("permissions", serde_json::json!(["system.exec"])),
        ("author", serde_json::json!("")),
        ("api_version", serde_json::json!(0)),
        (
            "settings",
            serde_json::json!([{"key":"note", "label":"说明", "type":"boolean", "default":"错误类型"}]),
        ),
        (
            "settings",
            serde_json::json!([{"key":"__proto__", "label":"说明", "type":"string", "default":""}]),
        ),
        (
            "settings",
            serde_json::json!([{"key":"note", "label":"说明", "type":"string", "default":"", "unknown":true}]),
        ),
    ] {
        let mut value = base.clone();
        value["entries"][0]["manifest"][field] = replacement;
        let error = CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&value).unwrap(),
            "lengyuesky/tgdrive-app",
        )
        .unwrap_err();
        assert!(!error.to_string().contains("signed.invalid"));
    }
    for size in [0, MAX_PACKAGE_SIZE + 1] {
        let mut value = base.clone();
        value["entries"][0]["size"] = serde_json::json!(size);
        assert!(
            CatalogManager::parse_catalog_bytes(
                &serde_json::to_vec(&value).unwrap(),
                "lengyuesky/tgdrive-app"
            )
            .is_err()
        );
    }
    let entries = (0..=MAX_ENTRIES)
        .map(|index| {
            package_entry(
                test_manifest(&format!("app-{index}"), "1.0.0", 2, "0.1.0"),
                b"x",
            )
        })
        .collect();
    assert!(
        CatalogManager::parse_catalog_bytes(&catalog_bytes(entries), "lengyuesky/tgdrive-app")
            .unwrap_err()
            .to_string()
            .contains("512")
    );
}

#[tokio::test]
async fn http_redirects_validate_every_hop_and_never_request_forbidden_targets() {
    let repo = "lengyuesky/tgdrive-app";
    let initial = format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
    for forbidden in [
        "http://raw.githubusercontent.com/asset",
        "https://username:password@raw.githubusercontent.com/asset",
        "https://raw.githubusercontent.com:8443/asset",
        "https://raw.githubusercontent.com/asset#fragment",
        "https://evil.invalid/asset?secret=hidden",
        "https://github.com/lengyuesky/tgdrive-app/releases/download/v1.0.0/catalog.json",
        "https://objects.githubusercontent.com/asset",
        "https://release-assets.githubusercontent.com/asset",
        "https://raw.githubusercontent.com/other/repo/main/catalog.json",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/dev/catalog.json",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/arbitrary.txt",
        "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json?credential=hidden",
    ] {
        let transport = ScriptedTransport::new(vec![http_response(302, Some(forbidden), vec![])]);
        let error = transport
            .fetch(
                &initial,
                repo,
                &ProxyConfig::default(),
                Duration::from_secs(1),
                100,
            )
            .await
            .unwrap_err();
        assert_eq!(
            transport.urls.lock().unwrap().as_slice(),
            std::slice::from_ref(&initial)
        );
        assert!(!error.to_string().contains(forbidden));
        assert!(!error.to_string().contains("hidden"));
    }
    let transport = ScriptedTransport::new(vec![]);
    assert!(
        transport
            .fetch(
                "https://raw.githubusercontent.com/other/repo/main/catalog.json",
                repo,
                &ProxyConfig::default(),
                Duration::from_secs(1),
                100
            )
            .await
            .is_err()
    );
    assert!(transport.urls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn http_redirect_limit_allows_five_hops_and_rejects_sixth_hop() {
    let repo = "lengyuesky/tgdrive-app";
    let initial = format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
    let target = "/lengyuesky/tgdrive-app/main/catalog.json";
    let mut replies: Vec<_> = (0..5)
        .map(|_| http_response(302, Some(target), vec![]))
        .collect();
    replies.push(http_response(200, None, b"complete".to_vec()));
    let transport = ScriptedTransport::new(replies);
    assert_eq!(
        transport
            .fetch(
                &initial,
                repo,
                &ProxyConfig::default(),
                Duration::from_secs(1),
                100
            )
            .await
            .unwrap(),
        b"complete"
    );
    {
        let urls = transport.urls.lock().unwrap();
        assert_eq!(urls.len(), 6);
    }
    let transport = ScriptedTransport::new(
        (0..6)
            .map(|_| http_response(302, Some(target), vec![]))
            .collect(),
    );
    let error = transport
        .fetch(
            &initial,
            repo,
            &ProxyConfig::default(),
            Duration::from_secs(1),
            100,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("5 次"));
    assert_eq!(transport.urls.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn http_total_deadline_includes_all_redirects_and_body_stream() {
    let repo = "lengyuesky/tgdrive-app";
    let initial = format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
    let calls = AtomicUsize::new(0);
    let result = fetch_http(&initial, repo, Duration::from_millis(220), 100, |_| {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        async move {
            tokio::time::sleep(Duration::from_millis(90)).await;
            Ok(if call < 2 {
                http_response(
                    302,
                    Some("/lengyuesky/tgdrive-app/main/catalog.json"),
                    vec![],
                )
            } else {
                http_response(200, None, b"late".to_vec())
            })
        }
    })
    .await;
    assert!(result.unwrap_err().to_string().contains("总超时"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let response = http::Response::new(reqwest::Body::wrap_stream(futures_util::stream::unfold(
        0,
        |index| async move {
            if index == 3 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(90)).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"x")),
                index + 1,
            ))
        },
    )));
    let mut response = Some(reqwest::Response::from(response));
    let result = fetch_http(&initial, repo, Duration::from_millis(220), 100, |_| {
        std::future::ready(Ok(response.take().unwrap()))
    })
    .await;
    assert!(result.unwrap_err().to_string().contains("总超时"));
}

#[tokio::test]
async fn http_stream_limits_size_and_sanitizes_body_and_request_errors() {
    let repo = "lengyuesky/tgdrive-app";
    let initial = format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
    let chunks = Arc::new(AtomicUsize::new(0));
    let counted = chunks.clone();
    let stream =
        futures_util::stream::iter([b"123".as_slice(), b"456", b"789"]).map(move |bytes| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(bytes))
        });
    let response = reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)));
    let transport = ScriptedTransport::new(vec![response]);
    assert!(
        transport
            .fetch(
                &initial,
                repo,
                &ProxyConfig::default(),
                Duration::from_secs(1),
                4
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("大小上限")
    );
    assert_eq!(chunks.load(Ordering::SeqCst), 2);
    let stream = futures_util::stream::iter([Err::<bytes::Bytes, _>(std::io::Error::other(
        "https://user:proxy-secret@proxy.invalid https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json",
    ))]);
    let response = reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)));
    let transport = ScriptedTransport::new(vec![
        http_response(
            302,
            Some("/lengyuesky/tgdrive-app/main/catalog.json"),
            vec![],
        ),
        response,
    ]);
    let error = transport
        .fetch(
            &initial,
            repo,
            &ProxyConfig::default(),
            Duration::from_secs(1),
            100,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("读取下载响应失败"));
    assert!(!error.to_string().contains("secret"));
    assert!(!error.to_string().contains("https://"));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut error = Some(
        client
            .get("https://objects.githubusercontent.com/asset?signature=secret")
            .header("bad\nheader", "value")
            .build()
            .unwrap_err(),
    );
    let result = fetch_http(&initial, repo, Duration::from_secs(1), 100, |_| {
        std::future::ready(Err(error.take().unwrap()))
    })
    .await
    .unwrap_err();
    assert!(result.to_string().contains("网络请求失败"));
    assert!(!result.to_string().contains("secret"));
}

#[tokio::test]
async fn catalog_and_package_requests_use_current_proxy_and_distinct_budgets() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = test_manifest("proxy", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&manifest);
    let entry = package_entry(manifest, &archive);
    let transport = Arc::new(ScriptedTransport::new(vec![
        http_response(200, None, catalog_bytes(vec![entry])),
        http_response(200, None, archive),
    ]));
    let proxy = Arc::new(std::sync::RwLock::new(ProxyConfig::default()));
    let provider = proxy.clone();
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into(),
        },
        transport.clone(),
        Arc::new(move || provider.read().unwrap().clone()),
    );
    manager.refresh(true).await;
    *proxy.write().unwrap() =
        ProxyConfig::new(true, "http://user:secret@proxy.invalid:7890").unwrap();
    manager.get_package("proxy", None).await.unwrap();
    let settings = transport.settings.lock().unwrap();
    assert_eq!(settings.len(), 2);
    assert!(!settings[0].0.enabled);
    assert_eq!(settings[0].1, Duration::from_secs(10));
    assert_eq!(settings[0].2, MAX_CATALOG_SIZE);
    assert!(settings[1].0.enabled);
    assert_eq!(settings[1].0.url, proxy.read().unwrap().url);
    assert_eq!(settings[1].1, Duration::from_secs(120));
    assert_eq!(settings[1].2, MAX_PACKAGE_SIZE);
}

#[tokio::test]
async fn refresh_rejects_mutated_known_versions_and_keeps_last_good_catalog() {
    let manifest = test_manifest("immutable", "1.0.0", 2, "0.1.0");
    let original = package_entry(manifest, b"archive");
    let good = catalog_bytes(vec![original.clone()]);
    for field in ["digest", "manifest", "size", "rollback"] {
        let tmp = tempfile::tempdir().unwrap();
        let current = Arc::new(std::sync::RwLock::new(good.clone()));
        let source = current.clone();
        let transport = Arc::new(MockTransport::new(move |_, _| {
            Ok(source.read().unwrap().clone())
        }));
        let manager = CatalogManager::new(
            tmp.path(),
            CatalogSourceConfig::Github {
                repo: "lengyuesky/tgdrive-app".into(),
            },
            transport,
            Arc::new(ProxyConfig::default),
        );
        manager.refresh(true).await;
        let mut changed = original.clone();
        match field {
            "digest" => changed.sha256 = "a".repeat(64),
            "manifest" => changed.manifest.author = "篡改作者".into(),
            "size" => changed.size += 1,
            "rollback" => {
                changed.manifest.version = "0.9.0".into();
                changed.url = format!(
                    "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/{}-{}.tgapp",
                    changed.manifest.id, changed.manifest.version
                );
            }
            _ => unreachable!(),
        }
        *current.write().unwrap() = catalog_bytes(vec![changed]);
        manager.snapshot.write().await.last_network_attempt = None;
        manager.refresh(true).await;
        assert_eq!(manager.status().await.state, "stale", "{field}");
        let err = manager.status().await.error.unwrap();
        assert!(
            err.contains("已知应用") || err.contains("版本倒退"),
            "{field}: {err}"
        );
        let entries = manager.available_apps(&[]).await.1;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].digest, original.sha256);
        assert_eq!(entries[0].manifest.author, original.manifest.author);
        assert_eq!(
            std::fs::read(tmp.path().join("lengyuesky__tgdrive-app/catalog.json")).unwrap(),
            good
        );
    }
}

#[tokio::test]
async fn schema2_new_version_replaces_old_version_and_rejects_old_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let old_manifest = test_manifest("history", "1.9.0", 2, "0.1.0");
    let new_manifest = test_manifest("history", "1.10.0", 2, "0.1.0");
    let old_package = test_archive(&old_manifest);
    let new_package = test_archive(&new_manifest);
    let old = package_entry(old_manifest, &old_package);
    let new = package_entry(new_manifest, &new_package);
    let current = Arc::new(std::sync::RwLock::new(catalog_bytes(vec![old.clone()])));
    let source = current.clone();
    let new_bytes = new_package.clone();
    let transport = Arc::new(MockTransport::new(move |url, _| {
        Ok(if url.ends_with("catalog.json") {
            source.read().unwrap().clone()
        } else {
            new_bytes.clone()
        })
    }));
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into(),
        },
        transport,
        Arc::new(ProxyConfig::default),
    );
    manager.refresh(true).await;
    let confirmed = manager.available_apps(&[]).await.1[0].digest.clone();

    // 在 schema 2 中，允许新版替换旧版
    *current.write().unwrap() = catalog_bytes(vec![new.clone()]);
    manager.snapshot.write().await.last_network_attempt = None;
    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");
    let available = manager.available_apps(&[]).await.1;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.version, "1.10.0");
    assert_eq!(
        available[0].source_url.as_deref(),
        Some("https://github.com/lengyuesky/tgdrive-app/tree/main/apps")
    );

    // 目录更新后，用户按旧版本确认的 digest 在新目录中不再存在，返回 409 Conflict 拒绝安装
    assert!(matches!(
        manager.get_package("history", Some(&confirmed)).await,
        Err(AppError::Conflict(_))
    ));

    // 无 digest 请求获取最新版本
    assert_eq!(
        manager.get_package("history", None).await.unwrap().2,
        new_package
    );

    // 允许目录移除插件
    *current.write().unwrap() = catalog_bytes(vec![]);
    manager.snapshot.write().await.last_network_attempt = None;
    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");
    assert!(manager.available_apps(&[]).await.1.is_empty());
}

#[tokio::test]
async fn cannot_reintroduce_changed_permissions_or_digest_for_same_version() {
    for rollback in [false, true] {
        for changed_digest in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let manifest = test_manifest("history-guard", "1.0.0", 2, "0.1.0");
            let archive = test_archive(&manifest);
            let correct = package_entry(manifest, &archive);
            let mut initial = correct.clone();
            initial.manifest.permissions.clear();
            let good = catalog_bytes(vec![initial.clone()]);
            let current = Arc::new(std::sync::RwLock::new(good.clone()));
            let source = current.clone();
            let transport = Arc::new(MockTransport::new(move |url, _| {
                Ok(if url.ends_with("catalog.json") {
                    source.read().unwrap().clone()
                } else {
                    archive.clone()
                })
            }));
            let manager = CatalogManager::new(
                tmp.path(),
                CatalogSourceConfig::Github {
                    repo: "lengyuesky/tgdrive-app".into(),
                },
                transport.clone(),
                Arc::new(ProxyConfig::default),
            );
            manager.refresh(true).await;
            assert_eq!(manager.status().await.state, "ready");

            let mut bad_entry = correct.clone();
            if rollback {
                bad_entry.manifest.version = "0.9.0".into();
                bad_entry.url = format!(
                    "https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/apps/{}-{}.tgapp",
                    bad_entry.manifest.id, bad_entry.manifest.version
                );
            } else if changed_digest {
                bad_entry.sha256 = "b".repeat(64);
            }
            *current.write().unwrap() = catalog_bytes(vec![bad_entry]);
            manager.snapshot.write().await.last_network_attempt = None;
            manager.refresh(true).await;
            assert_eq!(manager.status().await.state, "stale");
            assert_eq!(manager.snapshot.read().await.entries, vec![initial.clone()]);
            assert_eq!(
                std::fs::read(tmp.path().join("lengyuesky__tgdrive-app/catalog.json")).unwrap(),
                good
            );
        }
    }
}

#[test]
fn local_list_and_download_keep_async_runtime_responsive() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = test_manifest("local-scan", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&manifest);
    let digest = package_entry(manifest, &archive).sha256;
    std::fs::write(tmp.path().join("local-scan.tgapp"), archive).unwrap();
    let manager = CatalogManager::new(
        tmp.path().join("cache"),
        CatalogSourceConfig::Local {
            path: tmp.path().into(),
        },
        Arc::new(HttpCatalogTransport),
        Arc::new(ProxyConfig::default),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        for download in [false, true] {
            // 控制唯一阻塞线程，扫描必须排队等待，而异步心跳仍可前进；不依赖磁盘快慢。
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            started_rx.await.unwrap();
            let scan = async {
                if download {
                    manager
                        .get_package("local-scan", Some(&digest))
                        .await
                        .map(|_| 1)
                } else {
                    Ok(manager.available_apps(&[]).await.1.len())
                }
            };
            tokio::pin!(scan);
            tokio::select! {
                biased;
                _ = &mut scan => panic!("本地扫描没有交给阻塞线程"),
                _ = tokio::task::yield_now() => {}
            }
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            assert_eq!(scan.await.unwrap(), 1);
        }
    });
}

#[tokio::test]
async fn invalid_and_oversized_local_files_and_persistent_catalogs_are_not_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = test_archive(&test_manifest("valid", "1.0.0", 2, "0.1.0"));
    std::fs::write(tmp.path().join("valid.tgapp"), archive).unwrap();
    std::fs::write(tmp.path().join("broken.tgapp"), b"not zip").unwrap();
    let oversized = tmp.path().join("oversized.tgapp");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    assert!(read_bounded_file(&oversized, MAX_PACKAGE_SIZE).is_err());
    assert!(read_bounded_file(tmp.path(), MAX_PACKAGE_SIZE).is_err());
    let local: Vec<_> = CatalogManager::read_local_packages(tmp.path()).collect();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].manifest.id, "valid");
    let repo = tmp.path().join("cache/lengyuesky__tgdrive-app");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::File::create(repo.join("catalog.json"))
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let (manager, transport) =
        remote_manager(&tmp.path().join("cache"), catalog_bytes(vec![]), vec![]);
    assert_eq!(manager.status().await.state, "unavailable");
    assert!(manager.status().await.last_success_at.is_none());
    assert!(!repo.join("catalog.json").exists());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn local_and_cached_reads_reject_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    std::fs::write(&target, b"untouched").unwrap();
    let link = tmp.path().join("link.tgapp");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(read_bounded_file(&link, MAX_PACKAGE_SIZE).is_err());
    assert_eq!(CatalogManager::read_local_packages(tmp.path()).count(), 0);
    assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
}

#[tokio::test]
async fn corrupted_or_oversized_package_cache_is_replaced_only_after_validation() {
    let manifest = test_manifest("cache", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&manifest);
    let entry = package_entry(manifest, &archive);
    for corruption in ["digest", "length", "oversized"] {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, transport) = remote_manager(
            tmp.path(),
            catalog_bytes(vec![entry.clone()]),
            archive.clone(),
        );
        manager.refresh(true).await;
        let directory = tmp.path().join("lengyuesky__tgdrive-app/packages");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.tgapp", entry.sha256));
        match corruption {
            "digest" => std::fs::write(&path, vec![0; archive.len()]).unwrap(),
            "length" => std::fs::write(&path, b"short").unwrap(),
            "oversized" => std::fs::File::create(&path)
                .unwrap()
                .set_len(1024 * 1024 * 1024)
                .unwrap(),
            _ => unreachable!(),
        }
        let package = manager
            .get_package("cache", Some(&entry.sha256))
            .await
            .unwrap();
        assert_eq!(package.2, archive);
        assert_eq!(std::fs::read(&path).unwrap(), archive);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn matching_digest_cache_still_checks_zip_and_cold_download_checks_length() {
    let manifest = test_manifest("cache", "1.0.0", 2, "0.1.0");
    let invalid = b"not a zip".to_vec();
    let entry = package_entry(manifest.clone(), &invalid);
    let tmp = tempfile::tempdir().unwrap();
    let (manager, transport) = remote_manager(
        tmp.path(),
        catalog_bytes(vec![entry.clone()]),
        invalid.clone(),
    );
    manager.refresh(true).await;
    let directory = tmp.path().join("lengyuesky__tgdrive-app/packages");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{}.tgapp", entry.sha256)), &invalid).unwrap();
    assert!(
        manager
            .get_package("cache", Some(&entry.sha256))
            .await
            .unwrap_err()
            .to_string()
            .contains("格式或清单校验失败")
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    let archive = test_archive(&manifest);
    let mut entry = package_entry(manifest, &archive);
    entry.size += 1;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = remote_manager(tmp.path(), catalog_bytes(vec![entry]), archive);
    manager.refresh(true).await;
    assert!(
        manager
            .get_package("cache", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("实际大小")
    );
    assert!(!tmp.path().join("lengyuesky__tgdrive-app/packages").exists());
}

#[tokio::test]
async fn startup_and_package_writes_prune_lru_and_crash_temporary_files_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("lengyuesky__tgdrive-app");
    let packages = repo.join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    for index in 0..9 {
        let file = std::fs::File::create(packages.join(format!("{index:064x}.tgapp"))).unwrap();
        file.set_len(MAX_PACKAGE_SIZE as u64).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(index + 1))
            .unwrap();
    }
    std::fs::write(repo.join(".catalog.tmp.crash"), b"partial").unwrap();
    std::fs::File::create(packages.join(".pkg.tmp.crash"))
        .unwrap()
        .set_len(MAX_PACKAGE_SIZE as u64)
        .unwrap();
    let installed = tmp.path().join("apps");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("runtime.keep"), b"keep").unwrap();
    let manifest = test_manifest("lru", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&manifest);
    let entry = package_entry(manifest, &archive);
    let (manager, _) = remote_manager(tmp.path(), catalog_bytes(vec![entry]), archive);
    assert!(!packages.join(format!("{:064x}.tgapp", 0)).exists());
    assert!(!packages.join(".pkg.tmp.crash").exists());
    assert!(!repo.join(".catalog.tmp.crash").exists());
    manager.refresh(true).await;
    manager.get_package("lru", None).await.unwrap();
    let total: u64 = std::fs::read_dir(&packages)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= MAX_PACKAGE_CACHE_BYTES);
    assert!(!packages.join(format!("{:064x}.tgapp", 1)).exists());
    assert!(packages.join(format!("{:064x}.tgapp", 8)).exists());
    assert_eq!(
        std::fs::read(installed.join("runtime.keep")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn concurrent_package_cache_writes_are_complete_and_leave_no_temporary_files() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = test_manifest("concurrent", "1.0.0", 2, "0.1.0");
    let archive = test_archive(&manifest);
    let entry = package_entry(manifest, &archive);
    let (manager, _) = remote_manager(
        tmp.path(),
        catalog_bytes(vec![entry.clone()]),
        archive.clone(),
    );
    let manager = Arc::new(manager);
    manager.refresh(true).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let manager = manager.clone();
        tasks.spawn(async move { manager.get_package("concurrent", None).await.unwrap() });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap().2, archive);
    }
    let files: Vec<_> = std::fs::read_dir(tmp.path().join("lengyuesky__tgdrive-app/packages"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].file_name().to_str().unwrap(),
        format!("{}.tgapp", entry.sha256)
    );
    assert_eq!(std::fs::read(files[0].path()).unwrap(), archive);
}

#[test]
fn atomic_catalog_replacement_never_exposes_partial_files_and_cleans_failed_write() {
    let tmp = tempfile::tempdir().unwrap();
    let first = catalog_bytes(vec![]);
    let second = catalog_bytes(vec![package_entry(
        test_manifest("atomic", "1.0.0", 2, "0.1.0"),
        b"x",
    )]);
    persist_file(tmp.path(), "catalog.json", ".catalog.tmp.", &first).unwrap();
    let start = Arc::new(std::sync::Barrier::new(3));
    let mut writers = Vec::new();
    for bytes in [&first, &second] {
        let directory = tmp.path().to_path_buf();
        let bytes = bytes.clone();
        let start = start.clone();
        writers.push(std::thread::spawn(move || {
            start.wait();
            for _ in 0..20 {
                persist_file(&directory, "catalog.json", ".catalog.tmp.", &bytes).unwrap();
            }
        }));
    }
    start.wait();
    for _ in 0..200 {
        let bytes = read_bounded_file(&tmp.path().join("catalog.json"), MAX_CATALOG_SIZE).unwrap();
        assert!(bytes == first || bytes == second);
        CatalogManager::parse_catalog_bytes(&bytes, "lengyuesky/tgdrive-app").unwrap();
    }
    for writer in writers {
        writer.join().unwrap();
    }
    std::fs::create_dir(tmp.path().join("blocked.json")).unwrap();
    assert!(persist_file(tmp.path(), "blocked.json", ".catalog.tmp.", &first).is_err());
    assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .starts_with(".catalog.tmp.")
    }));
    assert!(tmp.path().join("catalog.json").is_file());
}

struct GatedTransport {
    body: Vec<u8>,
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl CatalogTransport for GatedTransport {
    async fn fetch(
        &self,
        _: &str,
        _: &str,
        _: &ProxyConfig,
        _: Duration,
        _: usize,
    ) -> AppResult<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(self.body.clone())
    }
}

#[tokio::test]
async fn refresh_is_single_flight_throttled_and_does_not_lock_snapshot_during_network() {
    let tmp = tempfile::tempdir().unwrap();
    let transport = Arc::new(GatedTransport {
        body: catalog_bytes(vec![]),
        calls: AtomicUsize::new(0),
        started: Default::default(),
        release: Default::default(),
    });
    let manager = Arc::new(CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into(),
        },
        transport.clone(),
        Arc::new(ProxyConfig::default),
    ));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let manager = manager.clone();
        tasks.spawn(async move { manager.refresh(true).await });
    }
    transport.started.notified().await;
    let (status, available) =
        tokio::time::timeout(Duration::from_secs(1), manager.available_apps(&[]))
            .await
            .unwrap();
    assert_eq!(status.state, "unavailable");
    assert!(available.is_empty());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    transport.release.notify_one();
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    manager.refresh(true).await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    manager.snapshot.write().await.last_network_attempt =
        Some(Instant::now() - MIN_CHECK_INTERVAL - Duration::from_secs(1));
    manager.refresh(false).await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    transport.release.notify_one();
    manager.refresh(true).await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    {
        let mut snapshot = manager.snapshot.write().await;
        snapshot.last_network_attempt =
            Some(Instant::now() - MIN_CHECK_INTERVAL - Duration::from_secs(1));
        snapshot.status.last_success_at =
            Some(now_millis() - CATALOG_CACHE_TTL.as_millis() as i64 - 1);
    }
    transport.release.notify_one();
    manager.refresh(false).await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn catalog_schema_requires_objects_instead_of_positional_arrays() {
    let entry = package_entry(test_manifest("shape", "1.0.0", 2, "0.1.0"), b"x");
    let base: serde_json::Value = serde_json::from_slice(&catalog_bytes(vec![entry])).unwrap();
    let top_array = serde_json::json!([2, "lengyuesky/tgdrive-app", base["entries"]]);
    assert!(
        CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&top_array).unwrap(),
            "lengyuesky/tgdrive-app"
        )
        .is_err()
    );
    let record = &base["entries"][0];
    let mut entry_array = base.clone();
    entry_array["entries"][0] = serde_json::json!([
        record["manifest"],
        record["sha256"],
        record["size"],
        record["url"]
    ]);
    assert!(
        CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&entry_array).unwrap(),
            "lengyuesky/tgdrive-app"
        )
        .is_err()
    );
    let mut settings_array = base.clone();
    settings_array["entries"][0]["manifest"]["settings"] =
        serde_json::json!([["note", "说明", "", "string", "默认值"]]);
    assert!(
        CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&settings_array).unwrap(),
            "lengyuesky/tgdrive-app"
        )
        .is_err()
    );
    let duplicate_field = serde_json::to_string(&base).unwrap().replace(
        "\"schema_version\":2",
        "\"schema_version\":2,\"schema_version\":2",
    );
    assert!(
        CatalogManager::parse_catalog_bytes(duplicate_field.as_bytes(), "lengyuesky/tgdrive-app")
            .is_err()
    );
    let mut future_array = base;
    future_array["entries"][0]["manifest"] = serde_json::json!(["shape", "1.0.0", 99, "0.1.0"]);
    assert!(
        CatalogManager::parse_catalog_bytes(
            &serde_json::to_vec(&future_array).unwrap(),
            "lengyuesky/tgdrive-app"
        )
        .is_err()
    );
}

#[tokio::test]
async fn legacy_schema_1_cache_is_safely_ignored_and_refreshed_to_schema_2() {
    let repo = "lengyuesky/tgdrive-app";
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join(sanitize_repo(repo));
    std::fs::create_dir_all(&repo_dir).unwrap();

    // 写入旧版 schema 1 缓存
    let legacy_cache = serde_json::json!({
        "schema_version": 1,
        "repository": repo,
        "release_tag": "v1.0.0",
        "entries": [
            {
                "manifest": test_manifest("legacy", "1.0.0", 2, "0.1.0"),
                "sha256": "1".repeat(64),
                "size": 100,
                "url": format!("https://github.com/{repo}/releases/download/v1.0.0/legacy-1.0.0.tgapp"),
                "release_tag": "v1.0.0"
            }
        ]
    });
    std::fs::write(
        repo_dir.join("catalog.json"),
        serde_json::to_vec(&legacy_cache).unwrap(),
    )
    .unwrap();

    // 启动时应安全忽略旧缓存，置为 unavailable 且条目为空
    let schema2_payload = catalog_bytes(vec![package_entry(
        test_manifest("modern", "2.0.0", 2, "0.1.0"),
        b"archive",
    )]);
    let schema2_clone = schema2_payload.clone();
    let transport = Arc::new(MockTransport::new(move |_, _| Ok(schema2_clone.clone())));
    let manager = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github { repo: repo.into() },
        transport,
        Arc::new(ProxyConfig::default),
    );

    assert_eq!(manager.status().await.state, "unavailable");
    assert!(manager.available_apps(&[]).await.1.is_empty());

    // 刷新时应正常获取并持久化新 schema 2 协议
    manager.refresh(true).await;
    assert_eq!(manager.status().await.state, "ready");
    let available = manager.available_apps(&[]).await.1;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].manifest.id, "modern");
    assert_eq!(available[0].manifest.version, "2.0.0");
    assert_eq!(
        std::fs::read(repo_dir.join("catalog.json")).unwrap(),
        schema2_payload
    );
}

#[tokio::test]
async fn persistent_catalog_cache_is_isolated_by_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let (first, _) = remote_manager(
        tmp.path(),
        catalog_bytes(vec![package_entry(
            test_manifest("repo", "1.0.0", 2, "0.1.0"),
            b"x",
        )]),
        vec![],
    );
    first.refresh(true).await;
    let second = CatalogManager::new(
        tmp.path(),
        CatalogSourceConfig::Github {
            repo: "other-owner/other-repo".into(),
        },
        Arc::new(MockTransport::new(|_, _| panic!("启动不应联网"))),
        Arc::new(ProxyConfig::default),
    );
    assert_eq!(first.available_apps(&[]).await.1.len(), 1);
    assert!(second.available_apps(&[]).await.1.is_empty());
    assert_eq!(second.status().await.state, "unavailable");
}
