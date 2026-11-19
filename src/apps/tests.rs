//! macOS 可运行的真实 HTTP/SQLite 符合性；包和网盘字节替身都只在测试中构造。

use super::*;
use crate::api::AppState;
use crate::auth::Auth;
use crate::config::Config;
use crate::db::Db;
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::settings::{Runtime, RuntimeCfg};
use crate::telegram::{ByteReader, TgApi, TgMessage, TgRegistry, UploadBody};
use crate::vfs::Vfs;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tgdrive_contract::{data::*, files::*, runtime::*};
use tower::ServiceExt;

#[derive(Default)]
pub(crate) struct Synthetic {
    pub contents: Mutex<BTreeMap<String, Vec<u8>>>,
    pub strict_reads: AtomicUsize,
    pub ordinary_reads: AtomicUsize,
}
#[async_trait::async_trait]
impl TgApi for Synthetic {
    async fn send_document(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<&bytes::Bytes>,
        _: &UploadBody,
    ) -> crate::error::AppResult<TgMessage> {
        Err(crate::error::AppError::BadRequest(
            "测试不上传 Telegram".into(),
        ))
    }
    async fn delete_message(&self, _: i64) -> crate::error::AppResult<()> {
        Ok(())
    }
    async fn get_me(&self) -> crate::error::AppResult<String> {
        Ok("测试存储".into())
    }
    async fn open_chunk_stream(
        &self,
        file: &str,
        _: i64,
        _: u64,
        offset: u64,
    ) -> crate::error::AppResult<ByteReader> {
        self.ordinary_reads.fetch_add(1, Ordering::SeqCst);
        let contents = self.contents.lock().unwrap();
        let body = contents.get(file).ok_or(crate::error::AppError::NotFound)?;
        Ok(Box::pin(std::io::Cursor::new(
            body[offset as usize..].to_vec(),
        )))
    }
    async fn open_chunk_strict_range(
        &self,
        file: &str,
        _: i64,
        _: u64,
        offset: u64,
        length: u64,
        budget: Option<Arc<crate::telegram::ReadBudget>>,
    ) -> crate::error::AppResult<ByteReader> {
        self.strict_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(budget) = budget {
            budget.reserve(length)?;
        }
        let contents = self.contents.lock().unwrap();
        let body = contents.get(file).ok_or(crate::error::AppError::NotFound)?;
        Ok(Box::pin(std::io::Cursor::new(
            body[offset as usize..(offset + length) as usize].to_vec(),
        )))
    }
}

