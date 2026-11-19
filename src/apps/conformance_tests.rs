//! 字节、会话、撤销与恢复使用真实宿主 HTTP/SQLite；仅 Telegram 内容是测试替身。

use super::tests::{authorize_all, file, fixture, install_page};
use super::*;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tgdrive_contract::{data::*, error_code, files::*, http::*, runtime::*};
use tower::ServiceExt;

async fn request(
    router: &Router,
    method: Method,
    path: String,
    token: &str,
    body: Body,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("origin", "null")
        .header("authorization", format!("Bearer {token}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}
async fn session(
    platform: &Arc<AppPlatform>,
) -> (InstalledApp, LaunchPageResponse, Arc<runtime::PageSession>) {
    let app = authorize_all(platform).await;
    let launch = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
        )
        .await
        .unwrap();
    let session = platform
        .sessions
        .authenticate(
            &platform.cursors,
            &launch.bootstrap.session_token,
            &platform.host.auth.device_epoch(),
        )
        .unwrap();
    (app, launch, session)
}
fn location(name: &str, params: &[(&str, String)]) -> String {
    let mut path = ENDPOINTS
        .iter()
        .find(|endpoint| endpoint.name == name)
        .unwrap()
        .path
        .to_string();
    for (key, value) in params {
        path = path.replace(&format!("{{{key}}}"), value);
    }
    path
}

#[tokio::test]
async fn bytes_range_head_conditions_and_empty_files_never_fallback() {
    let (_dir, platform, fake) = fixture().await;
    let (app, launch, _) = session(&platform).await;
    let id = file(&platform, &fake, 1, "bytes.bin", b"0123456789").await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    let node = store::reference(
        &mut connection,
        &record,
        &NodeReference::Id {
            id: id.to_string(),
            content_version: None,
        },
    )
    .await
    .unwrap();
    drop(connection);
    let path = format!(
        "{}?content_version={}",
        location("files.bytes", &[("id", id.to_string())]),
        node.content_version.unwrap()
    );
    let router = http::router_platform(platform.clone());
    let head = request(
        &router,
        Method::HEAD,
        path.clone(),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[("range", "bytes=2-4")],
    )
    .await;
    assert_eq!(head.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.headers()["content-range"], "bytes 2-4/10");
    assert_eq!(head.headers()["content-length"], "3");
    assert!(to_bytes(head.into_body(), 32).await.unwrap().is_empty());
    assert_eq!(
        fake.strict_reads.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let result = request(
        &router,
        Method::GET,
        path.clone(),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[("range", "bytes=2-4")],
    )
    .await;
    let etag = result.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(result.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(to_bytes(result.into_body(), 32).await.unwrap(), b"234"[..]);
    let result = request(
        &router,
        Method::GET,
        path.clone(),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[("if-none-match", &format!("\"other\", W/{etag}"))],
    )
    .await;
    assert_eq!(result.status(), StatusCode::NOT_MODIFIED);
    let result = request(
        &router,
        Method::GET,
        path,
        &launch.bootstrap.session_token,
        Body::empty(),
        &[("range", "bytes=0-1,4-5")],
    )
    .await;
    assert_eq!(result.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(result.headers()["content-range"], "bytes */10");
    assert_eq!(
        fake.ordinary_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
#[tokio::test]
async fn session_refresh_keeps_token_and_close_revokes_only_that_page_and_lease() {
    let (_dir, platform, fake) = fixture().await;
    let (app, launch, page) = session(&platform).await;
    let other = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
        )
        .await
        .unwrap();
    let id = file(&platform, &fake, 1, "data.bin", b"data").await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    let node = store::reference(
        &mut connection,
        &record,
        &NodeReference::Id {
            id: id.to_string(),
            content_version: None,
        },
    )
    .await
    .unwrap();
    drop(connection);
    let lease = platform
        .create_lease(&page, CreateReadLeaseRequest { ttl_seconds: 60 })
        .await
        .unwrap();
    let media = platform
        .media_url(
            &page,
            MediaUrlRequest {
                file: ContentRef {
                    id: node.id,
                    content_version: node.content_version.unwrap(),
                },
                representation: FileRepresentation::Original,
                disposition: Disposition::Inline,
                filename: None,
                content_type: None,
                ttl_seconds: None,
                lease_id: Some(lease.id.clone()),
            },
            "http://localhost",
        )
        .await
        .unwrap();
    let router = http::router_platform(platform.clone());
    let refreshed = request(
        &router,
        Method::POST,
        location("session.refresh", &[]),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[],
    )
    .await;
    assert_eq!(refreshed.status(), StatusCode::OK);
    let refreshed: SessionRefreshResponse =
        serde_json::from_slice(&to_bytes(refreshed.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(refreshed.session_token, launch.bootstrap.session_token);
    let closed = request(
        &router,
        Method::DELETE,
        location("session.close", &[]),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[],
    )
    .await;
    assert_eq!(closed.status(), StatusCode::NO_CONTENT);
    let denied = request(
        &router,
        Method::GET,
        location("runtime.info", &[]),
        &launch.bootstrap.session_token,
        Body::empty(),
        &[],
    )
    .await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let allowed = request(
        &router,
        Method::GET,
        location("runtime.info", &[]),
        &other.bootstrap.session_token,
        Body::empty(),
        &[],
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let ticket = media.url.rsplit('/').next().unwrap();
    assert_eq!(
        platform
            .ticket_bytes(ticket, false, &HeaderMap::new(), false)
            .await
            .unwrap_err()
            .code,
        error_code::TICKET_EXPIRED
    );
    assert!(
        platform
            .renew_lease(&page, &lease.id, CreateReadLeaseRequest { ttl_seconds: 60 })
            .await
            .is_err()
    );
    assert_eq!(
        platform
            .installed(&app.manifest.id)
            .await
            .unwrap()
            .generation,
        app.generation
    );
}
#[tokio::test]
async fn withdrawing_scope_aborts_remaining_bytes_without_error_json() {
    let (_dir, platform, fake) = fixture().await;
    let (app, _launch, page) = session(&platform).await;
    let id = file(&platform, &fake, 1, "large.bin", &vec![7; 256 * 1024]).await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    let node = store::reference(
        &mut connection,
        &record,
        &NodeReference::Id {
            id: id.to_string(),
            content_version: None,
        },
    )
    .await
    .unwrap();
    drop(connection);
    let response = platform
        .direct_bytes(
            &page,
            ContentRef {
                id: node.id,
                content_version: node.content_version.unwrap(),
            },
            &HeaderMap::new(),
            false,
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first.len(), 64 * 1024);
    store::update_scope(
        &platform.host.vfs.db,
        &app.manifest.id,
        ScopeUpdateRequest {
            expected_revision: app.scope.revision,
            scope: DirectoryScope::None,
        },
    )
    .await
    .unwrap();
    assert!(body.frame().await.unwrap().is_err());
}
#[tokio::test]
async fn cache_sources_overwrite_tickets_and_lru_are_generic() {
    let (_dir, platform, fake) = fixture().await;
    let (app, _launch, page) = session(&platform).await;
    let id = file(&platform, &fake, 1, "source.bin", b"original").await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    let node = store::reference(
        &mut connection,
        &record,
        &NodeReference::Id {
            id: id.to_string(),
            content_version: None,
        },
    )
    .await
    .unwrap();
    drop(connection);
    let sources = vec![ContentRef {
        id: node.id,
        content_version: node.content_version.unwrap(),
    }];
    let first = platform
        .cache_put(
            &page,
            "object",
            "text/plain".into(),
            CachePutMetadata {
                metadata: serde_json::json!({"meaning":"由应用解释"}),
                sources: sources.clone(),
            },
            bytes::Bytes::from_static(b"derived"),
        )
        .await
        .unwrap();
    let hits = platform
        .cache_lookup(
            &page,
            CacheLookupRequest {
                keys: vec!["object".into()],
            },
            "http://localhost",
        )
        .await
        .unwrap();
    assert_eq!(hits.hits.len(), 1);
    let ticket = hits.hits[0].url.rsplit('/').next().unwrap();
    platform
        .cache_put(
            &page,
            "object",
            "text/plain".into(),
            CachePutMetadata {
                metadata: serde_json::Value::Null,
                sources,
            },
            bytes::Bytes::from_static(b"new"),
        )
        .await
        .unwrap();
    assert_eq!(
        platform
            .ticket_bytes(ticket, true, &HeaderMap::new(), false)
            .await
            .unwrap_err()
            .code,
        error_code::NOT_FOUND
    );
    store::update_scope(
        &platform.host.vfs.db,
        &app.manifest.id,
        ScopeUpdateRequest {
            expected_revision: app.scope.revision,
            scope: DirectoryScope::None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        platform
            .cache_read(&page, "object", false, None)
            .await
            .err()
            .unwrap()
            .code,
        error_code::NOT_FOUND
    );
    assert!(tgdrive_contract::validation::validate_cache_entry_header(&first).is_ok());
}
#[tokio::test]
async fn kv_byte_pages_do_not_skip_large_records() {
    let (_dir, platform, _) = fixture().await;
    let app = install_page(&platform).await;
    let value = serde_json::Value::String("x".repeat(MAX_KV_VALUE_BYTES as usize - 2));
    for index in 0..40 {
        platform
            .kv_put(
                &app.manifest.id,
                &app.generation,
                &format!("key-{index:03}"),
                KvPutRequest {
                    value: value.clone(),
                    condition: KvCondition::Absent,
                },
            )
            .await
            .unwrap();
    }
    let mut request: KvListRequest =
        serde_json::from_value(serde_json::json!({"limit":200})).unwrap();
    let mut total = 0;
    loop {
        let page = platform
            .kv_list(&app.manifest.id, &app.generation, request.clone())
            .await
            .unwrap();
        tgdrive_contract::validation::validate_json_page(&page.records, MAX_JSON_PAGE_BYTES)
            .unwrap();
        assert!(page.records.len() < 200);
        total += page.records.len();
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    assert_eq!(total, 40);
}
#[tokio::test]
async fn snapshot_restore_changes_generations_and_expires_old_change_cursor() {
    let (_dir, platform, _) = fixture().await;
    let app = authorize_all(&platform).await;
    let cursor = platform
        .file_changes(
            &app.manifest.id,
            &app.generation,
            FileChangesRequest {
                cursor: None,
                limit: 200,
            },
        )
        .await
        .unwrap()
        .next_cursor;
    let snapshot = tempfile::tempdir().unwrap();
    let path = snapshot.path().join("snapshot.db");
    platform
        .host
        .vfs
        .db
        .vacuum_into(path.to_str().unwrap())
        .await
        .unwrap();
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    restore::mark_snapshot(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    let db = crate::db::Db::open(path.to_str().unwrap()).await.unwrap();
    let mut host = platform.host.clone();
    host.vfs.db = db;
    let restored = AppPlatform::new(host);
    let installed = restored.installed(&app.manifest.id).await.unwrap();
    assert_ne!(installed.generation, app.generation);
    assert_eq!(
        restored
            .file_changes(
                &app.manifest.id,
                &installed.generation,
                FileChangesRequest {
                    cursor: Some(cursor),
                    limit: 200
                }
            )
            .await
            .unwrap_err()
            .code,
        error_code::CURSOR_EXPIRED
    );
}
