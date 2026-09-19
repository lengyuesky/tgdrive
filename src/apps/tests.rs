use super::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::db::{Db, ROOT_ID};
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::settings::{Runtime, RuntimeCfg};
use crate::telegram::{ByteReader, TgApi, TgMessage, TgRegistry, UploadBody};
use crate::vfs::Vfs;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tower::ServiceExt as _;
use zip::write::SimpleFileOptions;

#[path = "catalog_api_tests.rs"]
mod catalog_api;
#[path = "media_tests.rs"]
mod media;
#[path = "reader_tests.rs"]
mod readers;
#[path = "scope_tests.rs"]
mod scopes;

#[derive(Default)]
struct MemoryMedia {
    files: StdMutex<HashMap<String, Vec<u8>>>,
    ranges: StdMutex<Vec<(u64, u64)>>,
    slow: AtomicBool,
    active: Arc<AtomicUsize>,
    bytes_sent: Arc<AtomicUsize>,
}

struct ActiveRead {
    active: Arc<AtomicUsize>,
    bytes_sent: Arc<AtomicUsize>,
}

impl Drop for ActiveRead {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MemoryMedia {
    fn reader(&self, data: Vec<u8>) -> ByteReader {
        if !self.slow.load(Ordering::SeqCst) {
            return Box::pin(Cursor::new(data));
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let guard = ActiveRead {
            active: self.active.clone(),
            bytes_sent: self.bytes_sent.clone(),
        };
        let stream =
            futures_util::stream::unfold((data, 0, guard), |(data, offset, guard)| async move {
                if offset == data.len() {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let end = (offset + 1024).min(data.len());
                let bytes = bytes::Bytes::copy_from_slice(&data[offset..end]);
                guard.bytes_sent.fetch_add(bytes.len(), Ordering::SeqCst);
                Some((Ok::<_, std::io::Error>(bytes), (data, end, guard)))
            });
        Box::pin(tokio_util::io::StreamReader::new(Box::pin(stream)))
    }
}

#[async_trait::async_trait]
impl TgApi for MemoryMedia {
    async fn send_document(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<&bytes::Bytes>,
        _: &UploadBody,
    ) -> AppResult<TgMessage> {
        Err(AppError::Forbidden("测试环境不发送 Telegram 消息".into()))
    }
    async fn delete_message(&self, _: i64) -> AppResult<()> {
        Ok(())
    }
    async fn get_me(&self) -> AppResult<String> {
        Ok("应用测试存储".into())
    }
    async fn open_chunk_stream(
        &self,
        file_id: &str,
        _: i64,
        _: u64,
        offset: u64,
    ) -> AppResult<ByteReader> {
        let files = self.files.lock().unwrap();
        let file = files.get(file_id).ok_or(AppError::NotFound)?;
        Ok(self.reader(file[offset as usize..].to_vec()))
    }
    async fn open_chunk_range(
        &self,
        file_id: &str,
        _: i64,
        _: u64,
        offset: u64,
        length: u64,
    ) -> AppResult<ByteReader> {
        self.ranges.lock().unwrap().push((offset, length));
        let files = self.files.lock().unwrap();
        let file = files.get(file_id).ok_or(AppError::NotFound)?;
        Ok(self.reader(file[offset as usize..(offset + length) as usize].to_vec()))
    }
}

async fn state_at(path: &std::path::Path, media: Arc<MemoryMedia>) -> AppState {
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
        db_path,
        tmp_dir: path.parent().unwrap().into(),
        local_data_root: path.parent().unwrap().into(),
        username: "apps-test".into(),
        password: "apps-test-only".into(),
        secret: None,
        cookie_secure: false,
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: false,
    });
    let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
    let target = db
        .ensure_storage_target("https://api.example.invalid", "fixture-token", "-1")
        .await
        .unwrap();
    let tg = Arc::new(TgRegistry::with_client(target.id, media));
    let knobs = Arc::new(UploadKnobs::new(1024 * 1024, env.tmp_dir.clone(), 1, false));
    let uploader = Uploader::new(tg.clone(), db.clone(), knobs.clone(), 1024 * 1024);
    let auth = Arc::new(
        Auth::init(&env.username, &env.password, None, false, &db)
            .await
            .unwrap(),
    );
    let rt = Arc::new(Runtime::new(
        db.clone(),
        env,
        cfg,
        tg.clone(),
        knobs,
        auth.clone(),
    ));
    let vfs = Vfs {
        db: db.clone(),
        tg: tg.clone(),
        uploader,
    };
    // 单元测试显式禁用目录，不继承开发机的目录配置或触发生产网络。
    let apps_catalog = Arc::new(crate::apps::CatalogManager::disabled());
    AppState {
        dav: crate::dav::build_handler(vfs.clone()),
        vfs,
        auth,
        rt,
        attempts: Default::default(),
        tasks: crate::tasks::TaskManager::new(),
        migration: crate::migration::StorageMigrationManager::new(db, tg),
        apps_catalog,
        events: crate::events::bus(),
        range_cache: std::sync::Arc::new(std::sync::Mutex::new(crate::apps::RangeCache::default())),
    }
}

fn router(state: &AppState) -> Router {
    Router::new()
        .nest("/api", crate::api::router(state.clone()))
        .layer(middleware::from_fn_with_state(
            false,
            crate::security::response_headers,
        ))
}

async fn fixture() -> (AppState, Arc<MemoryMedia>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let media = Arc::new(MemoryMedia::default());
    (
        state_at(&dir.path().join("test.db"), media.clone()).await,
        media,
        dir,
    )
}

fn manifest() -> Manifest {
    Manifest {
        id: "shorts".into(),
        name: "测试短视频".into(),
        version: "1.0.0".into(),
        api_version: 2,
        min_host_version: "0.1.0".into(),
        description: "自包含测试应用".into(),
        author: "测试作者".into(),
        entry: "index.html".into(),
        icon: None,
        permissions: vec![
            "files.read".into(),
            "media.read".into(),
            "favorites.write".into(),
        ],
        settings: vec![
            package::SettingField {
                key: "source_dir".into(),
                label: "目录".into(),
                description: "短视频目录".into(),
                kind: "directory".into(),
                default: Value::String("/".into()),
            },
            package::SettingField {
                key: "muted".into(),
                label: "静音播放".into(),
                description: "静音测试".into(),
                kind: "boolean".into(),
                default: Value::Bool(true),
            },
        ],
    }
}

fn archive(manifest: &Manifest) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let metadata = serde_json::to_vec(manifest).unwrap();
    for (name, bytes) in [
        ("app.json", metadata.as_slice()),
        (
            "index.html",
            b"<!DOCTYPE html><html><body>Shorts Test</body></html>".as_slice(),
        ),
        ("app.js", b"console.log('shorts');".as_slice()),
        ("style.css", b"body { margin: 0; }".as_slice()),
        ("tgdrive-sdk.js", b"window.tgdrive = {};".as_slice()),
    ] {
        zip.start_file(
            name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

async fn add_app(state: &AppState, manifest: &Manifest, expected: Option<String>) -> InstalledApp {
    let package = AppPackage::parse(archive(manifest)).unwrap();
    let options = InstallOptions {
        digest: package.digest.clone(),
        expected_revision: expected,
    };
    install_package(state, package, options, "import")
        .await
        .unwrap()
}

async fn call(state: &AppState, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn launch_app(state: &AppState, id: &str) -> Value {
    let (status, response) =
        call(state, "POST", &format!("/api/apps/{id}/launch"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    response
}

async fn rpc_call(
    state: &AppState,
    id: &str,
    session: &str,
    method: &str,
    params: Value,
) -> (StatusCode, Value) {
    call(
        state,
        "POST",
        &format!("/api/apps/{id}/rpc"),
        json!({"session": session, "method": method, "params": params}),
    )
    .await
}

async fn seed_file(
    state: &AppState,
    media: &MemoryMedia,
    path: &str,
    bytes: &[u8],
    chunk_size: usize,
) -> i64 {
    let (parent, name) = state.vfs.resolve_parent(path).await.unwrap();
    let id = state
        .vfs
        .db
        .create_uploading(parent.id, &name)
        .await
        .unwrap();
    let target = state.vfs.db.storage_targets().await.unwrap()[0].id;
    for (idx, part) in bytes.chunks(chunk_size).enumerate() {
        let fid = format!("{id}-{idx}");
        media
            .files
            .lock()
            .unwrap()
            .insert(fid.clone(), part.to_vec());
        state
            .vfs
            .db
            .add_chunk(
                id,
                idx as i64,
                part.len() as i64,
                &fid,
                id * 100 + idx as i64,
                target,
                None,
            )
            .await
            .unwrap();
    }
    state
        .vfs
        .db
        .finalize_upload(id, bytes.len() as i64, None)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn lifecycle_preserves_settings_and_revokes_old_pages() {
    let (state, _, _dir) = fixture().await;
    assert!(state.vfs.db.apps().await.unwrap().is_empty());
    let app = add_app(&state, &manifest(), None).await;
    save_settings(&state, &app, json!({"source_dir": "/视频", "muted": false}))
        .await
        .unwrap();
    let first = launch_app(&state, "shorts").await;
    let session = first["session"].as_str().unwrap();
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(first["frame_url"].as_str().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .starts_with("sandbox allow-scripts;")
    );
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    state
        .vfs
        .db
        .set_app_state("shorts", None, Some(true))
        .await
        .unwrap();
    assert_eq!(
        rpc_call(&state, "shorts", session, "app.ping", json!({}))
            .await
            .0,
        StatusCode::OK
    );
    state
        .vfs
        .db
        .set_app_state("shorts", Some(false), None)
        .await
        .unwrap();
    assert_eq!(
        rpc_call(&state, "shorts", session, "app.ping", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    state
        .vfs
        .db
        .set_app_state("shorts", Some(true), None)
        .await
        .unwrap();
    assert_eq!(
        rpc_call(&state, "shorts", session, "app.ping", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&state, "DELETE", "/api/apps/shorts", json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let reinstalled = add_app(&state, &manifest(), None).await;
    assert_eq!(
        read_settings(&state, &reinstalled).await.unwrap()["source_dir"],
        "/视频"
    );
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/api/apps/shorts?purge_data=true",
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let clean = add_app(&state, &manifest(), None).await;
    assert_eq!(
        read_settings(&state, &clean).await.unwrap()["source_dir"],
        "/"
    );
    assert!(!clean.pinned);
}

#[tokio::test]
async fn update_is_atomic_and_requires_reviewed_digest_revision_and_newer_version() {
    let (state, _, _dir) = fixture().await;
    let first = add_app(&state, &manifest(), None).await;
    let old_session = launch_app(&state, "shorts").await;
    let mut next = manifest();
    next.version = "1.1.0".into();
    let package = AppPackage::parse(archive(&next)).unwrap();
    assert!(
        install_package(
            &state,
            package,
            InstallOptions {
                digest: "错误摘要".into(),
                expected_revision: Some(first.revision.clone())
            },
            "import"
        )
        .await
        .is_err()
    );
    assert_eq!(
        installed(&state, "shorts").await.unwrap().manifest.version,
        first.manifest.version
    );
    let second = add_app(&state, &next, Some(first.revision.clone())).await;
    assert_eq!(second.manifest.version, "1.1.0");
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
        StatusCode::CONFLICT
    );
    for (version, revision) in [
        ("1.0.0", second.revision.clone()),
        ("1.1.0", second.revision.clone()),
        ("1.2.0", first.revision),
    ] {
        next.version = version.into();
        let package = AppPackage::parse(archive(&next)).unwrap();
        let options = InstallOptions {
            digest: package.digest.clone(),
            expected_revision: Some(revision),
        };
        assert!(
            install_package(&state, package, options, "import")
                .await
                .is_err()
        );
    }
    assert_eq!(
        installed(&state, "shorts").await.unwrap().manifest.version,
        "1.1.0"
    );
    // 更新成功后旧摘要运行缓存被清理：目录中只保留当前版本。
    let cache = cache_root(&state).join("shorts");
    let mut names: Vec<String> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec![second.digest.clone()]);
    assert!(cache.join(&second.digest).join(".complete").is_file());
}

#[tokio::test]
async fn permissions_and_identity_are_checked_on_the_server() {
    let (state, _, _dir) = fixture().await;
    let mut restricted = manifest();
    restricted.permissions.clear();
    add_app(&state, &restricted, None).await;
    let session = launch_app(&state, "shorts").await;
    for (method, params) in [
        ("files.search", json!({})),
        ("media.url", json!({"path": "/a.mp4"})),
        ("favorites.set", json!({"path": "/a.mp4", "favorite": true})),
    ] {
        assert_eq!(
            rpc_call(
                &state,
                "shorts",
                session["session"].as_str().unwrap(),
                method,
                params
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            session["session"].as_str().unwrap(),
            "settings.get",
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        rpc_call(
            &state,
            "another",
            session["session"].as_str().unwrap(),
            "settings.get",
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            session["session"].as_str().unwrap(),
            "/admin/config",
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let direct = Request::builder()
        .uri("/api/admin/config")
        .header(header::ORIGIN, "null")
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(&state).oneshot(direct).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let unauthenticated = Request::builder()
        .uri("/api/apps")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(&state)
            .oneshot(unauthenticated)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    state.auth.rotate_secret(&state.vfs.db).await.unwrap();
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            session["session"].as_str().unwrap(),
            "app.ping",
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn scoped_media_keeps_range_bounds_and_rejects_hidden_or_revoked_files() {
    let (state, media, _dir) = fixture().await;
    state.vfs.mkdir("/视频").await.unwrap();
    let bytes = b"0123456789abcdefghijklmnopqrstuvwxyz";
    seed_file(&state, &media, "/视频/示例.mp4", bytes, 16).await;
    add_app(&state, &manifest(), None).await;
    let session = launch_app(&state, "shorts").await;
    let (status, grant) = rpc_call(
        &state,
        "shorts",
        session["session"].as_str().unwrap(),
        "media.url",
        json!({"path": "/视频/示例.mp4"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uri = grant["url"].as_str().unwrap();
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::RANGE, "bytes=5-22")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 5-22/36");
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert_eq!(
        to_bytes(response.into_body(), 100).await.unwrap().as_ref(),
        &bytes[5..23]
    );
    assert_eq!(*media.ranges.lock().unwrap(), [(5, 11), (0, 7)]);
    let media_token = uri.trim_start_matches("/api/apps/media/");
    assert_eq!(
        rpc_call(&state, "shorts", media_token, "app.ping", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let wrong = format!("/api/apps/media/{}!", media_token);
    assert_eq!(
        router(&state)
            .oneshot(Request::builder().uri(wrong).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    state.vfs.delete("/视频").await.unwrap();
    assert_eq!(
        router(&state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    state
        .vfs
        .db
        .set_app_state("shorts", Some(false), None)
        .await
        .unwrap();
    assert_eq!(
        router(&state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn backup_restores_package_and_settings_without_an_existing_cache() {
    let (state, media, dir) = fixture().await;
    let app = add_app(&state, &manifest(), None).await;
    save_settings(&state, &app, json!({"source_dir": "/备份目录"}))
        .await
        .unwrap();
    let restored_dir = dir.path().join("restored");
    std::fs::create_dir_all(&restored_dir).unwrap();
    let restored_path = restored_dir.join("backup.db");
    state
        .vfs
        .db
        .vacuum_into(restored_path.to_str().unwrap())
        .await
        .unwrap();
    let restored = state_at(&restored_path, media).await;
    assert!(!cache_root(&restored).exists());
    let launch = launch_app(&restored, "shorts").await;
    assert!(
        cache_root(&restored)
            .join("shorts")
            .join(&app.digest)
            .join("index.html")
            .is_file()
    );
    let (_, values) = rpc_call(
        &restored,
        "shorts",
        launch["session"].as_str().unwrap(),
        "settings.get",
        json!({}),
    )
    .await;
    assert_eq!(values["source_dir"], "/备份目录");
}

#[tokio::test]
async fn catalog_api_refresh_download_and_install_with_exact_digest() {
    let (state, _media, dir) = fixture().await;
    let cat_dir = dir.path().join("local-catalog");
    std::fs::create_dir_all(&cat_dir).unwrap();
    let m = manifest();
    let pkg_bytes = archive(&m);
    let pkg = AppPackage::parse(pkg_bytes.clone()).unwrap();
    std::fs::write(cat_dir.join("shorts-1.0.0.tgapp"), &pkg_bytes).unwrap();

    let cat_manager = Arc::new(CatalogManager::new(
        dir.path().join("app-catalog-cache"),
        crate::apps::catalog::CatalogSourceConfig::Local {
            path: cat_dir.clone(),
        },
        Arc::new(crate::apps::catalog::HttpCatalogTransport),
        Arc::new(crate::proxy::ProxyConfig::default),
    ));
    let mut custom_state = state.clone();
    custom_state.apps_catalog = cat_manager;

    // GET /api/apps
    let (status, index) = call(&custom_state, "GET", "/api/apps", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index["catalog"]["source"], "local");
    assert_eq!(index["catalog"]["state"], "ready");
    assert_eq!(index["available"].as_array().unwrap().len(), 1);
    assert_eq!(index["available"][0]["manifest"]["id"], "shorts");

    // POST /api/apps/catalog/refresh
    let (status, refreshed) = call(
        &custom_state,
        "POST",
        "/api/apps/catalog/refresh",
        json!({"force": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(refreshed["catalog"]["state"], "ready");

    // 用户确认旧摘要之后出现新版，仍应下载、安装被确认的历史包。
    let mut newer = m.clone();
    newer.version = "1.1.0".into();
    std::fs::write(cat_dir.join("shorts-1.1.0.tgapp"), archive(&newer)).unwrap();
    let (_, changed) = call(&custom_state, "GET", "/api/apps", json!({})).await;
    assert_eq!(changed["available"][0]["manifest"]["version"], "1.1.0");

    // GET /api/apps/catalog/{id}/package?digest=...
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/apps/catalog/shorts/package?digest={}",
            pkg.digest
        ))
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(&custom_state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/zip");
    let downloaded_bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(downloaded_bytes.as_ref(), pkg_bytes.as_slice());

    // POST /api/apps/catalog/{id}/install with wrong digest -> 409
    let (status, _) = call(
        &custom_state,
        "POST",
        "/api/apps/catalog/shorts/install",
        json!({"digest": "0000000000000000000000000000000000000000000000000000000000000000"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // POST /api/apps/catalog/{id}/install with correct digest -> OK
    let (status, installed) = call(
        &custom_state,
        "POST",
        "/api/apps/catalog/shorts/install",
        json!({"digest": pkg.digest}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(installed["manifest"]["id"], "shorts");
    assert_eq!(installed["manifest"]["version"], "1.0.0");
    assert_eq!(installed["source"], "catalog");
}

#[tokio::test]
async fn concurrent_catalog_installs_succeed_without_deadlock() {
    let (state, _media, dir) = fixture().await;
    let cat_dir = dir.path().join("local-catalog-concurrent");
    std::fs::create_dir_all(&cat_dir).unwrap();
    let m1 = manifest();
    let pkg1_bytes = archive(&m1);
    let pkg1 = AppPackage::parse(pkg1_bytes.clone()).unwrap();
    std::fs::write(cat_dir.join("shorts-1.0.0.tgapp"), &pkg1_bytes).unwrap();

    let mut m2 = manifest();
    m2.id = "demo".into();
    m2.name = "Demo".into();
    let pkg2_bytes = archive(&m2);
    let pkg2 = AppPackage::parse(pkg2_bytes.clone()).unwrap();
    std::fs::write(cat_dir.join("demo-1.0.0.tgapp"), &pkg2_bytes).unwrap();

    let cat_manager = Arc::new(CatalogManager::new(
        dir.path().join("app-catalog-cache"),
        crate::apps::catalog::CatalogSourceConfig::Local {
            path: cat_dir.clone(),
        },
        Arc::new(crate::apps::catalog::HttpCatalogTransport),
        Arc::new(crate::proxy::ProxyConfig::default),
    ));
    let mut custom_state = state.clone();
    custom_state.apps_catalog = cat_manager;

    let s1 = custom_state.clone();
    let s2 = custom_state.clone();
    let d1 = pkg1.digest.clone();
    let d2 = pkg2.digest.clone();

    let handle1 = tokio::spawn(async move {
        call(
            &s1,
            "POST",
            "/api/apps/catalog/shorts/install",
            json!({"digest": d1}),
        )
        .await
    });
    let handle2 = tokio::spawn(async move {
        call(
            &s2,
            "POST",
            "/api/apps/catalog/demo/install",
            json!({"digest": d2}),
        )
        .await
    });

    let (res1, res2) = tokio::join!(handle1, handle2);
    let (status1, app1) = res1.unwrap();
    let (status2, app2) = res2.unwrap();
    assert_eq!(status1, StatusCode::OK);
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(app1["manifest"]["id"], "shorts");
    assert_eq!(app2["manifest"]["id"], "demo");
}

fn browser_catalog_source(
    directory: Option<PathBuf>,
    repository: Option<String>,
) -> AppResult<catalog::CatalogSourceConfig> {
    if let Some(path) = directory.filter(|path| !path.as_os_str().is_empty()) {
        if !path.is_absolute() || !path.is_dir() {
            return Err(AppError::BadRequest(
                "测试应用目录必须是明确的绝对目录".into(),
            ));
        }
        return Ok(catalog::CatalogSourceConfig::Local { path });
    }
    if let Some(repo) = repository {
        if !catalog::is_valid_repo(&repo) {
            return Err(AppError::BadRequest("测试 GitHub 仓库标识无效".into()));
        }
        return Ok(catalog::CatalogSourceConfig::Github { repo });
    }
    Ok(catalog::CatalogSourceConfig::Disabled)
}

#[test]
fn browser_catalog_requires_explicit_test_inputs() {
    use catalog::CatalogSourceConfig;
    assert_eq!(
        browser_catalog_source(None, None).unwrap(),
        CatalogSourceConfig::Disabled
    );
    assert_eq!(
        browser_catalog_source(None, Some("lengyuesky/tgdrive-app".into())).unwrap(),
        CatalogSourceConfig::Github {
            repo: "lengyuesky/tgdrive-app".into()
        }
    );
    assert!(browser_catalog_source(None, Some("https://github.com/owner/repo".into())).is_err());
    assert!(browser_catalog_source(Some(PathBuf::from(".")), None).is_err());
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        browser_catalog_source(Some(directory.path().into()), Some("invalid".into())).unwrap(),
        CatalogSourceConfig::Local {
            path: directory.path().into()
        }
    );
}

async fn asset_request(
    state: &AppState,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router(state)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// 资源响应可长期不可变缓存并支持 ETag 再验证；文本资源按需压缩，SVG 图标不压缩。
#[tokio::test]
async fn runtime_assets_cache_immutably_compress_and_revalidate() {
    let (state, _, _dir) = fixture().await;
    let mut icon_manifest = manifest();
    icon_manifest.icon = Some("icon.svg".into());
    let metadata = serde_json::to_vec(&icon_manifest).unwrap();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("app.json", metadata.as_slice()),
        (
            "index.html",
            b"<!DOCTYPE html><html><body>Icon</body></html>".as_slice(),
        ),
        ("app.js", "console.log('shorts'); console.log('tgdrive-app-runtime'); console.log('assets'); console.log('end');".as_bytes()),
        (
            "icon.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'></svg>".as_slice(),
        ),
    ] {
        zip.start_file(
            name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    let package = AppPackage::parse(zip.finish().unwrap().into_inner()).unwrap();
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
    let base = format!("/api/apps/runtime/shorts/{}", app.revision);

    let response = asset_request(&state, &format!("{base}/app.js"), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(response.headers()[header::VARY], "accept-encoding");
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(etag, format!("\"{}:app.js\"", app.digest));
    let plain = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        &plain[..],
        "console.log('shorts'); console.log('tgdrive-app-runtime'); console.log('assets'); console.log('end');".as_bytes()
    );

    // 未声明压缩能力的客户端拿到原始字节。
    let response = asset_request(&state, &format!("{base}/app.js"), &[]).await;
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());

    // 声明 gzip 时文本资源压缩传输，体积小于原文且 ETag 不变。
    let response = asset_request(
        &state,
        &format!("{base}/app.js"),
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(response.headers()[header::ETAG], etag);
    let compressed = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(compressed.len() < plain.len());

    // If-None-Match 命中返回 304：无正文但保留沙箱与缓存语义。
    let response = asset_request(
        &state,
        &format!("{base}/app.js"),
        &[("if-none-match", etag.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some()
    );
    assert_eq!(response.headers()[header::ETAG], etag);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert!(to_bytes(response.into_body(), 64).await.unwrap().is_empty());

    // SVG 是文本型资源，压缩谓词将其作为 image/* 的例外正常压缩。
    let response = asset_request(
        &state,
        &format!("{base}/icon.svg"),
        &[("accept-encoding", "gzip, br")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(header::CONTENT_ENCODING).is_some());
    assert_eq!(response.headers()[header::VARY], "accept-encoding");

    // 修订不匹配的资源一律 404，停用/更新后不再暴露。
    let response = asset_request(
        &state,
        "/api/apps/runtime/shorts/stale-revision/app.js",
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// 过期会话与修订轮换返回稳定的机器码，宿主凭码静默续签或提示重开。
#[tokio::test]
async fn session_expiry_and_rotation_return_stable_error_codes() {
    let (state, _, _dir) = fixture().await;
    let app = add_app(&state, &manifest(), None).await;
    let active = launch_app(&state, "shorts").await;
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            active["session"].as_str().unwrap(),
            "app.ping",
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );

    // 过期会话：session_expired 码允许宿主前端静默续签重试。
    let claims = json!({"purpose": "session", "app_id": "shorts", "revision": app.revision, "expires_at": 0});
    let expired = state.auth.sign_app_claims(&claims.to_string());
    let (status, body) = rpc_call(&state, "shorts", &expired, "app.ping", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "session_expired");
    assert_eq!(body["error"], "应用会话已过期，请重新打开");

    // 停用轮换修订后：app_changed 码代表应用状态变化而非网络故障。
    state
        .vfs
        .db
        .set_app_state("shorts", Some(false), None)
        .await
        .unwrap();
    let (status, body) = rpc_call(
        &state,
        "shorts",
        active["session"].as_str().unwrap(),
        "app.ping",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "app_changed");
    assert_eq!(body["error"], "应用已停用、卸载或更新，请重新打开");

    // 重新启用会再次轮换修订；旧会话仍应被拒绝。
    state
        .vfs
        .db
        .set_app_state("shorts", Some(true), None)
        .await
        .unwrap();
    let (status, body) = rpc_call(
        &state,
        "shorts",
        active["session"].as_str().unwrap(),
        "app.ping",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "app_changed");
}

/// 为浏览器测试提供独立网盘和内存媒体，不读取真实账号、数据库或 Telegram。
#[tokio::test]
#[ignore]
async fn browser_fixture() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/dist");
    assert!(
        directory.join("index.html").is_file(),
        "请先执行 npm --prefix frontend run check 构建宿主前端"
    );
    let video_path = std::env::var("TGDRIVE_TEST_VIDEO").ok();
    let (mut state, media, dir) = fixture().await;
    // 只有 ignored 夹具显式选择远程测试仓库；普通测试始终注入禁用目录。
    let catalog_source = browser_catalog_source(
        std::env::var_os("TGDRIVE_APP_CATALOG_DIR").map(PathBuf::from),
        std::env::var("TGDRIVE_TEST_GITHUB_REPO").ok(),
    )
    .expect("浏览器夹具的目录参数无效");
    state.apps_catalog = Arc::new(CatalogManager::new(
        dir.path().join("app-catalog-cache"),
        catalog_source,
        Arc::new(catalog::HttpCatalogTransport),
        Arc::new(crate::proxy::ProxyConfig::default),
    ));
    state.vfs.db.mkdir(ROOT_ID, "测试视频").await.unwrap();
    state.vfs.db.mkdir(ROOT_ID, "空目录").await.unwrap();
    let mut file_size = 0;
    if let Some(ref path) = video_path {
        let bytes = std::fs::read(path).unwrap();
        file_size = bytes.len();
        for name in ["片段 A.webm", "片段 B.webm"] {
            seed_file(
                &state,
                &media,
                &format!("/测试视频/{name}"),
                &bytes,
                1024 * 1024,
            )
            .await;
        }
    }
    let files_dir = std::env::var("TGDRIVE_TEST_FILES_DIR")
        .or_else(|_| std::env::var("TGDRIVE_TEST_READERS"))
        .ok();
    if let Some(root) = files_dir {
        let mut directories = vec![(std::path::PathBuf::from(root), String::new())];
        while let Some((disk, virtual_path)) = directories.pop() {
            for item in std::fs::read_dir(disk).unwrap() {
                let item = item.unwrap();
                let path = format!("{}/{}", virtual_path, item.file_name().to_str().unwrap());
                let kind = item.file_type().unwrap();
                if kind.is_dir() {
                    state.vfs.mkdir(&path).await.unwrap();
                    directories.push((item.path(), path));
                } else if kind.is_file() {
                    seed_file(
                        &state,
                        &media,
                        &path,
                        &std::fs::read(item.path()).unwrap(),
                        1024 * 1024,
                    )
                    .await;
                } else {
                    panic!("测试夹具不能包含特殊文件");
                }
            }
        }
    }
    let stats = media.clone();
    let control = media.clone();
    let fixture_range_cache = state.range_cache.clone();
    let app = router(&state)
        .route(
            "/__apps_fixture",
            get(|| async { Json(json!({"fixture": "tgdrive-apps-tests"})) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/__apps_fixture/media",
            get(move || {
                let stats = stats.clone();
                async move {
                    let (range_count, range_bytes) = {
                        let ranges = stats.ranges.lock().unwrap();
                        (
                            ranges.len(),
                            ranges.iter().map(|(_, length)| length).sum::<u64>(),
                        )
                    };
                    Json(json!({"active": stats.active.load(Ordering::SeqCst),
                "bytes_sent": stats.bytes_sent.load(Ordering::SeqCst), "file_size": file_size,
                "range_count": range_count, "range_bytes": range_bytes}))
                }
            })
            .post(move |Json(input): Json<Value>| {
                let control = control.clone();
                let range_cache = fixture_range_cache.clone();
                async move {
                    control.slow.store(
                        input["enabled"].as_bool().unwrap_or(false),
                        Ordering::SeqCst,
                    );
                    control.bytes_sent.store(0, Ordering::SeqCst);
                    control.ranges.lock().unwrap().clear();
                    if let Ok(mut cache) = range_cache.lock() {
                        *cache = crate::apps::RangeCache::default();
                    }
                    Json(json!({"ok": true}))
                }
            }),
        )
        .fallback_service(crate::web::router(&directory))
        .layer(middleware::from_fn_with_state(
            false,
            crate::security::response_headers,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:4187")
        .await
        .unwrap();
    println!("应用测试网盘已启动：http://127.0.0.1:4187；账号 apps-test，密码 apps-test-only");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
