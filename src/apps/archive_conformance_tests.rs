//! 归档下载、旧格式、恢复轮次和失败保留的真实 SQLite 符合性验收。
use super::backend_tests::backend_fixture;
use super::*;
use sqlx::Connection;
use tgdrive_contract::{data::*, error_code, runtime::*};

#[tokio::test]
async fn failed_or_missing_application_snapshot_keeps_previous_success_and_running_backend() {
    let (_db, output, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let snapshot = output.path().join("last-success.sqlite3");
    assert!(
        snapshot::snapshot(
            &platform.host.vfs.db,
            "test-backend",
            std::path::Path::new(&run.data_dir),
            &snapshot
        )
        .await
        .unwrap()
    );
    let previous = std::fs::read(&snapshot).unwrap();
    let source = std::path::Path::new(&run.data_dir).join("state/app.sqlite3");
    let saved = source.with_file_name("saved.sqlite3");
    std::fs::rename(&source, &saved).unwrap();
    assert!(
        !snapshot::snapshot(
            &platform.host.vfs.db,
            "test-backend",
            std::path::Path::new(&run.data_dir),
            &snapshot
        )
        .await
        .unwrap()
    );
    assert_eq!(std::fs::read(&snapshot).unwrap(), previous);
    std::os::unix::fs::symlink(&saved, &source).unwrap();
    assert!(
        snapshot::snapshot(
            &platform.host.vfs.db,
            "test-backend",
            std::path::Path::new(&run.data_dir),
            &snapshot
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&snapshot).unwrap(), previous);
    assert!(!run.caller.cancelled.is_cancelled());
    assert!(!run.finished.is_cancelled());
    std::fs::remove_file(&source).unwrap();
    std::fs::rename(&saved, &source).unwrap();
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn downloaded_plain_archive_restores_kv_application_database_and_fresh_generations_offline() {
    let (_db, output, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    platform
        .kv_put(
            "test-backend",
            &run.caller.generation,
            "progress",
            KvPutRequest {
                value: serde_json::json!({"saved":true}),
                condition: KvCondition::Absent,
            },
        )
        .await
        .unwrap();
    platform
        .host
        .vfs
        .db
        .setting_set(
            "extra.backup_encryption_password",
            "download-must-remain-plain",
        )
        .await
        .unwrap();
    let response = crate::backup::download(&platform.host.vfs, &platform.host.rt)
        .await
        .unwrap();
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .ends_with(".tar\"")
    );
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let backup = output.path().join("download.tar");
    std::fs::write(&backup, bytes).unwrap();
    let old_generation = run.caller.generation.clone();
    platform.backends.stop("test-backend").await.unwrap();
    let cache = platform
        .data_root()
        .join("test-backend/cache/work/old-cache");
    std::fs::write(&cache, b"discarded").unwrap();
    let env = platform.host.rt.env.clone();
    platform.host.vfs.db.close().await;
    crate::restore::restore_file(&env, &backup).await.unwrap();
    assert!(!cache.exists());
    let restored = crate::db::Db::open(&env.db_path).await.unwrap();
    let generation: String =
        sqlx::query_scalar("SELECT generation FROM app_installations WHERE id='test-backend'")
            .fetch_one(&restored.pool)
            .await
            .unwrap();
    assert_ne!(generation, old_generation);
    let value: String = sqlx::query_scalar(
        "SELECT value FROM app_kv WHERE app_id='test-backend' AND key='progress'",
    )
    .fetch_one(&restored.pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&value).unwrap(),
        serde_json::json!({"saved":true})
    );
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(platform.data_root().join("test-backend/state/app.sqlite3"))
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM fixture LIMIT 1")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        "在线快照测试"
    );
    connection.close().await.unwrap();
    restored.close().await;
    assert!(
        crate::restore::RestoreManager::new(&env.db_path)
            .unwrap()
            .status()
            .await
            .safe_mode
    );
}

#[tokio::test]
async fn generation_changes_after_host_snapshot_reject_mixed_application_archives() {
    let (_db, output, platform, _) = backend_fixture().await;
    platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let main = output.path().join("host.sqlite3");
    platform
        .host
        .vfs
        .db
        .vacuum_into(main.to_str().unwrap())
        .await
        .unwrap();
    let app = platform.installed("test-backend").await.unwrap();
    platform
        .state_patch(
            "test-backend",
            AppStatePatch {
                expected_state_revision: app.state_revision,
                enabled: Some(false),
                pinned: None,
            },
        )
        .await
        .unwrap();
    let previous = output.path().join("last-success.tar");
    std::fs::write(&previous, b"last successful backup").unwrap();
    assert_eq!(
        archive::create(
            &platform.host.vfs.db,
            &platform.host.rt.env.db_path,
            &main,
            &previous
        )
        .await
        .unwrap_err()
        .code,
        error_code::APP_CHANGED
    );
    assert_eq!(std::fs::read(&previous).unwrap(), b"last successful backup");
}
