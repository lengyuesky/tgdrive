//! 在线应用库快照和主库归档的真实管道/SQLite 验收。
use super::backend_tests::backend_fixture;
use super::*;
use sqlx::Connection;

#[tokio::test]
async fn online_snapshot_and_archive_preserve_application_database_without_stopping_backend() {
    let (_db, data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let snapshot = data.path().join("application.sqlite3");
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
    assert!(!run.caller.cancelled.is_cancelled());
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&snapshot)
            .read_only(true),
    )
    .await
    .unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM fixture LIMIT 1")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(value, "在线快照测试");
    connection.close().await.unwrap();
    let main = data.path().join("main.sqlite3");
    platform
        .host
        .vfs
        .db
        .vacuum_into(main.to_str().unwrap())
        .await
        .unwrap();
    let package = data.path().join("backup.tar");
    archive::create(
        &platform.host.vfs.db,
        &platform.host.rt.env.db_path,
        &main,
        &package,
    )
    .await
    .unwrap();
    assert!(!run.caller.cancelled.is_cancelled());
    let extracted = tempfile::tempdir().unwrap();
    let host = extracted.path().join("host.db");
    let apps = extracted.path().join("apps");
    assert!(archive::extract(&package, &host, &apps).await.unwrap());
    assert!(apps.join("test-backend.sqlite3").is_file());
    let digest = archive::bundle_digest(&host, &apps).await.unwrap();
    let restored = crate::db::Db::open(host.to_str().unwrap()).await.unwrap();
    archive::restore_apps(&restored, host.to_str().unwrap(), Some(&apps))
        .await
        .unwrap();
    let database = extracted
        .path()
        .join("apps-v1/data/test-backend/state/app.sqlite3");
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(database)
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM fixture LIMIT 1")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        value
    );
    connection.close().await.unwrap();
    restored.close().await;
    // 应用库篡改必须使整份候选失效，不能只校验主库摘要。
    std::fs::write(apps.join("test-backend.sqlite3"), b"changed").unwrap();
    assert!(archive::bundle_digest(&host, &apps).await.is_err());
    assert!(!digest.is_empty());
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn old_single_database_is_accepted_and_archive_links_are_rejected() {
    let (_db, data, platform, _) = backend_fixture().await;
    let main = data.path().join("old.db");
    platform
        .host
        .vfs
        .db
        .vacuum_into(main.to_str().unwrap())
        .await
        .unwrap();
    let output = data.path().join("output.db");
    assert!(
        !archive::extract(&main, &output, &data.path().join("empty-apps"))
            .await
            .unwrap()
    );
    let bad = data.path().join("bad.tar");
    let mut builder = tar::Builder::new(std::fs::File::create(&bad).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o600);
    header.set_link_name("/outside").unwrap();
    header.set_cksum();
    builder
        .append_data(&mut header, "host.sqlite3", std::io::empty())
        .unwrap();
    builder.finish().unwrap();
    assert!(
        archive::extract(
            &bad,
            &data.path().join("rejected.db"),
            &data.path().join("rejected-apps")
        )
        .await
        .is_err()
    );
}
