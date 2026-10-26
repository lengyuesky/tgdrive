//! 原生接口使用隔离 SQLite 与空 Telegram 注册表验证，不读取生产环境。
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn fixture() -> (Router, AppState, tempfile::TempDir) {
    crate::api::entries::tests::fixture().await
}
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
        ));
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}
async fn login(app: &Router, totp: Option<&str>) -> Value {
    let (status, headers, body) = call(app, "POST", "/api/auth/device/login", None, json!({
        "username":"entries-test","password":"entries-test-only","device_name":"隔离测试 iPhone","totp":totp,
    })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(
        !headers.contains_key(header::SET_COOKIE),
        "原生登录不能把 Cookie 意外留给后续请求"
    );
    body
}

#[tokio::test]
async fn devices_rotate_refresh_detect_replay_and_revoke_access() {
    let (app, state, _dir) = fixture().await;
    let initial = login(&app, None).await;
    let access = initial["access_token"].as_str().unwrap();
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(access), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (_, _, capabilities) = call(
        &app,
        "GET",
        "/api/client/capabilities",
        Some(access),
        Value::Null,
    )
    .await;
    assert_eq!(capabilities["contract_version"], 1);
    assert_eq!(capabilities["server_id"], initial["server_id"]);
    let (_, _, devices) = call(&app, "GET", "/api/auth/devices", Some(access), Value::Null).await;
    assert_eq!(devices["items"].as_array().unwrap().len(), 1);
    assert!(devices["items"][0].get("auth_epoch").is_none());
    let raw = initial["refresh_token"].as_str().unwrap();
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&state.rt.env.db_path)
            .read_only(true),
    )
    .await
    .unwrap();
    use sqlx::Connection as _;
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM device_refresh_tokens")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    assert_ne!(stored, raw);
    assert_eq!(stored, refresh_hash(raw));
    let (status, _, next) = call(
        &app,
        "POST",
        "/api/auth/device/refresh",
        None,
        json!({"refresh_token":raw}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(next["refresh_token"], initial["refresh_token"]);
    assert_eq!(next["refresh_expires_at"], initial["refresh_expires_at"]);
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/auth/device/refresh",
            None,
            json!({"refresh_token":raw})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/auth/me",
            Some(next["access_token"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/auth/device/refresh",
            None,
            json!({"refresh_token":next["refresh_token"]})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let fresh = login(&app, None).await;
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/auth/device/logout",
            Some(fresh["access_token"].as_str().unwrap()),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/auth/me",
            Some(fresh["access_token"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn devices_share_login_limit_and_honor_totp_and_key_rotation() {
    let (app, state, _dir) = fixture().await;
    let setup = state.auth.begin_totp_setup();
    let codes = state
        .auth
        .enable_totp(
            &crate::totp::code_now(&setup.secret, crate::db::now_ts()),
            &state.vfs.db,
        )
        .await
        .unwrap();
    let (_, _, error) = call(
        &app,
        "POST",
        "/api/auth/device/login",
        None,
        json!({
            "username":"entries-test","password":"entries-test-only","device_name":"测试设备",
        }),
    )
    .await;
    assert_eq!(error["code"], "totp_required");
    let token = login(&app, Some(&codes[0])).await;
    let (_, _, error) = call(&app, "POST", "/api/auth/device/login", None, json!({
        "username":"entries-test","password":"entries-test-only","device_name":"测试设备","totp":codes[0],
    })).await;
    assert_eq!(error["code"], "totp_invalid");
    state.auth.rotate_secret(&state.vfs.db).await.unwrap();
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/auth/me",
            Some(token["access_token"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/auth/device/refresh",
            None,
            json!({"refresh_token":token["refresh_token"]})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // 原生与网页的失败共享来源桶，不能各尝试五次。
    for _ in 0..4 {
        assert_eq!(
            call(
                &app,
                "POST",
                "/api/auth/login",
                None,
                json!({"username":"entries-test","password":"wrong"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(call(&app, "POST", "/api/auth/device/login", None, json!({
        "username":"entries-test","password":"entries-test-only","device_name":"测试设备","totp":codes[1],
    })).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn stable_file_references_tickets_etags_and_strong_auth_boundaries() {
    let (app, state, _dir) = fixture().await;
    let login = login(&app, None).await;
    let token = login["access_token"].as_str().unwrap();
    let id = state.vfs.db.create_uploading(1, "empty.txt").await.unwrap();
    state
        .vfs
        .db
        .finalize_upload(id, 0, Some(&"a".repeat(64)))
        .await
        .unwrap();
    let (status, _, stat) = call(
        &app,
        "GET",
        &format!("/api/fs/stat?id={id}"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let version = stat["content_version"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/api/fs/stat?id={id}&path=/empty.txt"),
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, _, ticket) = call(
        &app,
        "POST",
        "/api/fs/media-ticket",
        Some(token),
        json!({"id":id,"content_version":version,"kind":"preview"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let url = ticket["url"].as_str().unwrap();
    let response = app
        .clone()
        .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, format!("\"{version}\""));
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(url)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/auth/me",
            Some(url.trim_start_matches("/api/media/")),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    state
        .vfs
        .rename("/empty.txt", "/renamed.txt")
        .await
        .unwrap();
    let (_, _, moved) = call(
        &app,
        "GET",
        &format!("/api/fs/stat?id={id}"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(moved["path"], "/renamed.txt");
    assert_eq!(moved["content_version"], version);
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/admin/restore/status",
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    // 无第二次密码/TOTP 认证的恢复仍然被拒绝，绝不通过设备 Bearer 放宽高危操作。
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/admin/restore/resume",
            Some(token),
            json!({"password":"wrong"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let basic = base64::engine::general_purpose::STANDARD.encode("entries-test:entries-test-only");
    use base64::Engine as _;
    for path in ["/api/admin/restore/status", "/api/auth/devices"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Basic {basic}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dav/renamed.txt")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // 内容更新和回收站都会使旧票据失效；同名路径不会自动重新绑定。
    // 夹具直接更新内容哈希模拟版本变更；生产覆盖写使用新的节点 ID。
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&state.rt.env.db_path),
    )
    .await
    .unwrap();
    use sqlx::Connection as _;
    sqlx::query("UPDATE nodes SET sha256 = ? WHERE id = ?")
        .bind("b".repeat(64))
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    assert_eq!(
        call(&app, "GET", url, None, Value::Null).await.0,
        StatusCode::CONFLICT
    );
    state.vfs.delete("/renamed.txt").await.unwrap();
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/api/fs/stat?id={id}"),
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn refresh_is_atomic_under_concurrency_and_survives_restart() {
    let (app, state, _dir) = fixture().await;
    let initial = login(&app, None).await;
    let body = json!({"refresh_token":initial["refresh_token"]});
    let (a, b) = tokio::join!(
        call(&app, "POST", "/api/auth/device/refresh", None, body.clone()),
        call(&app, "POST", "/api/auth/device/refresh", None, body)
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 401]);
    let fresh = login(&app, None).await;
    let reloaded = Auth::init(
        "entries-test",
        "entries-test-only",
        None,
        false,
        &state.vfs.db,
    )
    .await
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", fresh["access_token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    assert!(
        reloaded
            .device_from_headers(&headers, &state.vfs.db)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(reloaded.device_epoch(), state.auth.device_epoch());
}
