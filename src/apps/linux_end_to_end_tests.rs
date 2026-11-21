//! Linux root 测试构建中的真实静态包与真实 HTTP/Unix HTTP 端到端验证。
//! 网盘内容替身仅来自 cfg(test) 的 tests::Synthetic，发布构建没有此模块。
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use std::path::Path;
use std::sync::Arc;
use tgdrive_contract::{files::*, http::*, runtime::*};

struct Http {
    client: Client,
    origin: String,
    cookie: String,
}
impl Http {
    fn owner(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.origin))
            .header(header::COOKIE, &self.cookie)
    }
    fn page(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{SELF_API_BASE}{path}", self.origin))
            .bearer_auth(token)
    }
}
async fn json<T: DeserializeOwned>(response: Response) -> T {
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    assert!(
        status.is_success(),
        "HTTP {status}：{}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(512)])
    );
    serde_json::from_slice(&bytes).unwrap()
}
async fn install(http: &Http, path: &Path) -> InstalledApp {
    let response = http
        .owner(Method::POST, &format!("{APP_CENTER_BASE}/packages"))
        .header(header::CONTENT_TYPE, "application/zip")
        .body(std::fs::read(path).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let upload: PackageUploadReceipt = json(response).await;
    let preview: InstallPreview = json(
        http.owner(Method::POST, &format!("{APP_CENTER_BASE}/install/preview"))
            .json(&InstallPreviewRequest {
                source: InstallSource::Uploaded {
                    package_id: upload.package_id,
                },
            })
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(preview.starts_backend);
    json(
        http.owner(Method::POST, &format!("{APP_CENTER_BASE}/install"))
            .json(&InstallRequest {
                preview_id: preview.preview_id,
            })
            .send()
            .await
            .unwrap(),
    )
    .await
}
async fn grant(http: &Http, app: &InstalledApp, access: AccessLevel) {
    let scope: ScopeInfo = json(
        http.owner(
            Method::PUT,
            &format!("{APP_CENTER_BASE}/apps/{}/scope", app.manifest.id),
        )
        .json(&ScopeUpdateRequest {
            expected_revision: app.scope.revision.clone(),
            scope: DirectoryScope::Selected {
                directories: vec![DirectoryGrant {
                    directory_id: "1".into(),
                    access,
                }],
            },
        })
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(scope.roots.len(), 1);
    assert_eq!(scope.roots[0].directory_id, "1");
}
async fn launch(http: &Http, id: &str) -> LaunchPageResponse {
    json(
        http.owner(Method::POST, &format!("{APP_CENTER_BASE}/apps/{id}/launch"))
            .json(&LaunchPageRequest { open_node_id: None })
            .send()
            .await
            .unwrap(),
    )
    .await
}
async fn app(http: &Http, id: &str) -> InstalledApp {
    json(
        http.owner(Method::GET, &format!("{APP_CENTER_BASE}/apps/{id}"))
            .send()
            .await
            .unwrap(),
    )
    .await
}
async fn uninstall(http: &Http, id: &str, revision: &str) {
    let response = http
        .owner(
            Method::DELETE,
            &format!("{APP_CENTER_BASE}/apps/{id}?purge_data=true"),
        )
        .header(header::IF_MATCH, format!("\"{revision}\""))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
fn media_request(file: ContentRef) -> MediaUrlRequest {
    MediaUrlRequest {
        file,
        representation: FileRepresentation::Original,
        disposition: Disposition::Inline,
        filename: None,
        content_type: None,
        ttl_seconds: Some(120),
        lease_id: None,
    }
}
async fn ticket_revoked(http: &Http, ticket: &MediaUrl) {
    let response = http
        .client
        .get(format!("{}{}", http.origin, ticket.url))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let response: tgdrive_contract::ErrorResponse = response.json().await.unwrap();
    assert!(!response.error.code.is_empty());
}
async fn example(platform: &Arc<AppPlatform>, fake: &super::tests::Synthetic, http: &Http) {
    let path =
        std::env::var_os("TGDRIVE_TEST_EXAMPLE_PACKAGE").expect("须提供真实示例 Linux 静态包");
    let installed = install(http, Path::new(&path)).await;
    assert_eq!(installed.manifest.id, "example-backend");
    assert_eq!(installed.manifest.version, "1.0.0");
    assert_eq!(installed.scope.scope, DirectoryScope::None);
    grant(http, &installed, AccessLevel::ReadWrite).await;
    let id = super::tests::file(platform, fake, 1, "example.bin", b"0123456789abcdef").await;
    let launched = launch(http, "example-backend").await;
    let page = &launched.bootstrap.session_token;
    let node: FileNode = json(
        http.page(Method::POST, "/files/stat", page)
            .json(&NodeReference::Id {
                id: id.to_string(),
                content_version: None,
            })
            .send()
            .await
            .unwrap(),
    )
    .await;
    let file = ContentRef {
        id: node.id,
        content_version: node.content_version.unwrap(),
    };
    let token = "0123456789abcdef0123456789abcdef";
    let configured: serde_json::Value = json(
        http.page(Method::PUT, "/backend/owner/config", page)
            .json(&serde_json::json!({"file":file,"token":token}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(configured["configured"], true);
    let configuration: serde_json::Value = json(
        http.page(Method::GET, "/backend/owner/config", page)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(configuration["file"]["id"], file.id);
    let run = platform
        .backends
        .run("example-backend")
        .expect("页面请求应触发真实按需启动");
    let backend: serde_json::Value = json(
        http.owner(
            Method::GET,
            &format!("{APP_CENTER_BASE}/apps/example-backend/backend"),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    let status: BackendStatus = serde_json::from_value(backend["status"].clone()).unwrap();
    assert_eq!(status.state, BackendState::Ready);
    assert_eq!(status.run_id.as_deref(), Some(run.caller.id.as_str()));
    println!(
        "端到端：真实 example-backend 1.0.0 静态包经 HTTP 上传/预览/安装、指定目录授权、按需启动就绪及 caller=owner 页面配置通过"
    );
    let spoofed = http
        .client
        .get(format!(
            "{}{SERVICE_BASE}/example-backend/owner/config",
            http.origin
        ))
        .bearer_auth(page)
        .header(
            FORWARD_CONTEXT_HEADER,
            super::backend_tests::forward(&run, ForwardCaller::Owner, ForwardEntry::Page),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(spoofed.status(), StatusCode::NOT_FOUND);
    let raw = Request::builder()
        .uri("/file")
        .header("host", "tgdrive")
        .header("authorization", format!("Bearer {token}"))
        .header(
            FORWARD_CONTEXT_HEADER,
            super::backend_tests::forward(&run, ForwardCaller::External, ForwardEntry::Prefix),
        );
    let controlled = backend::request(&run, raw.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(controlled.status(), StatusCode::NO_CONTENT);
    assert!(controlled.headers().contains_key(STREAM_DIRECTIVE_HEADER));
    assert!(
        to_bytes(controlled.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    let prefix = http
        .client
        .get(format!(
            "{}{SERVICE_BASE}/example-backend/file",
            http.origin
        ))
        .bearer_auth(token)
        .header(header::RANGE, "bytes=3-9")
        .send()
        .await
        .unwrap();
    assert_eq!(prefix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(prefix.headers()[header::CONTENT_RANGE], "bytes 3-9/16");
    assert!(!prefix.headers().contains_key(STREAM_DIRECTIVE_HEADER));
    assert_eq!(prefix.bytes().await.unwrap(), "3456789");
    let selection: RootAppSelection = json(
        http.owner(Method::GET, &format!("{APP_CENTER_BASE}/root-app"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let selected: RootAppSelection = json(
        http.owner(Method::PUT, &format!("{APP_CENTER_BASE}/root-app"))
            .json(&RootAppUpdateRequest {
                expected_revision: selection.revision,
                app_id: Some("example-backend".into()),
            })
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(selected.app_id.as_deref(), Some("example-backend"));
    let root = http
        .client
        .get(format!("{}/file", http.origin))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);
    assert!(!root.headers().contains_key(STREAM_DIRECTIVE_HEADER));
    assert_eq!(
        root.headers()[header::CONTENT_SECURITY_POLICY],
        EXTERNAL_CSP
    );
    assert_eq!(root.bytes().await.unwrap(), "0123456789abcdef");
    let health = http
        .client
        .get(format!("{}/admin/healthz", http.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    println!(
        "端到端：固定前缀保持 caller=external 并拒绝伪造 owner；根地址选择；真实 204 + X-Tgdrive-Stream 转换为 Range/完整原文件字节通过"
    );
    let receipt: MutationReceipt = json(http.page(Method::POST, "/backend/owner/directory", page)
        .json(&serde_json::json!({"parent_id":"1","name":"example-created","idempotency_key":"example-e2e-directory-key-0001"})).send().await.unwrap()).await;
    assert_eq!(receipt.results.len(), 1);
    assert!(
        platform
            .host
            .vfs
            .db
            .child(1, "example-created")
            .await
            .unwrap()
            .is_some()
    );
    let page_ticket: MediaUrl = json(
        http.page(Method::POST, "/files/media-url", page)
            .json(&media_request(file.clone()))
            .send()
            .await
            .unwrap(),
    )
    .await;
    // 示例只输出直传指令；补充使用同一生产签发函数验证该真实启动轮次的媒体票据撤销。
    let backend_ticket = platform
        .media_url(&run.caller, media_request(file), &http.origin)
        .await
        .unwrap();
    for ticket in [&page_ticket, &backend_ticket] {
        let response = http
            .client
            .get(format!("{}{}", http.origin, ticket.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap(), "0123456789abcdef");
    }
    let output = tempfile::tempdir().unwrap();
    assert!(
        snapshot::snapshot(
            &platform.host.vfs.db,
            "example-backend",
            Path::new(&run.data_dir),
            &output.path().join("app.sqlite3")
        )
        .await
        .unwrap()
    );
    assert!(!run.caller.cancelled.is_cancelled());
    println!(
        "端到端：真实后端经 SDK 提交带目录修订断言的新建目录；页面/后端轮次媒体票据可读；运行中应用库在线快照通过"
    );
    let installed = app(http, "example-backend").await;
    let disabled: InstalledApp = json(
        http.owner(
            Method::PATCH,
            &format!("{APP_CENTER_BASE}/apps/example-backend/state"),
        )
        .json(&AppStatePatch {
            expected_state_revision: installed.state_revision,
            enabled: Some(false),
            pinned: None,
        })
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert!(run.finished.is_cancelled());
    super::linux_acceptance_tests::gone(run.pid).await;
    for path in ["/file", "/admin/api/apps/v1/services/example-backend/file"] {
        let response = http
            .client
            .get(format!("{}{path}", http.origin))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    ticket_revoked(http, &page_ticket).await;
    ticket_revoked(http, &backend_ticket).await;
    let stale_page = http
        .page(Method::GET, "/runtime", page)
        .send()
        .await
        .unwrap();
    assert!(stale_page.status().is_client_error());
    uninstall(http, "example-backend", &disabled.state_revision).await;
    assert!(!Path::new(&run.data_dir).exists());
    let selection: RootAppSelection = json(
        http.owner(Method::GET, &format!("{APP_CENTER_BASE}/root-app"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(selection.app_id.is_none());
    let fallback = http
        .client
        .get(format!("{}/file", http.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(fallback.status(), StatusCode::FOUND);
    assert_eq!(fallback.headers()[header::LOCATION], "/admin/");
    ticket_revoked(http, &page_ticket).await;
    ticket_revoked(http, &backend_ticket).await;
    println!(
        "端到端：HTTP 停用/卸载回收真实进程、页面会话和旧票据，清除根应用及私有数据，根地址恢复 302 → /admin/ 通过"
    );
}
async fn cinema(platform: &Arc<AppPlatform>, http: &Http) {
    let Some(path) = std::env::var_os("TGDRIVE_TEST_CINEMA_PACKAGE") else {
        println!("端到端：影视实包未提供，本轮不执行其安装/启动/就绪验收");
        return;
    };
    let installed = install(http, Path::new(&path)).await;
    assert_eq!(installed.manifest.id, "cinema");
    grant(http, &installed, AccessLevel::ReadOnly).await;
    let launched = launch(http, "cinema").await;
    let configuration: serde_json::Value = json(
        http.page(
            Method::GET,
            "/backend/owner/config",
            &launched.bootstrap.session_token,
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert!(configuration.is_object());
    let run = platform
        .backends
        .run("cinema")
        .expect("影视真实后端应已经启动就绪");
    let management: serde_json::Value = json(
        http.owner(
            Method::GET,
            &format!("{APP_CENTER_BASE}/apps/cinema/backend"),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    let status: BackendStatus = serde_json::from_value(management["status"].clone()).unwrap();
    assert_eq!(status.state, BackendState::Ready);
    let installed = app(http, "cinema").await;
    uninstall(http, "cinema", &installed.state_revision).await;
    super::linux_acceptance_tests::gone(run.pid).await;
    assert!(run.caller.cancelled.is_cancelled());
    println!(
        "端到端：影视真实静态包 HTTP 安装、目录只读授权、后端启动就绪、页面配置读取及卸载回收通过"
    );
}
#[tokio::test]
#[ignore = "须在独立 Linux root 容器显式执行，并提供经过核验的真实 Linux 静态应用包"]
async fn linux_static_packages_end_to_end() {
    let _isolation = super::linux_acceptance_tests::RealIsolation::new();
    let (_data, platform, fake) = super::tests::fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = crate::http_router_with_platform(platform.host.clone(), None, platform.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Server(server);
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let origin = format!("http://{address}");
    let login = client
        .post(format!("{origin}/admin/api/auth/login"))
        .json(&serde_json::json!({"username":"test","password":"test-password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let http = Http {
        client,
        origin,
        cookie,
    };
    example(&platform, &fake, &http).await;
    cinema(&platform, &http).await;
    platform.host.restore.shutdown.cancel();
    platform.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "仅供独立 Linux 容器中的真实影视浏览器回归，必须显式提供静态页面和已核验应用包"]
async fn linux_cinema_library_browser() {
    let _isolation = super::linux_acceptance_tests::RealIsolation::new();
    let (_data, platform, _) = super::tests::fixture().await;
    let movie = platform.host.vfs.db.mkdir(1, "电影").await.unwrap();
    let series = platform.host.vfs.db.mkdir(1, "电视剧").await.unwrap();
    platform.host.vfs.db.mkdir(1, "私人文件").await.unwrap();
    let package = std::env::var_os("TGDRIVE_TEST_CINEMA_PACKAGE").unwrap();
    let staged = _data.path().join("cinema-input.tgapp");
    std::fs::copy(package, &staged).unwrap();
    let uploaded = platform.upload_package(&staged).await.unwrap();
    let preview = platform
        .preview(InstallPreviewRequest {
            source: InstallSource::Uploaded {
                package_id: uploaded.package_id,
            },
        })
        .await
        .unwrap();
    let installed = platform
        .install(InstallRequest {
            preview_id: preview.preview_id,
        })
        .await
        .unwrap();
    assert_eq!(installed.manifest.id, "cinema");
    assert_eq!(installed.scope.scope, DirectoryScope::None);
    let static_directory = std::env::var_os("TGDRIVE_TEST_STATIC_DIR").unwrap();
    let router = crate::http_router_with_platform(
        platform.host.clone(),
        Some(Path::new(&static_directory)),
        platform.clone(),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let ready = std::env::var_os("TGDRIVE_TEST_BROWSER_READY").unwrap();
    std::fs::write(ready, b"ready\n").unwrap();
    let done = std::env::var_os("TGDRIVE_TEST_BROWSER_DONE").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(240), async {
        while !Path::new(&done).exists() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let scope = platform.app_scope("cinema").await.unwrap();
    let DirectoryScope::Selected { directories } = scope.scope else {
        panic!("浏览器添加媒体库后只能保留指定目录授权");
    };
    assert_eq!(directories.len(), 2);
    for id in [movie.id, series.id] {
        assert!(directories.iter().any(|grant| {
            grant.directory_id == id.to_string() && grant.access == AccessLevel::ReadOnly
        }));
    }
    let run = platform.backends.run("cinema").unwrap();
    platform.shutdown().await.unwrap();
    super::linux_acceptance_tests::gone(run.pid).await;
    platform.host.restore.shutdown.cancel();
    server.abort();
    println!(
        "浏览器回归：真实影视包、页面 SDK、宿主 HTTP/SQLite 和独立 UID 后端完成媒体库添加；只授权两个选定目录，未授权私人目录；进程已回收"
    );
}
