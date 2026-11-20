//! 真实 SDK 后端的三入口、直传、票据归属与撤销符合性。
use super::backend_tests::{backend_fixture, forwarded, test_file};
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tgdrive_contract::{events::*, files::*, http::*, runtime::*};
use tower::ServiceExt;

async fn request(
    router: &axum::Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "drive.invalid");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn page(platform: &Arc<AppPlatform>) -> LaunchPageResponse {
    platform
        .launch(
            "test-backend",
            LaunchPageRequest { open_node_id: None },
            "http://drive.invalid".into(),
        )
        .await
        .unwrap()
}
async fn root(platform: &Arc<AppPlatform>, id: Option<&str>) {
    let old = platform.root_selection().await.unwrap();
    platform
        .root_update(RootAppUpdateRequest {
            expected_revision: old.revision,
            app_id: id.map(str::to_owned),
        })
        .await
        .unwrap();
}
async fn value(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_JSON_RESPONSE_BYTES as usize)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn three_entries_enforce_cookie_bearer_identity_and_safety_headers() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let router = http::router_platform(platform.clone());
    for method in [Method::GET, Method::HEAD, Method::POST] {
        let response = request(&router, method.clone(), "/unset", &[]).await;
        assert_eq!(
            response.status(),
            if matches!(method, Method::GET | Method::HEAD) {
                302
            } else {
                303
            }
        );
        assert_eq!(response.headers()["location"], "/admin/");
        assert_eq!(response.headers()["content-security-policy"], EXTERNAL_CSP);
    }
    root(&platform, Some("test-backend")).await;
    let launch = page(&platform).await;
    assert!(launch.bootstrap.backend_base.is_some());
    let bearer = format!("Bearer {}", launch.bootstrap.session_token);
    let forged = [
        (
            "cookie",
            "tgdrive_token=host; app_session=root; other=value",
        ),
        ("authorization", bearer.as_str()),
        ("x-tgdrive-context", "forged"),
        ("x-forwarded-host", "outside.invalid"),
        ("x-forwarded-proto", "https"),
    ];
    for (entry, path) in [
        ("page", format!("{SELF_API_BASE}/backend/cookies")),
        ("prefix", format!("{SERVICE_BASE}/test-backend/cookies")),
        ("root", "/cookies".into()),
    ] {
        let response = request(&router, Method::GET, &path, &forged).await;
        assert_eq!(response.status(), 200, "{entry}");
        assert_eq!(response.headers()["content-security-policy"], EXTERNAL_CSP);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert!(
            !response
                .headers()
                .contains_key("content-security-policy-report-only")
        );
        assert!(!response.headers().contains_key("x-tgdrive-forged"));
        let cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .collect::<Vec<_>>();
        if entry == "root" {
            assert_eq!(cookies, vec!["app_session=private; HttpOnly; Path=/"]);
        } else {
            assert!(cookies.is_empty());
        }
        let value = value(response).await;
        assert_eq!(value["context"]["entry"], entry);
        assert_eq!(
            value["context"]["caller"],
            if entry == "page" { "owner" } else { "external" }
        );
        assert_eq!(value["context"]["public_origin"], "http://drive.invalid");
        assert_eq!(value["uri"], "/cookies");
        assert!(value["headers"].get("authorization").is_none());
        assert!(value["headers"].get("x-forwarded-host").is_none());
        if entry == "root" {
            assert_eq!(value["headers"]["cookie"], "app_session=root; other=value");
        } else {
            assert!(value["headers"].get("cookie").is_none());
        }
    }
    let expired_native = platform
        .host
        .auth
        .sign_native_claims(&crate::auth::native::NativeClaims {
            purpose: "access".into(),
            session_id: "device-1".into(),
            auth_epoch: platform.host.auth.device_epoch(),
            expires_at: crate::db::now_ts() - 1,
            node_id: None,
            content_version: None,
        })
        .unwrap();
    platform.sessions.revoke(&launch.bootstrap.ui_nonce);
    for token in [
        expired_native,
        launch.bootstrap.session_token,
        platform.host.auth.issue_token(),
    ] {
        for path in [
            "/echo".to_owned(),
            format!("{SERVICE_BASE}/test-backend/echo"),
        ] {
            let bearer = format!("Bearer {token}");
            let response =
                request(&router, Method::GET, &path, &[("authorization", &bearer)]).await;
            let value = value(response).await;
            assert_eq!(value["context"]["caller"], "external");
            assert!(value["headers"].get("authorization").is_none());
        }
    }
    let response = request(
        &router,
        Method::GET,
        "/echo",
        &[("authorization", "Bearer session-looking.but-not-signed")],
    )
    .await;
    let value = value(response).await;
    assert_eq!(
        value["headers"]["authorization"],
        "Bearer session-looking.but-not-signed"
    );
    assert_eq!(value["context"]["caller"], "external");
    let response = request(&router, Method::GET, "/invalid-stream", &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers()["content-security-policy"], EXTERNAL_CSP);
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn root_assignment_changes_notify_only_affected_apps_without_revoking_pages() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let mut other = store::installation(&mut connection, "test-backend")
        .await
        .unwrap()
        .manifest()
        .unwrap();
    other.id = "other-backend".into();
    sqlx::query("INSERT INTO app_installations SELECT ?,?,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at FROM app_installations WHERE id='test-backend'").bind(&other.id).bind(serde_json::to_string(&other).unwrap()).execute(&mut *connection).await.unwrap();
    drop(connection);
    let launch = page(&platform).await;
    let (_, mut old) = platform.events.subscribe("test-backend", None).unwrap();
    let (_, mut new) = platform.events.subscribe("other-backend", None).unwrap();
    root(&platform, Some("test-backend")).await;
    assert_eq!(
        old.recv().await.unwrap().event,
        event_name::EXTERNAL_SERVICE_CHANGED
    );
    assert!(new.try_recv().is_err());
    root(&platform, Some("other-backend")).await;
    for receiver in [&mut old, &mut new] {
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.event, event_name::EXTERNAL_SERVICE_CHANGED);
        assert_eq!(event.data, serde_json::json!({}));
    }
    assert!(
        platform
            .sessions
            .authenticate(
                &platform.cursors,
                &launch.bootstrap.session_token,
                &platform.host.auth.device_epoch()
            )
            .is_ok()
    );
    let router = http::router_platform(platform.clone());
    let response = request(
        &router,
        Method::GET,
        &format!("{SELF_API_BASE}/info"),
        &[(
            "authorization",
            &format!("Bearer {}", launch.bootstrap.session_token),
        )],
    )
    .await;
    let info = value(response).await;
    assert_eq!(info["external_service"]["root_assigned"], false);
    assert!(info["external_service"]["root_url"].is_null());
    assert_eq!(
        info["external_service"]["prefix_url"],
        format!("http://drive.invalid{SERVICE_BASE}/test-backend")
    );
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let info = value(forwarded(&run, "/host-info").await).await;
    assert!(info["external_service"]["root_url"].is_null());
    assert!(info["external_service"]["prefix_url"].is_null());
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn directive_windows_conditions_events_and_revocation_are_host_owned() {
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let file = test_file(&platform, &fake, b"0123456789").await;
    root(&platform, Some("test-backend")).await;
    let router = http::router_platform(platform.clone());
    let (_, mut events) = platform.events.subscribe("test-backend", None).unwrap();
    let path = format!(
        "/stream?id={}&offset=2&length=6&correlation=transfer-1",
        file.id
    );
    let response = request(&router, Method::GET, &path, &[("range", "bytes=2-4")]).await;
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()["content-range"], "bytes 2-4/6");
    assert_eq!(response.headers()["content-length"], "3");
    assert_eq!(
        response.headers()["etag"],
        format!("\"v{}:2:6\"", file.content_version)
    );
    assert_eq!(response.headers()["x-example-protocol"], "kept");
    assert!(!response.headers().contains_key(STREAM_DIRECTIVE_HEADER));
    assert_eq!(
        to_bytes(response.into_body(), 32).await.unwrap(),
        b"456"[..]
    );
    let started = events.recv().await.unwrap();
    assert_eq!(started.event, event_name::STREAM_STARTED);
    assert_eq!(
        started.data["range"],
        serde_json::json!({"offset":"4","length":"3"})
    );
    let finished = events.recv().await.unwrap();
    assert_eq!(finished.event, event_name::STREAM_FINISHED);
    assert_eq!(finished.data["bytes_sent"], "3");
    assert_eq!(finished.data["outcome"], "completed");
    for (method, headers, status) in [
        (Method::HEAD, vec![("range", "bytes=2-4")], 206),
        (Method::GET, vec![("if-none-match", "*")], 304),
        (Method::GET, vec![("range", "bytes=0-1,3-4")], 416),
    ] {
        let response = request(&router, method, &path, &headers).await;
        assert_eq!(response.status(), status);
        assert!(events.try_recv().is_err());
    }
    let lease = platform
        .create_lease(&run.caller, CreateReadLeaseRequest { ttl_seconds: 60 })
        .await
        .unwrap();
    let large = test_file(&platform, &fake, &vec![1; 1024 * 1024]).await;
    let mut response = request(
        &router,
        Method::GET,
        &format!(
            "/stream?id={}&lease={}&correlation=transfer-2",
            large.id, lease.id
        ),
        &[],
    )
    .await;
    assert!(
        response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .len()
            > 0
    );
    platform.revoke_lease(&run.caller, &lease.id).unwrap();
    assert!(response.body_mut().frame().await.unwrap().is_err());
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn page_streams_close_and_backend_crashes_do_not_revoke_the_page() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let launch = page(&platform).await;
    let router = http::router_platform(platform.clone());
    let mut response = request(
        &router,
        Method::GET,
        &format!("{SELF_API_BASE}/backend/sse"),
        &[(
            "authorization",
            &format!("Bearer {}", launch.bootstrap.session_token),
        )],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert!(response.body_mut().frame().await.unwrap().is_ok());
    platform.sessions.revoke(&launch.bootstrap.ui_nonce);
    assert!(response.body_mut().frame().await.unwrap().is_err());
    let launch = page(&platform).await;
    let run = platform.backends.run("test-backend").unwrap();
    let response = forwarded(&run, "/spawn").await;
    let child = value(response).await["pid"].as_i64().unwrap() as i32;
    let _ = backend::request(
        &run,
        hyper::Request::builder()
            .uri("/exit")
            .header("host", "tgdrive")
            .header(
                FORWARD_CONTEXT_HEADER,
                super::backend_tests::forward(&run, ForwardCaller::Owner, ForwardEntry::Page),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), run.finished.cancelled())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while unsafe { libc::kill(child, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(run.caller.cancelled.is_cancelled());
    assert!(
        platform
            .sessions
            .authenticate(
                &platform.cursors,
                &launch.bootstrap.session_token,
                &platform.host.auth.device_epoch()
            )
            .is_ok()
    );
    assert_eq!(
        platform
            .installed("test-backend")
            .await
            .unwrap()
            .backend
            .unwrap()
            .state,
        BackendState::Failed
    );
}

use std::time::Duration;
