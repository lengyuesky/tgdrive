use super::*;
use axum::{Json, Router, extract::Request};

pub(crate) struct HttpProxy {
    pub url: String,
    pub requests: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    task: JoinHandle<()>,
}

impl Drop for HttpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 只返回合成 Bot API 响应，不连接 Telegram 或任何真实代理。
pub(crate) async fn http_proxy(name: &'static str) -> HttpProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let app = Router::new().fallback(move |request: Request| {
        let seen = seen.clone();
        async move {
            let uri = request.uri().to_string();
            let auth = request.headers().get("proxy-authorization")
                .and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
            seen.lock().unwrap().push((uri, auth));
            if name == "rejected" {
                return Json(serde_json::json!({"ok": false, "error_code": 401, "description": "测试拒绝连接"}));
            }
            let result = match request.uri().path().rsplit('/').next().unwrap_or_default() {
                "getMe" => serde_json::json!({"id": 1, "username": name}),
                "getChat" => serde_json::json!({"type": "channel"}),
                "getChatMember" => serde_json::json!({"status": "administrator", "can_post_messages": true, "can_delete_messages": true}),
                "getUpdates" => serde_json::json!([]),
                _ => serde_json::json!(true),
            };
            Json(serde_json::json!({"ok": true, "result": result}))
        }
    });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    HttpProxy {
        url,
        requests,
        task,
    }
}

#[test]
fn validates_protocol_host_port_and_credentials_without_echoing_secrets() {
    for raw in [
        "http://localhost:7890",
        "http://proxy.example",
        "http://[::1]:8080",
        "socks5://proxy.example:1080",
        "socks5h://u%40ser:p%3Aass@proxy.example:1080",
        "socks://proxy.example",
        "  http://proxy.example:7890/  ",
    ] {
        assert!(ProxyConfig::new(true, raw).is_ok(), "{raw}");
    }
    assert_eq!(
        ProxyConfig::new(true, "socks://proxy.example").unwrap().url,
        "socks5h://proxy.example:1080"
    );
    assert!(ProxyConfig::new(false, "").is_ok());
    assert!(ProxyConfig::new(true, "").is_err());
    for raw in [
        "proxy.example:7890",
        "https://proxy.example",
        "ftp://proxy.example",
        "socks4://proxy.example",
        "http://",
        "http://proxy.example:0",
        "socks5://proxy.example:65536",
        "http://proxy.example/path",
        "http://proxy.example?x=1",
        "http://proxy.example#fragment",
        "http://user:private-secret@proxy.example/path",
        "http://:private-secret@proxy.example",
        "socks5://user@proxy.example:1080",
        "http://user:private-secret\n@proxy.example",
    ] {
        let error = ProxyConfig::new(true, raw)
            .err()
            .expect("应拒绝无效代理")
            .to_string();
        assert!(!error.contains("private-secret"));
    }
}

#[test]
fn redacted_credentials_roundtrip_but_cannot_be_saved_for_another_host() {
    let config = ProxyConfig::new(
        true,
        "socks5h://private-user:private-secret@proxy.example:1080",
    )
    .unwrap();
    let masked = mask_url(&config.url);
    assert_eq!(masked, "socks5h://***:***@proxy.example:1080");
    assert!(config.patched(Some(false), Some(&masked)).unwrap().url == config.url);
    assert!(
        config
            .patched(None, Some(&masked.replace("proxy.example", "new.example")))
            .is_err()
    );
    assert_eq!(mask_url(""), "");
    assert_eq!(
        mask_url("http://proxy.example:7890/"),
        "http://proxy.example:7890/"
    );
    assert_eq!(mask_url("无效敏感内容"), "[已脱敏]");
    assert!(
        config
            .patched(Some(false), Some(""))
            .unwrap()
            .url
            .is_empty()
    );
}

