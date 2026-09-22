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
    Manifest {
        id: "test-app".into(),
        name: "测试应用".into(),
        version: "1.0.0".into(),
        api_version: 2,
        min_host_version: "0.1.0".into(),
        description: "自包含测试应用".into(),
        author: "测试作者".into(),
        entry: "index.html".into(),
        icon: None,
        permissions: vec!["files.read".into()],
        settings: vec![
            SettingField {
                key: "source_dir".into(),
                label: "目录".into(),
                description: "测试目录".into(),
                kind: "directory".into(),
                default: Value::String("/test".into()),
            },
            SettingField {
                key: "muted".into(),
                label: "静音".into(),
                description: "静音测试".into(),
                kind: "boolean".into(),
                default: Value::Bool(true),
            },
        ],
    }
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
    let script = "export const answer = 42; // tgdrive package test\n".repeat(32);
    let package = AppPackage::parse(archive(&[
        ("app.json", &metadata),
        ("index.html", b"test"),
        ("assets/main.js", b"42"),
        ("assets/vendor.js", script.as_bytes()),
        ("assets/photo.jpg", &[0u8; 2048]),
    ]))
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("keep"), "保留").unwrap();
    let destination = package.materialize(root.path()).unwrap();
    assert_eq!(
        std::fs::read(destination.join("assets/main.js")).unwrap(),
        b"42"
    );
    assert!(destination.join(COMPLETE_MARK).is_file());
    assert!(is_materialized(&destination));
    assert_eq!(package.materialize(root.path()).unwrap(), destination);
    assert_eq!(
        std::fs::read_to_string(root.path().join("keep")).unwrap(),
        "保留"
    );
    // 可压缩的大文件同时生成 brotli 与 gzip 变体，解压后与原文一致；
    // 过小的文件与已压缩的图片不生成变体。
    let encoded = destination.join(ENCODED_DIR);
    let gz = std::fs::read(encoded.join("gz/assets/vendor.js")).unwrap();
    assert!(gz.len() * 10 <= script.len() * 9);
    let mut inflated = Vec::new();
    Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut inflated).unwrap();
    assert_eq!(inflated, script.as_bytes());
    let br = std::fs::read(encoded.join("br/assets/vendor.js")).unwrap();
    let mut decoded = Vec::new();
    brotli::BrotliDecompress(&mut &br[..], &mut decoded).unwrap();
    assert_eq!(decoded, script.as_bytes());
    assert!(!encoded.join("gz/assets/main.js").exists());
    assert!(!encoded.join("br/assets/photo.jpg").exists());
    assert!(!encoded.join("gz/index.html").exists());

    // 旧目录只缺变体时原地补齐，不重新解包。
    std::fs::remove_dir_all(&encoded).unwrap();
    std::fs::write(destination.join("assets/main.js"), b"43").unwrap();
    assert_eq!(package.materialize(root.path()).unwrap(), destination);
    assert!(is_materialized(&destination));
    assert_eq!(
        std::fs::read(destination.join("assets/main.js")).unwrap(),
        b"43"
    );
}

#[test]
fn icon_field_accepts_bundled_svg_and_rejects_invalid_declarations() {
    let mut valid = manifest();
    valid.icon = Some("icon.svg".into());
    let metadata = serde_json::to_vec(&valid).unwrap();
    let package = AppPackage::parse(archive(&[
        ("app.json", &metadata),
        ("index.html", b"test"),
        (
            "icon.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
        ),
    ]))
    .unwrap();
    assert_eq!(package.manifest.icon.as_deref(), Some("icon.svg"));
    assert!(package.files.contains_key("icon.svg"));

    // 图标缺文件时必须拒绝。
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", metadata.as_slice()),
            ("index.html", b"test"),
        ]))
        .is_err()
    );

    // 非 SVG 后缀由清单校验拒绝。
    let mut wrong_type = manifest();
    wrong_type.icon = Some("icon.png".into());
    let wrong_type = serde_json::to_vec(&wrong_type).unwrap();
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", wrong_type.as_slice()),
            ("index.html", b"test"),
            ("icon.png", b"x"),
        ]))
        .is_err()
    );

    // 超过 256 KiB 的图标拒绝。
    let mut oversize = manifest();
    oversize.icon = Some("icon.svg".into());
    let oversize = serde_json::to_vec(&oversize).unwrap();
    let huge = vec![b' '; 256 * 1024 + 1];
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", oversize.as_slice()),
            ("index.html", b"test"),
            ("icon.svg", &huge),
        ]))
        .is_err()
    );

    // 未声明 icon 的旧包继续可用。
    let legacy = serde_json::to_vec(&manifest()).unwrap();
    assert!(
        AppPackage::parse(archive(&[
            ("app.json", legacy.as_slice()),
            ("index.html", b"test"),
        ]))
        .is_ok()
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
