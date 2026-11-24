//! 后端日志访问边界、总体限额同步与根选择原子清除的 HTTP/SQLite 验收。
use super::backend_tests::{backend_fixture, runtime_info};
use super::tests::install_page;
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tgdrive_contract::{error_code, http::APP_CENTER_BASE, runtime::*};
use tower::ServiceExt;

#[tokio::test]
async fn bounded_backend_logs_are_owner_only_and_backend_management_route_is_absent() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let info = runtime_info(&run).await;
    let status = info.backend.unwrap();
    assert_eq!(status.state, BackendState::Ready);
    assert_eq!(status.run_id.as_deref(), Some(run.caller.id.as_str()));
    let limits = info.limits.backend.unwrap();
    assert!(!limits.enforced.memory && !limits.enforced.cpu);
    let directory = platform.root.join("logs/test-backend");
    std::fs::create_dir_all(&directory).unwrap();
    let stdout = format!("{}newest", "x".repeat(96 * 1024));
    std::fs::write(directory.join("stdout.log"), stdout).unwrap();
    let logs = platform.backend_logs("test-backend").await.unwrap();
    assert_eq!(logs["stdout"].as_str().unwrap().len(), 64 * 1024);
    assert!(logs["stdout"].as_str().unwrap().ends_with("newest"));
    assert_eq!(logs["truncated"], true);
    let router = http::router_platform(platform.clone());
    let cookie = platform
        .host
        .auth
        .cookie_header(&platform.host.auth.issue_token())
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let launch = platform
        .launch(
            "test-backend",
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
            &super::http::PagePolicy::new(None).revision,
        )
        .await
        .unwrap();
    let path = format!("{APP_CENTER_BASE}/apps/test-backend/backend/logs");
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("host", "localhost")
                .header(
                    "authorization",
                    format!("Bearer {}", launch.bootstrap.session_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let sandbox = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("host", "localhost")
                .header("origin", "null")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sandbox.status(), StatusCode::UNAUTHORIZED);
    let allowed = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("host", "localhost")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    let logs: serde_json::Value = serde_json::from_slice(
        &to_bytes(allowed.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(logs["stdout"].as_str().unwrap().len(), 64 * 1024);
    assert!(logs["stdout"].as_str().unwrap().ends_with("newest"));
    assert_eq!(logs["truncated"], true);
    // 已删除的管理接口应与其他未知应用中心路径一致返回 404，主人 Cookie 也不能恢复它。
    for suffix in ["backend", "unknown-management-route"] {
        let absent = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{APP_CENTER_BASE}/apps/test-backend/{suffix}"))
                    .header("host", "localhost")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    }
    let restarted = platform
        .backends
        .restart(&platform, "test-backend")
        .await
        .unwrap();
    assert_ne!(restarted.run_id.as_deref(), Some(run.caller.id.as_str()));
    assert_eq!(
        platform.installed("test-backend").await.unwrap().generation,
        run.caller.generation
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn only_declared_enabled_backends_can_own_root_and_unrelated_uninstall_keeps_revision() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let page = install_page(&platform).await;
    let initial = platform.root_selection().await.unwrap();
    assert_eq!(
        platform
            .root_update(RootAppUpdateRequest {
                expected_revision: initial.revision.clone(),
                app_id: Some(page.manifest.id.clone())
            })
            .await
            .unwrap_err()
            .code,
        error_code::CALLER_NOT_ALLOWED
    );
    let selected = platform
        .root_update(RootAppUpdateRequest {
            expected_revision: initial.revision,
            app_id: Some("test-backend".into()),
        })
        .await
        .unwrap();
    platform
        .uninstall(&page.manifest.id, &page.state_revision, true)
        .await
        .unwrap();
    assert_eq!(platform.root_selection().await.unwrap(), selected);
    let backend = platform.installed("test-backend").await.unwrap();
    platform
        .state_patch(
            "test-backend",
            AppStatePatch {
                expected_state_revision: backend.state_revision,
                enabled: Some(false),
                pinned: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        platform
            .root_update(RootAppUpdateRequest {
                expected_revision: selected.revision.clone(),
                app_id: Some("test-backend".into())
            })
            .await
            .unwrap_err()
            .code,
        error_code::CALLER_NOT_ALLOWED
    );
    assert_eq!(platform.root_selection().await.unwrap(), selected);
    let backend = platform.installed("test-backend").await.unwrap();
    platform
        .uninstall("test-backend", &backend.state_revision, true)
        .await
        .unwrap();
    let cleared = platform.root_selection().await.unwrap();
    assert!(cleared.app_id.is_none());
    assert_ne!(cleared.revision, selected.revision);
}

#[tokio::test]
async fn one_shared_resource_limit_applies_to_all_backends_without_restart_and_persists() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    sqlx::query("INSERT INTO app_installations(id,manifest,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at) SELECT ?,json_set(manifest,'$.id',?),generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at FROM app_installations WHERE id=?")
        .bind("second-backend")
        .bind("second-backend")
        .bind("test-backend")
        .execute(&platform.host.vfs.db.pool)
        .await
        .unwrap();
    let second = platform
        .backends
        .ensure(&platform, "second-backend")
        .await
        .unwrap();
    // 默认值：内存合计 2 GiB、CPU 不限制；应用清单不参与决定。
    let initial = runtime_info(&run).await.limits.backend.unwrap();
    assert_eq!(
        initial.memory_bytes.0,
        tgdrive_contract::http::DEFAULT_SHARED_BACKEND_MEMORY_BYTES
    );
    assert_eq!(initial.cpu_millis, 0);
    assert_eq!(runtime_info(&second).await.limits.backend.unwrap(), initial);
    let router = http::router_platform(platform.clone());
    let cookie = platform
        .host
        .auth
        .cookie_header(&platform.host.auth.issue_token())
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let path = format!("{APP_CENTER_BASE}/resource-limits");
    let send = |method: &str, cookie: Option<&str>, body: Option<serde_json::Value>| {
        let mut request = Request::builder()
            .method(method)
            .uri(&path)
            .header("host", "localhost")
            .header("origin", "http://localhost");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let body = match body {
            Some(value) => {
                request = request.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&value).unwrap())
            }
            None => Body::empty(),
        };
        router.clone().oneshot(request.body(body).unwrap())
    };
    assert_eq!(
        send("GET", None, None).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let read = send("GET", Some(&cookie), None).await.unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    let read: AppResourceLimits =
        serde_json::from_slice(&to_bytes(read.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(read.memory_bytes, initial.memory_bytes);
    assert!(!read.memory_enforced && !read.cpu_enforced);
    for invalid in [
        serde_json::json!({"memory_bytes":"1048576","cpu_millis":0}),
        serde_json::json!({"memory_bytes":"0","cpu_millis":50}),
    ] {
        let rejected = send("PUT", Some(&cookie), Some(invalid)).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        platform.shared_limits().await.unwrap().report(),
        read,
        "无效请求不能改变限制"
    );
    let saved = send(
        "PUT",
        Some(&cookie),
        Some(serde_json::json!({"memory_bytes":"1073741824","cpu_millis":1500})),
    )
    .await
    .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    // 运行中的两个后端不重启，实际调用 RuntimeInfo 立即看到相同的新总体限制。
    for running in [&run, &second] {
        let current = runtime_info(running).await;
        assert_eq!(
            current.backend.unwrap().run_id.as_deref(),
            Some(running.caller.id.as_str())
        );
        let limits = current.limits.backend.unwrap();
        assert_eq!(limits.memory_bytes.0, 1024 * 1024 * 1024);
        assert_eq!(limits.cpu_millis, 1500);
        assert!(!limits.enforced.memory && !limits.enforced.cpu);
        assert_eq!(*running.limits.read().unwrap(), limits);
    }
    platform.backends.stop("test-backend").await.unwrap();
    platform.backends.stop("second-backend").await.unwrap();
    // 重新构造平台后从设置表读回，不回到默认值。
    let reopened = AppPlatform::with_repository(platform.host.clone(), String::new());
    let report = reopened.shared_limits().await.unwrap().report();
    assert_eq!(report.memory_bytes.0, 1024 * 1024 * 1024);
    assert_eq!(report.cpu_millis, 1500);
    let unlimited = reopened
        .resource_limits_update(AppResourceLimitsUpdateRequest {
            memory_bytes: tgdrive_contract::ByteCount(0),
            cpu_millis: 0,
        })
        .await
        .unwrap();
    assert_eq!((unlimited.memory_bytes.0, unlimited.cpu_millis), (0, 0));
}
