//! 主人后端管理、真实日志边界与根选择原子清除的 HTTP/SQLite 验收。
use super::backend_tests::backend_fixture;
use super::tests::install_page;
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tgdrive_contract::{error_code, http::APP_CENTER_BASE, runtime::*};
use tower::ServiceExt;

#[tokio::test]
async fn backend_management_and_bounded_logs_are_owner_only_and_report_actual_enforcement() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let info = platform.backend_management("test-backend").await.unwrap();
    assert_eq!(info.status.state, BackendState::Ready);
    assert_eq!(info.status.run_id.as_deref(), Some(run.caller.id.as_str()));
    assert!(!info.limits.enforced.memory && !info.limits.enforced.cpu);
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
        )
        .await
        .unwrap();
    for suffix in ["backend", "backend/logs"] {
        let path = format!("{APP_CENTER_BASE}/apps/test-backend/{suffix}");
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
        assert!(
            !to_bytes(allowed.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .is_empty()
        );
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