pub(crate) async fn fixture() -> (tempfile::TempDir, Arc<AppPlatform>, Arc<Synthetic>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let env = Arc::new(Config {
        proxy: Default::default(),
        listen: "127.0.0.1:0".parse().unwrap(),
        bot_token: String::new(),
        upload_bot_tokens: vec![],
        chat_id: String::new(),
        api_base: "http://127.0.0.1:1".into(),
        chunk_size: 1024 * 1024,
        upload_concurrency: 1,
        download_concurrency: 1,
        db_path: path.join("test.db").to_string_lossy().into_owned(),
        tmp_dir: path.to_path_buf(),
        local_data_root: path.to_path_buf(),
        username: "test".into(),
        password: "test-password".into(),
        secret: None,
        cookie_secure: false,
        trusted_proxies: vec![],
        telegram_api_id: String::new(),
        telegram_api_hash: String::new(),
        send_video: false,
    });
    let db = Db::open(&env.db_path).await.unwrap();
    let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
    let fake = Arc::new(Synthetic::default());
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "test-token", "test-chat")
        .await
        .unwrap();
    let tg = Arc::new(TgRegistry::with_client(target.id, fake.clone()));
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        path.to_path_buf(),
        1,
        false,
    ));
    let auth = Arc::new(
        Auth::init("test", "test-password", None, false, &db)
            .await
            .unwrap(),
    );
    let rt = Arc::new(Runtime::new(
        db.clone(),
        env.clone(),
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
    let host = AppState {
        dav: crate::dav::build_handler(vfs.clone()),
        vfs,
        auth,
        rt,
        attempts: Default::default(),
        tasks: crate::tasks::TaskManager::new(),
        migration: crate::migration::StorageMigrationManager::new(db, tg),
        restore: crate::restore::RestoreManager::new(&env.db_path).unwrap(),
        events: crate::events::bus(),
    };
    (
        dir,
        Arc::new(AppPlatform::with_repository(host, String::new())),
        fake,
    )
}
pub(crate) async fn install_page(platform: &AppPlatform) -> InstalledApp {
    let upload = tempfile::tempdir().unwrap();
    let path = upload.path().join("input.tgapp");
    std::fs::write(&path, package::tests::page_package("1.0.0")).unwrap();
    let package = platform.upload_package(&path).await.unwrap();
    let preview = platform
        .preview(InstallPreviewRequest {
            source: InstallSource::Uploaded {
                package_id: package.package_id,
            },
        })
        .await
        .unwrap();
    platform
        .install(InstallRequest {
            preview_id: preview.preview_id,
        })
        .await
        .unwrap()
}
pub(crate) async fn authorize_all(platform: &AppPlatform) -> InstalledApp {
    let app = install_page(platform).await;
    store::update_scope(
        &platform.host.vfs.db,
        &app.manifest.id,
        ScopeUpdateRequest {
            expected_revision: app.scope.revision,
            scope: DirectoryScope::All {
                access: AccessLevel::ReadOnly,
            },
        },
    )
    .await
    .unwrap();
    platform.installed(&app.manifest.id).await.unwrap()
}
pub(crate) async fn file(
    platform: &AppPlatform,
    fake: &Synthetic,
    parent: i64,
    name: &str,
    body: &[u8],
) -> i64 {
    let db = &platform.host.vfs.db;
    let id = db.create_uploading(parent, name).await.unwrap();
    let file_id = format!("file-{id}");
    fake.contents
        .lock()
        .unwrap()
        .insert(file_id.clone(), body.to_vec());
    let target = platform.host.vfs.tg.current().unwrap().storage_target_id;
    db.add_chunk(id, 0, body.len() as i64, &file_id, id, target, None)
        .await
        .unwrap();
    db.finalize_upload(id, body.len() as i64, None)
        .await
        .unwrap();
    id
}
fn query() -> FileQuery {
    serde_json::from_value(serde_json::json!({})).unwrap()
}

#[tokio::test]
async fn install_preview_is_exact_and_pinning_does_not_revoke_generation() {
    let (_dir, platform, _) = fixture().await;
    let app = install_page(&platform).await;
    assert!(matches!(app.scope.scope, DirectoryScope::None));
    let pinned = platform
        .state_patch(
            &app.manifest.id,
            AppStatePatch {
                expected_state_revision: app.state_revision.clone(),
                enabled: None,
                pinned: Some(true),
            },
        )
        .await
        .unwrap();
    assert_eq!(pinned.generation, app.generation);
    assert!(
        platform
            .state_patch(
                &app.manifest.id,
                AppStatePatch {
                    expected_state_revision: app.state_revision,
                    enabled: Some(false),
                    pinned: None
                }
            )
            .await
            .is_err()
    );
    let disabled = platform
        .state_patch(
            &pinned.manifest.id,
            AppStatePatch {
                expected_state_revision: pinned.state_revision,
                enabled: Some(false),
                pinned: None,
            },
        )
        .await
        .unwrap();
    assert_ne!(disabled.generation, pinned.generation);
}
#[tokio::test]
async fn identity_scopes_overlap_and_follow_renamed_roots() {
    let (_dir, platform, fake) = fixture().await;
    let app = install_page(&platform).await;
    let db = &platform.host.vfs.db;
    let root = db.mkdir(1, "a").await.unwrap();
    let sub = db.mkdir(root.id, "b").await.unwrap();
    let allowed = file(&platform, &fake, sub.id, "a.bin", b"file").await;
    let other = db.mkdir(1, "ab").await.unwrap();
    let denied = file(&platform, &fake, other.id, "b.bin", b"hidden").await;
    let scope = store::update_scope(
        db,
        &app.manifest.id,
        ScopeUpdateRequest {
            expected_revision: app.scope.revision,
            scope: DirectoryScope::Selected {
                directories: vec![
                    DirectoryGrant {
                        directory_id: root.id.to_string(),
                        access: AccessLevel::ReadWrite,
                    },
                    DirectoryGrant {
                        directory_id: sub.id.to_string(),
                        access: AccessLevel::ReadOnly,
                    },
                ],
            },
        },
    )
    .await
    .unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    assert_eq!(
        store::access(&mut connection, &record, allowed)
            .await
            .unwrap(),
        AccessLevel::ReadWrite
    );
    assert!(
        store::access(&mut connection, &record, denied)
            .await
            .is_err()
    );
    drop(connection);
    db.move_node(root.id, 1, "renamed").await.unwrap();
    let updated = platform.app_scope(&app.manifest.id).await.unwrap();
    assert_ne!(updated.revision, scope.revision);
    assert_eq!(updated.roots[0].node.as_ref().unwrap().path, "/renamed");
    let replacement = db.mkdir(1, "a").await.unwrap();
    let hidden = file(&platform, &fake, replacement.id, "replacement.bin", b"no").await;
    let mut connection = db.pool.acquire().await.unwrap();
    let record = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    assert!(
        store::access(&mut connection, &record, hidden)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn pages_are_keysets_directories_match_only_direct_files_and_changes_advance() {
    let (_dir, platform, fake) = fixture().await;
    let app = authorize_all(&platform).await;
    let db = &platform.host.vfs.db;
    let top = db.mkdir(1, "top").await.unwrap();
    let chapter = db.mkdir(top.id, "chapter").await.unwrap();
    for index in 0..13 {
        file(
            &platform,
            &fake,
            chapter.id,
            &format!("{index:03}.jpg"),
            b"image",
        )
        .await;
    }
    let mut request = query();
    request.limit = 5;
    let page = platform
        .file_query(&app.manifest.id, &app.generation, request.clone())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 5);
    request.cursor = page.next_cursor.clone();
    let second = platform
        .file_query(&app.manifest.id, &app.generation, request.clone())
        .await
        .unwrap();
    assert_eq!(second.items.len(), 5);
    assert_eq!(page.scan_cursor, second.scan_cursor);
    assert!(
        !page
            .items
            .iter()
            .any(|node| second.items.iter().any(|other| node.id == other.id))
    );
    request.limit = 4;
    assert_eq!(
        platform
            .file_query(&app.manifest.id, &app.generation, request)
            .await
            .unwrap_err()
            .code,
        tgdrive_contract::error_code::INVALID_CURSOR
    );
    let directories: DirectoriesWithFilesQuery =
        serde_json::from_value(serde_json::json!({"file_filter":{"extensions":["jpg"]}})).unwrap();
    let page = platform
        .directories_with_files(&app.manifest.id, &app.generation, directories)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, chapter.id.to_string());
    let checkpoint = platform
        .file_changes(
            &app.manifest.id,
            &app.generation,
            FileChangesRequest {
                cursor: None,
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(checkpoint.reset_required);
    let file = file(&platform, &fake, 1, "new.bin", b"data").await;
    let changes = platform
        .file_changes(
            &app.manifest.id,
            &app.generation,
            FileChangesRequest {
                cursor: Some(checkpoint.next_cursor),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(
        changes
            .changes
            .iter()
            .any(|change| matches!(change,FileChange::Upsert{node} if node.id==file.to_string()))
    );
}
#[tokio::test]
async fn kv_cas_prevents_aba_and_uninstall_preserves_data_by_default() {
    let (_dir, platform, _) = fixture().await;
    let app = install_page(&platform).await;
    let first = platform
        .kv_put(
            &app.manifest.id,
            &app.generation,
            "progress",
            KvPutRequest {
                value: serde_json::Value::Null,
                condition: KvCondition::Absent,
            },
        )
        .await
        .unwrap();
    assert!(
        platform
            .kv_get(&app.manifest.id, &app.generation, "progress")
            .await
            .unwrap()
            .record
            .unwrap()
            .value
            .is_null()
    );
    assert_eq!(
        platform
            .kv_put(
                &app.manifest.id,
                &app.generation,
                "progress",
                KvPutRequest {
                    value: serde_json::json!(1),
                    condition: KvCondition::Absent
                }
            )
            .await
            .unwrap_err()
            .code,
        tgdrive_contract::error_code::STORAGE_CONFLICT
    );
    platform
        .kv_delete(
            &app.manifest.id,
            &app.generation,
            "progress",
            KvDeleteRequest {
                revision: first.revision.clone(),
            },
        )
        .await
        .unwrap();
    let second = platform
        .kv_put(
            &app.manifest.id,
            &app.generation,
            "progress",
            KvPutRequest {
                value: serde_json::json!(2),
                condition: KvCondition::Absent,
            },
        )
        .await
        .unwrap();
    assert_ne!(first.revision, second.revision);
    assert!(
        platform
            .kv_delete(
                &app.manifest.id,
                &app.generation,
                "progress",
                KvDeleteRequest {
                    revision: first.revision
                }
            )
            .await
            .is_err()
    );
    platform
        .uninstall(&app.manifest.id, &app.state_revision, false)
        .await
        .unwrap();
    let reinstalled = install_page(&platform).await;
    assert_ne!(reinstalled.generation, app.generation);
    assert_eq!(
        platform
            .kv_get(&app.manifest.id, &reinstalled.generation, "progress")
            .await
            .unwrap()
            .record
            .unwrap()
            .value,
        serde_json::json!(2)
    );
}
#[tokio::test]
async fn management_requires_cookie_and_page_cors_has_no_credentials() {
    let (_dir, platform, _) = fixture().await;
    let app = install_page(&platform).await;
    let launch = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
        )
        .await
        .unwrap();
    let router = http::router(platform.host.clone());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(tgdrive_contract::http::APP_CENTER_BASE.to_owned() + "/apps")
                .header(
                    "authorization",
                    format!("Bearer {}", launch.bootstrap.session_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri(format!(
                    "{}/files/query",
                    tgdrive_contract::http::SELF_API_BASE
                ))
                .header("origin", "null")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["access-control-allow-origin"], "null");
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-credentials")
    );
    assert!(response.headers().contains_key("x-tgdrive-request-id"));
    assert!(to_bytes(response.into_body(), 32).await.unwrap().is_empty());
}
