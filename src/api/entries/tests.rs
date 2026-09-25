use super::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::db::Db;
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::settings::{Runtime, RuntimeCfg};
use crate::telegram::TgRegistry;
use crate::vfs::Vfs;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt as _;

async fn fixture() -> (Router, AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("api.db").to_str().unwrap().to_string();
    let db = Db::open(&db_path).await.unwrap();
    // 不读取环境配置，也不创建任何 Telegram 客户端；元数据请求必须仍能成功。
    let env = Arc::new(Config {
        proxy: crate::proxy::ProxyConfig::default(),
        listen: "127.0.0.1:0".parse().unwrap(),
        bot_token: String::new(),
        upload_bot_tokens: vec![],
        chat_id: String::new(),
        api_base: "http://127.0.0.1:1".into(),
        chunk_size: 1024 * 1024,
        upload_concurrency: 1,
        download_concurrency: crate::config::DEFAULT_DOWNLOAD_CONCURRENCY,
        db_path,
        tmp_dir: dir.path().to_path_buf(),
        local_data_root: dir.path().to_path_buf(),
        username: "entries-test".into(),
        password: "entries-test-only".into(),
        secret: None,
        cookie_secure: false,
        trusted_proxies: Vec::new(),
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: false,
    });
    let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
    let tg = Arc::new(
        TgRegistry::new(
            &[],
            None,
            vec![],
            None,
            crate::proxy::ProxyConfig::default(),
        )
        .unwrap(),
    );
    assert!(tg.current().is_err());
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        dir.path().to_path_buf(),
        1,
        false,
    ));
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
    let state = AppState {
        dav: crate::dav::build_handler(vfs.clone()),
        vfs,
        auth,
        rt,
        attempts: Default::default(),
        tasks: crate::tasks::TaskManager::new(),
        migration: crate::migration::StorageMigrationManager::new(db, tg),
        apps_catalog: Arc::new(crate::apps::CatalogManager::disabled()),
        events: crate::events::bus(),
        range_cache: Arc::new(std::sync::Mutex::new(crate::apps::RangeCache::default())),
    };
    let app = Router::new()
        .nest("/api", crate::api::router(state.clone()))
        .route(
            "/dav/{*path}",
            axum::routing::any(crate::dav::dav_handler).with_state(state.clone()),
        );
    (app, state, dir)
}