#[tokio::test]
async fn http_requests_use_proxy_auth_and_hot_updates_preserve_old_snapshots() {
    let first = http_proxy("first").await;
    let second = http_proxy("second").await;
    let url = first.url.replacen("http://", "http://u%40ser:p%3Aass@", 1);
    let network = ProxyClient::new(ProxyConfig::new(true, &url).unwrap()).unwrap();
    let old = network.current();
    let target = "http://telegram-proxy-test.invalid/bot-test/getMe";
    let result: serde_json::Value = old
        .http
        .get(target)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["result"]["username"], "first");
    let seen = first.requests.lock().unwrap().clone();
    assert_eq!(seen[0].0, target);
    assert_eq!(
        seen[0].1,
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u@ser:p:ass")
        )
    );

    network.install(ProxySnapshot::new(ProxyConfig::new(true, &second.url).unwrap()).unwrap());
    let result: serde_json::Value = network
        .current()
        .http
        .get(target)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["result"]["username"], "second");
    let result: serde_json::Value = old
        .http
        .get(target)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["result"]["username"], "first");

    // 本地服务始终直连；关闭开关后也不会再经过此前配置的代理。
    let local = format!("{}/getMe", first.url);
    network.current().http.get(&local).send().await.unwrap();
    network.install(ProxySnapshot::new(ProxyConfig::default()).unwrap());
    network.current().http.get(&local).send().await.unwrap();
    assert_eq!(second.requests.lock().unwrap().len(), 1);
    let seen = first.requests.lock().unwrap();
    assert_eq!(seen[2], ("/getMe".into(), "".into()));
    assert_eq!(seen[3], ("/getMe".into(), "".into()));
}

async fn read_header(stream: &mut TcpStream) -> String {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 8192);
        header.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(header).unwrap()
}

async fn accept_socks(stream: &mut TcpStream, auth: bool) -> (String, u16) {
    assert_eq!(stream.read_u8().await.unwrap(), 5);
    let len = stream.read_u8().await.unwrap() as usize;
    let mut methods = vec![0; len];
    stream.read_exact(&mut methods).await.unwrap();
    let method = if auth { 2 } else { 0 };
    assert!(methods.contains(&method));
    stream.write_all(&[5, method]).await.unwrap();
    if auth {
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        let len = stream.read_u8().await.unwrap() as usize;
        let mut username = vec![0; len];
        stream.read_exact(&mut username).await.unwrap();
        let len = stream.read_u8().await.unwrap() as usize;
        let mut password = vec![0; len];
        stream.read_exact(&mut password).await.unwrap();
        assert_eq!(username, b"u@ser");
        assert_eq!(password, b"p:ass");
        stream.write_all(&[1, 0]).await.unwrap();
    }
    let mut request = [0; 4];
    stream.read_exact(&mut request).await.unwrap();
    assert_eq!(request[..3], [5, 1, 0]);
    let host = match request[3] {
        1 => {
            let mut ip = [0; 4];
            stream.read_exact(&mut ip).await.unwrap();
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let len = stream.read_u8().await.unwrap() as usize;
            let mut domain = vec![0; len];
            stream.read_exact(&mut domain).await.unwrap();
            String::from_utf8(domain).unwrap()
        }
        _ => panic!("测试目标应为 IPv4 或域名"),
    };
    let port = stream.read_u16().await.unwrap();
    stream
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
        .await
        .unwrap();
    (host, port)
}

