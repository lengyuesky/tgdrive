use super::*;
use std::io::Write;
use zip::write::SimpleFileOptions;

fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in files {
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(contents).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn manifest() -> Manifest {
    serde_json::from_str(include_str!("../../../apps/shorts/app.json")).unwrap()
}

#[test]
fn rejects_traversal_duplicate_case_and_missing_entry() {
    let metadata = serde_json::to_vec(&manifest()).unwrap();
    for name in [
        "../escape.js",
        "/escape.js",
        "assets/../../escape.js",
        "assets\\escape.js",
        "assets/%2e%2e/escape.js",
        ".hidden.js",
    ] {
        let bytes = archive(&[
            ("app.json", &metadata),
            ("index.html", b"<html></html>"),
            (name, b"x"),
        ]);
        assert!(AppPackage::parse(bytes).is_err(), "拒绝路径 {name}");
    }
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", &metadata),
            ("index.html", b"x"),
            ("INDEX.HTML", b"y")
        ]))
        .is_err()
    );
    assert!(AppPackage::parse(archive(&[("app.json", &metadata)])).is_err());
}

#[test]
fn rejects_zip_bombs_and_symlinks() {
    let metadata = serde_json::to_vec(&manifest()).unwrap();
    let oversized = vec![0; MAX_FILE_SIZE + 1];
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", &metadata),
            ("index.html", &oversized)
        ]))
        .is_err()
    );
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_symlink("index.html", "../../secret", SimpleFileOptions::default())
        .unwrap();
    assert!(AppPackage::parse(zip.finish().unwrap().into_inner()).is_err());
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..257 {
        zip.start_file(format!("file{index}.txt"), SimpleFileOptions::default())
            .unwrap();
    }
    assert!(AppPackage::parse(zip.finish().unwrap().into_inner()).is_err());
}

#[test]
fn checks_compatibility_permissions_and_settings() {
    let mut app = manifest();
    assert!(app.validate().is_ok());
    app.permissions.push("system.exec".into());
    assert!(app.validate().is_err());
    app = manifest();
    app.api_version = 99;
    assert!(app.validate().is_err());
    app = manifest();
    app.min_host_version = "999.0.0".into();
    assert!(app.validate().is_err());
    app = manifest();
    assert_eq!(
        app.validate_patch(serde_json::json!({"source_dir": "影视//短片/"}))
            .unwrap()["source_dir"],
        "/影视/短片"
    );
    assert!(
        app.validate_patch(serde_json::json!({"source_dir": "/a/../b"}))
            .is_err()
    );
    assert!(
        app.validate_patch(serde_json::json!({"muted": "false"}))
            .is_err()
    );
    assert!(
        app.validate_patch(serde_json::json!({"bot_token": "x"}))
            .is_err()
    );
    assert!(
        app.validate_patch(serde_json::json!({"muted": null}))
            .is_err()
    );
    assert_eq!(
        app.settings_values(r#"{"source_dir":"/旧目录","muted":"旧类型"}"#)
            .unwrap()["muted"],
        true
    );
}

#[test]
fn materializes_complete_package_without_touching_siblings() {
    let metadata = serde_json::to_vec(&manifest()).unwrap();
    let package = AppPackage::parse(archive(&[
        ("app.json", &metadata),
        ("index.html", b"test"),
        ("assets/main.js", b"42"),
    ]))
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("keep"), "保留").unwrap();
    let destination = package.materialize(root.path()).unwrap();
    assert_eq!(
        std::fs::read(destination.join("assets/main.js")).unwrap(),
        b"42"
    );
    assert!(destination.join(".complete").is_file());
    assert_eq!(package.materialize(root.path()).unwrap(), destination);
    assert_eq!(
        std::fs::read_to_string(root.path().join("keep")).unwrap(),
        "保留"
    );
}

#[tokio::test]
async fn migrates_legacy_settings_without_installing_shorts() {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
        INSERT INTO settings VALUES ('extra.shorts_dir', '/旧短片目录');",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0014_apps.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let saved: String =
        sqlx::query_scalar("SELECT settings FROM app_settings WHERE app_id = 'shorts'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&saved).unwrap()["source_dir"],
        "/旧短片目录"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let legacy: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM settings WHERE k = 'extra.shorts_dir'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy, 0);
}