async fn request(app: &Router, uri: &str, authenticated: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri);
    if authenticated {
        let basic =
            base64::engine::general_purpose::STANDARD.encode("entries-test:entries-test-only");
        builder = builder.header("Authorization", format!("Basic {basic}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

#[tokio::test]
async fn entries_api_auth_pagination_normalization_and_old_list_compatibility() {
    let (app, state, _dir) = fixture().await;
    let directory = state.vfs.db.mkdir(1, "目录").await.unwrap();
    for index in 0..201 {
        state
            .vfs
            .db
            .mkdir(directory.id, &format!("子目录{index:03}"))
            .await
            .unwrap();
    }
    let path = "/%E7%9B%AE%E5%BD%95";
    let uri = format!("/api/fs/entries?path={path}//./");
    assert_eq!(request(&app, &uri, false).await.0, StatusCode::UNAUTHORIZED);
    let (status, first) = request(&app, &uri, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["path"], "/目录");
    assert_eq!(first["total"], 201);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["entries"].as_array().unwrap().len(), 200);
    let entry = &first["entries"][0];
    for field in ["id", "size", "created_at", "modified_at"] {
        assert!(entry[field].is_i64(), "{field}");
    }
    assert!(entry["name"].is_string());
    assert!(entry["is_dir"].is_boolean());
    assert!(entry["favorite"].is_boolean());
    assert!(entry["sha256"].is_null());
    assert_eq!(entry["has_thumbnail"], false);
    let cursor = first["next_cursor"].as_str().unwrap();
    let next_uri = format!("/api/fs/entries?path={path}&cursor={cursor}");
    assert_eq!(
        request(&app, &next_uri, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, next) = request(&app, &next_uri, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["entries"].as_array().unwrap().len(), 1);
    assert!(next.get("total").is_none());
    assert_eq!(next["has_more"], false);
    assert!(next["next_cursor"].is_null());
    assert_ne!(next["entries"][0]["id"], first["entries"][199]["id"]);
    let (status, old) = request(&app, &format!("/api/fs/list?path={path}"), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(old["entries"].as_array().unwrap().len(), 201);
    assert!(old["entries"][0].get("id").is_none());
    assert!(old["entries"][0].get("has_thumbnail").is_none());
    for suffix in ["&sort=size", "&order=desc", "&kind=dir", "&q=x", "&limit=1"] {
        assert_eq!(
            request(&app, &format!("{next_uri}{suffix}"), true).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &app,
            &format!("/api/fs/entries?path=/&cursor={cursor}"),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // 游标未签名也不能绕过当前目录的 ready 检查。
    state
        .vfs
        .db
        .trash_node(directory.id, "/目录")
        .await
        .unwrap();
    assert_eq!(
        request(&app, &next_uri, true).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn entries_api_validation_filters_and_file_thumbnail_metadata_without_telegram() {
    let (app, state, _dir) = fixture().await;
    let db = &state.vfs.db;
    db.mkdir(1, "folder").await.unwrap();
    let id = db.create_uploading(1, "A%_文.jpg").await.unwrap();
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "test-only", "test-only")
        .await
        .unwrap();
    db.add_chunk(id, 0, 7, "test-file", 1, target.id, Some("test-thumb"))
        .await
        .unwrap();
    db.finalize_upload(id, 7, None).await.unwrap();
    db.create_uploading(1, "hidden").await.unwrap();
    let (_, files) = request(
        &app,
        "/api/fs/entries?path=/&kind=file&q=a%25_&sort=mtime&order=desc",
        true,
    )
    .await;
    assert_eq!(files["total"], 1);
    assert_eq!(files["entries"][0]["id"], id);
    assert_eq!(files["entries"][0]["size"], 7);
    assert_eq!(files["entries"][0]["has_thumbnail"], true);
    let (_, dirs) = request(&app, "/api/fs/entries?path=/&kind=dir&limit=500", true).await;
    assert_eq!(dirs["total"], 1);
    assert_eq!(dirs["entries"][0]["name"], "folder");
    let (_, empty) = request(&app, "/api/fs/entries?path=/folder", true).await;
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["has_more"], false);
    for suffix in [
        "limit=0",
        "limit=501",
        "limit=-1",
        "limit=1.5",
        "limit=no",
        "limit=65536",
        "sort=created",
        "order=DESC",
        "kind=image",
        "cursor=",
        "cursor=bad!",
    ] {
        assert_eq!(
            request(&app, &format!("/api/fs/entries?path=/&{suffix}"), true)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{suffix}"
        );
    }
    for uri in [
        "/api/fs/entries",
        "/api/fs/entries?path=/../",
        "/api/fs/entries?path=/a%00",
    ] {
        assert_eq!(
            request(&app, uri, true).await.0,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    assert_eq!(
        request(
            &app,
            &format!("/api/fs/entries?path=/&q={}", "a".repeat(256)),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            &format!("/api/fs/entries?path={}", "/".repeat(4097)),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "/api/fs/entries?path=/missing", true).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "/api/fs/entries?path=/hidden", true).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "/api/fs/entries?path=/A%25_%E6%96%87.jpg", true)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let no_first = db.create_uploading(1, "no-first.jpg").await.unwrap();
    db.add_chunk(no_first, 1, 1, "later", 2, target.id, Some("ignored-thumb"))
        .await
        .unwrap();
    db.finalize_upload(no_first, 1, None).await.unwrap();
    assert_eq!(
        request(&app, "/api/fs/thumbnail?path=/no-first.jpg", true)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(state.vfs.tg.current().is_err());
}

#[tokio::test]
async fn entries_migration_preserves_webdav_full_directory_listing() {
    let (app, state, _dir) = fixture().await;
    let parent = state.vfs.db.mkdir(1, "dav-test").await.unwrap();
    for index in 0..201 {
        state
            .vfs
            .db
            .mkdir(parent.id, &format!("child-{index:03}"))
            .await
            .unwrap();
    }
    let basic = base64::engine::general_purpose::STANDARD.encode("entries-test:entries-test-only");
    let request = Request::builder()
        .method("PROPFIND")
        .uri("/dav/dav-test/")
        .header("Authorization", format!("Basic {basic}"))
        .header("Depth", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for index in 0..201 {
        assert!(body.contains(&format!("child-{index:03}")));
    }
}

async fn send(app: &Router, request: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(request).await.unwrap()
}

fn basic(user_pass: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(user_pass)
    )
}

#[tokio::test]
async fn dav_requires_basic_and_ignores_session_cookie() {
    let (app, state, _dir) = fixture().await;
    state.vfs.db.mkdir(1, "d").await.unwrap();
    let response = send(
        &app,
        Request::builder()
            .uri("/dav/d")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));

    // 浏览器登录 Cookie 对 /api 有效，但不能借 /dav 读写网盘。
    let cookie = format!("tgdrive_token={}", state.auth.issue_token());
    let response = send(
        &app,
        Request::builder()
            .uri("/api/auth/me")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send(
        &app,
        Request::builder()
            .uri("/dav/d")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn basic_auth_failures_lock_out_even_correct_password() {
    let (app, state, _dir) = fixture().await;
    state.vfs.db.mkdir(1, "d").await.unwrap();
    let dav = |auth: String| {
        Request::builder()
            .method("PROPFIND")
            .uri("/dav/d")
            .header("authorization", auth)
            .header("depth", "0")
            .body(Body::empty())
            .unwrap()
    };
    let ok = send(&app, dav(basic("entries-test:entries-test-only"))).await;
    assert!(ok.status().is_success(), "{}", ok.status());
    for _ in 0..10 {
        let response = send(&app, dav(basic("entries-test:wrong"))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let locked = send(&app, dav(basic("entries-test:entries-test-only"))).await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn cross_site_writes_are_rejected_before_auth() {
    let (app, state, _dir) = fixture().await;
    let cookie = format!("tgdrive_token={}", state.auth.issue_token());
    let mkdir = |site: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/fs/mkdir")
            .header("cookie", &cookie)
            .header("sec-fetch-site", site)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"path":"/csrf"}"#))
            .unwrap()
    };
    assert_eq!(
        send(&app, mkdir("cross-site")).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(state.vfs.db.resolve(&["csrf"]).await.unwrap().is_none());
    assert_eq!(
        send(&app, mkdir("same-origin")).await.status(),
        StatusCode::OK
    );
    assert!(state.vfs.db.resolve(&["csrf"]).await.unwrap().is_some());
}

#[tokio::test]
async fn metrics_require_auth_and_use_prometheus_text() {
    let (app, state, _dir) = fixture().await;
    state.vfs.db.mkdir(1, "m").await.unwrap();
    assert_eq!(
        request(&app, "/api/metrics", false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = request(&app, "/api/metrics", true).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap();
    assert!(
        text.contains("# TYPE tgdrive_dirs gauge\ntgdrive_dirs 1\n"),
        "{text}"
    );
    assert!(text.contains("tgdrive_http_requests_total{class=\"4xx\"}"));
}
