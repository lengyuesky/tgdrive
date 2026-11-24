//! 主人私有接口的真实认证与灾难恢复冲突行为。
use super::tests::{file, fixture, install_page};
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use tgdrive_contract::{data::*, files::*, http::*, runtime::*};
use tower::ServiceExt;

#[tokio::test]
async fn private_node_and_usage_require_owner_and_support_selected_scope_and_open() {
    let (_dir, platform, fake) = fixture().await;
    let app = install_page(&platform).await;
    let directory = platform.host.vfs.db.mkdir(1, "selected").await.unwrap();
    let file_id = file(&platform, &fake, directory.id, "item.bin", b"content").await;
    platform
        .kv_put(
            &app.manifest.id,
            &app.generation,
            "setting",
            KvPutRequest {
                value: serde_json::json!({"saved":true}),
                condition: KvCondition::Absent,
            },
        )
        .await
        .unwrap();
    let router = http::router_platform(platform.clone());
    let cookie = platform
        .host
        .auth
        .cookie_header(&platform.host.auth.issue_token())
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let node_path = format!("{APP_CENTER_BASE}/nodes/stat");
    let node_body = serde_json::to_vec(&NodeReference::Path {
        path: "/selected".into(),
        content_version: None,
    })
    .unwrap();
    let launch = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
            &super::http::PagePolicy::new(None).revision,
        )
        .await
        .unwrap();
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&node_path)
                .header("host", "localhost")
                .header("origin", "null")
                .header("cookie", &cookie)
                .header(
                    "authorization",
                    format!("Bearer {}", launch.bootstrap.session_token),
                )
                .body(Body::from(node_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let cross = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&node_path)
                .header("host", "localhost")
                .header("origin", "https://other.invalid")
                .header("cookie", &cookie)
                .body(Body::from(node_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(node_path)
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(node_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let node: FileNode =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(node.id, directory.id.to_string());
    assert_eq!(node.kind, NodeKind::Directory);
    assert!(
        platform
            .launch(
                &app.manifest.id,
                LaunchPageRequest {
                    open_node_id: Some(file_id.to_string())
                },
                "http://localhost".into(),
                &super::http::PagePolicy::new(None).revision
            )
            .await
            .is_err()
    );
    let scope = ScopeUpdateRequest {
        expected_revision: app.scope.revision,
        scope: DirectoryScope::Selected {
            directories: vec![DirectoryGrant {
                directory_id: node.id,
                access: AccessLevel::ReadOnly,
            }],
        },
    };
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("{APP_CENTER_BASE}/apps/{}/scope", app.manifest.id))
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&scope).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let opened = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest {
                open_node_id: Some(file_id.to_string()),
            },
            "http://localhost".into(),
            &super::http::PagePolicy::new(None).revision,
        )
        .await
        .unwrap();
    assert!(
        matches!(opened.bootstrap.launch,LaunchAction::Open{file} if file.id==file_id.to_string())
    );
    let usage_path = format!("{APP_CENTER_BASE}/apps/{}/data/usage", app.manifest.id);
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&usage_path)
                .header(
                    "authorization",
                    format!("Bearer {}", opened.bootstrap.session_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(
            Request::builder()
                .uri(usage_path)
                .header("host", "localhost")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: DataUsage =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(usage.kv_entries, 1);
}

#[tokio::test]
async fn restore_conflicts_keep_backup_bindings_log_warnings_and_do_not_merge_kv() {
    let (_dir, platform, _) = fixture().await;
    let app = install_page(&platform).await;
    platform
        .kv_put(
            &app.manifest.id,
            &app.generation,
            "state",
            KvPutRequest {
                value: serde_json::json!("backup"),
                condition: KvCondition::Absent,
            },
        )
        .await
        .unwrap();
    let snapshot = tempfile::tempdir().unwrap();
    let path = snapshot.path().join("candidate.db");
    platform
        .host
        .vfs
        .db
        .vacuum_into(path.to_str().unwrap())
        .await
        .unwrap();
    let sha_before = app.package.sha256.clone();
    let changed = "b".repeat(64);
    sqlx::query("UPDATE app_package_bindings SET sha256=? WHERE app_id=?")
        .bind(&changed)
        .bind(&app.manifest.id)
        .execute(&platform.host.vfs.db.pool)
        .await
        .unwrap();
    let mut extra = app.manifest.clone();
    extra.id = "live-only".into();
    sqlx::query("INSERT INTO app_package_bindings(app_id,version,target,manifest,sha256,size) VALUES(?,?,?,?,?,?)").bind(&extra.id).bind(&extra.version).bind("web").bind(serde_json::to_string(&extra).unwrap()).bind(&changed).bind(100).execute(&platform.host.vfs.db.pool).await.unwrap();
    let old = platform
        .kv_get(&app.manifest.id, &app.generation, "state")
        .await
        .unwrap()
        .record
        .unwrap();
    platform
        .kv_put(
            &app.manifest.id,
            &app.generation,
            "state",
            KvPutRequest {
                value: serde_json::json!("live"),
                condition: KvCondition::Revision {
                    revision: old.revision,
                },
            },
        )
        .await
        .unwrap();
    use sqlx::Connection;
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    let mut tx = conn.begin().await.unwrap();
    let warnings = restore::preserve_history(&mut tx, &platform.host.vfs.db)
        .await
        .unwrap();
    assert!(!warnings.is_empty());
    tx.commit().await.unwrap();
    let sha: String = sqlx::query_scalar("SELECT sha256 FROM app_package_bindings WHERE app_id=?")
        .bind(&app.manifest.id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(sha, sha_before);
    let added: i64 =
        sqlx::query_scalar("SELECT count(*) FROM app_package_bindings WHERE app_id='live-only'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(added, 1);
    let value: String =
        sqlx::query_scalar("SELECT value FROM app_kv WHERE app_id=? AND key='state'")
            .bind(&app.manifest.id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&value).unwrap(),
        serde_json::json!("backup")
    );
    let logs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backend_logs WHERE category='应用平台' AND level='warn'",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert!(logs > 0);
    conn.close().await.unwrap();
}