#[tokio::test]
async fn socks_http_requests_support_local_and_remote_dns_with_optional_auth() {
    for (scheme, host, auth) in [
        ("socks5", "198.51.100.7", false),
        ("socks5", "198.51.100.7", true),
        ("socks5h", "telegram-proxy-test.invalid", false),
        ("socks5h", "telegram-proxy-test.invalid", true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let credentials = if auth { "u%40ser:p%3Aass@" } else { "" };
        let config = ProxyConfig::new(
            true,
            &format!("{scheme}://{credentials}{}", listener.local_addr().unwrap()),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(accept_socks(&mut stream, auth).await, (host.into(), 80));
            let request = read_header(&mut stream).await;
            assert!(request.starts_with("GET /test HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                )
                .await
                .unwrap();
        });
        let client = config
            .http_builder()
            .unwrap()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        assert_eq!(
            client
                .get(format!("http://{host}/test"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "proxied"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn mtproto_tunnels_support_http_connect_and_socks5_without_losing_early_bytes() {
    for scheme in ["http", "socks5", "socks5h"] {
        for auth in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let credentials = if auth { "u%40ser:p%3Aass@" } else { "" };
            let config = ProxyConfig::new(
                true,
                &format!("{scheme}://{credentials}{}", listener.local_addr().unwrap()),
            )
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                if scheme == "http" {
                    let request = read_header(&mut stream).await;
                    assert!(request.starts_with("CONNECT 203.0.113.8:443 HTTP/1.1\r\n"));
                    if auth {
                        let encoded =
                            base64::engine::general_purpose::STANDARD.encode("u@ser:p:ass");
                        assert!(
                            request.contains(&format!("Proxy-Authorization: Basic {encoded}\r\n"))
                        );
                    } else {
                        assert!(!request.contains("Proxy-Authorization"));
                    }
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nready")
                        .await
                        .unwrap();
                } else {
                    assert_eq!(
                        accept_socks(&mut stream, auth).await,
                        ("203.0.113.8".into(), 443)
                    );
                    stream.write_all(b"ready").await.unwrap();
                }
                let mut input = [0; 4];
                stream.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"ping");
                stream.write_all(b"pong").await.unwrap();
            });
            let tunnel = DcTunnel::new(&config, "203.0.113.8:443".parse().unwrap()).unwrap();
            let address = tunnel.address;
            tokio::time::timeout(Duration::from_secs(3), async {
                let mut client = TcpStream::connect(address).await.unwrap();
                let mut ready = [0; 5];
                client.read_exact(&mut ready).await.unwrap();
                assert_eq!(&ready, b"ready");
                client.write_all(b"ping").await.unwrap();
                let mut reply = [0; 4];
                client.read_exact(&mut reply).await.unwrap();
                assert_eq!(&reply, b"pong");
                server.await.unwrap();
            })
            .await
            .unwrap();
            drop(tunnel);
            tokio::task::yield_now().await;
            assert!(TcpStream::connect(address).await.is_err());
        }
    }
}

#[tokio::test]
async fn rejected_http_tunnels_never_expose_the_proxy_response() {
    for response in [
        "HTTP/1.1 407 private-secret\r\n\r\n",
        "HTTP/1.1 403 private-secret\r\n\r\n",
        "无效 private-secret\r\n\r\n",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse(&format!(
            "http://user:private-secret@{}",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_header(&mut stream).await;
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let error = connect_tunnel(&url, "203.0.113.8:443".parse().unwrap())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("private-secret"));
        server.await.unwrap();
    }
}

async fn app_fixture(
    proxy: ProxyConfig,
    configured: bool,
) -> (crate::api::AppState, tempfile::TempDir) {
    use crate::auth::Auth;
    use crate::config::Config;
    use crate::db::Db;
    use crate::engine::upload::{UploadKnobs, Uploader};
    use crate::settings::{Runtime, RuntimeCfg};
    use crate::telegram::TgRegistry;
    use crate::vfs::Vfs;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("proxy.db").to_str().unwrap().to_string();
    let db = Db::open(&db_path).await.unwrap();
    let env = Arc::new(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        bot_token: if configured {
            "main-test".into()
        } else {
            String::new()
        },
        upload_bot_tokens: if configured {
            vec!["upload-test".into()]
        } else {
            vec![]
        },
        chat_id: if configured {
            "-100123".into()
        } else {
            String::new()
        },
        api_base: "http://telegram-proxy-test.invalid".into(),
        proxy,
        chunk_size: 1024 * 1024,
        upload_concurrency: 1,
        download_concurrency: 4,
        db_path,
        tmp_dir: dir.path().to_path_buf(),
        local_data_root: dir.path().to_path_buf(),
        username: "proxy-test".into(),
        password: "proxy-test-only".into(),
        secret: None,
        cookie_secure: false,
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: false,
    });
    let mut cfg = RuntimeCfg::load(&db, &env).await.unwrap();
    for token in cfg.all_upload_bot_tokens() {
        let target = db
            .ensure_storage_target(&cfg.api_base, &token, &cfg.chat_id)
            .await
            .unwrap();
        cfg.upload_target_ids.push(target.id);
    }
    cfg.storage_target_id = cfg.upload_target_ids.first().copied();
    if configured {
        db.ensure_storage_target(&cfg.api_base, "historical-test", "-100456")
            .await
            .unwrap();
    }
    let tg = Arc::new(
        TgRegistry::new(
            &db.storage_targets().await.unwrap(),
            cfg.storage_target_id,
            cfg.upload_target_ids.clone(),
            None,
            cfg.proxy.clone(),
        )
        .unwrap(),
    );
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        cfg.tmp_dir.clone(),
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
        env,
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
    let state = crate::api::AppState {
        dav: crate::dav::build_handler(vfs.clone()),
        vfs,
        auth,
        rt,
        attempts: Default::default(),
        tasks: crate::tasks::TaskManager::new(),
        migration: crate::migration::StorageMigrationManager::new(db, tg),
        apps_catalog: Arc::new(crate::apps::CatalogManager::disabled()),
    };
    (state, dir)
}

#[tokio::test]
async fn runtime_proxy_updates_cover_historical_bots_and_rollback_failed_validation() {
    use crate::settings::{ConfigPatch, RuntimeCfg};

    let first = http_proxy("first").await;
    let second = http_proxy("second").await;
    let rejected = http_proxy("rejected").await;
    let env_proxy = ProxyConfig::new(true, &first.url).unwrap();
    let (state, _dir) = app_fixture(env_proxy.clone(), true).await;
    let db = &state.vfs.db;
    let rt = &state.rt;
    let targets = db.storage_targets().await.unwrap();
    let bindings: Vec<_> = targets
        .iter()
        .map(|target| state.vfs.tg.binding(target.id).unwrap())
        .collect();
    assert_eq!(bindings.len(), 3);
    for binding in &bindings {
        assert_eq!(binding.api.get_me().await.unwrap(), "first");
    }
    let outcome = rt
        .apply(ConfigPatch {
            proxy_url: Some(second.url.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(outcome.changed, ["proxy_url"]);
    assert!(!outcome.needs_local_api_restart);
    assert_eq!(outcome.bot.as_deref(), Some("second"));
    for binding in &bindings {
        // 保存前取得的历史句柄也使用新代理，而不是只重建当前主 Bot。
        assert_eq!(binding.api.get_me().await.unwrap(), "second");
    }
    assert_eq!(db.storage_targets().await.unwrap().len(), 3);
    let current_url = rt.current().proxy.url.clone();
    assert_eq!(
        RuntimeCfg::load(db, &rt.env).await.unwrap().proxy.url,
        current_url
    );
    assert_eq!(rt.sources().await.unwrap()["proxy_url"], "db");
    assert!(
        rt.env_export()
            .contains(&format!("TGDRIVE_PROXY_URL={current_url}\n"))
    );
    assert!(rt.env_export().contains("TGDRIVE_PROXY_ENABLED=true\n"));

    let error = rt
        .apply(ConfigPatch {
            proxy_url: Some(rejected.url.clone()),
            ..Default::default()
        })
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("强制保存"));
    for patch in [
        ConfigPatch {
            proxy_enabled: Some(true),
            proxy_url: Some(String::new()),
            force: true,
            ..Default::default()
        },
        ConfigPatch {
            proxy_url: Some("ftp://invalid".into()),
            force: true,
            ..Default::default()
        },
    ] {
        assert!(rt.apply(patch).await.is_err());
    }
    assert_eq!(rt.current().proxy.url, current_url);
    assert_eq!(
        db.setting_get("cfg.proxy_url").await.unwrap().as_deref(),
        Some(current_url.as_str())
    );
    assert_eq!(bindings[0].api.get_me().await.unwrap(), "second");

    rt.apply(ConfigPatch {
        proxy_enabled: Some(false),
        force: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(!rt.current().proxy.enabled);
    assert_eq!(rt.current().proxy.url, current_url);
    assert!(!RuntimeCfg::load(db, &rt.env).await.unwrap().proxy.enabled);
    rt.reset(&["proxy_enabled".into(), "proxy_url".into()])
        .await
        .unwrap();
    assert!(rt.current().proxy == env_proxy);
    assert!(db.setting_get("cfg.proxy_url").await.unwrap().is_none());
    assert!(db.setting_get("cfg.proxy_enabled").await.unwrap().is_none());
    for binding in &bindings {
        assert_eq!(binding.api.get_me().await.unwrap(), "first");
    }
}

async fn api_request(
    state: &crate::api::AppState,
    method: &str,
    path: &str,
    body: serde_json::Value,
    authenticated: bool,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt as _;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json");
    if authenticated {
        let auth = base64::engine::general_purpose::STANDARD.encode("proxy-test:proxy-test-only");
        request = request.header("Authorization", format!("Basic {auth}"));
    }
    let response = crate::api::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn proxy_admin_api_masks_credentials_and_probes_unsaved_settings_without_persisting() {
    use axum::http::StatusCode;
    use serde_json::json;

    let first = http_proxy("first").await;
    let second = http_proxy("second").await;
    let (state, _dir) = app_fixture(ProxyConfig::default(), false).await;
    let raw_url = first
        .url
        .replacen("http://", "http://private-user:private-secret@", 1);
    let save = json!({"proxy_enabled": true, "proxy_url": raw_url});
    assert_eq!(
        api_request(&state, "POST", "/admin/config", save.clone(), false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(!state.rt.current().proxy.enabled);
    assert_eq!(
        api_request(&state, "POST", "/admin/config", save, true)
            .await
            .0,
        StatusCode::OK
    );
    let (status, saved) = api_request(&state, "GET", "/admin/config", json!(null), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["values"]["proxy_enabled"], true);
    assert_eq!(saved["sources"]["proxy_url"], "db");
    assert!(!saved.to_string().contains("private-user"));
    assert!(!saved.to_string().contains("private-secret"));
    let masked = saved["values"]["proxy_url"].as_str().unwrap();
    assert!(masked.contains("***:***@"));
    let persisted_url = state.rt.current().proxy.url.clone();
    assert!(
        state
            .rt
            .env_export()
            .contains("private-user:private-secret@")
    );

    let unsaved = json!({"proxy_enabled": true, "proxy_url": second.url, "bot_token": "probe-test", "chat_id": "-100123"});
    for path in ["/admin/test", "/admin/detect-chats"] {
        let (status, result) = api_request(&state, "POST", path, unsaved.clone(), true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["bot"], "second");
    }
    assert_eq!(state.rt.current().proxy.url, persisted_url);
    assert!(state.rt.current().bot_token.is_empty());
    assert!(first.requests.lock().unwrap().is_empty());
    assert!(
        second
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(uri, _)| uri.ends_with("/getUpdates"))
    );

    let (_, result) = api_request(
        &state,
        "POST",
        "/admin/test",
        json!({
            "proxy_url": masked, "bot_token": "probe-test", "chat_id": "-100123",
        }),
        true,
    )
    .await;
    assert_eq!(result["bot"], "first");
    assert_eq!(
        api_request(
            &state,
            "POST",
            "/admin/config",
            json!({"proxy_enabled": false, "proxy_url": masked}),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!state.rt.current().proxy.enabled);
    assert_eq!(state.rt.current().proxy.url, persisted_url);
    let (status, error) = api_request(
        &state,
        "POST",
        "/admin/config",
        json!({
            "proxy_url": "ftp://private-user:private-secret@invalid", "force": true,
        }),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error.to_string().contains("private-secret"));
    assert_eq!(state.rt.current().proxy.url, persisted_url);
    assert_eq!(
        api_request(
            &state,
            "POST",
            "/admin/config/reset",
            json!({"keys": ["proxy_url", "proxy_enabled"]}),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!state.rt.current().proxy.enabled);
    assert!(state.rt.current().proxy.url.is_empty());
}
