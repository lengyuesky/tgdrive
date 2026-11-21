//! 备份归档只包含主库和固定应用库；路径、长度、摘要及数据库分别校验。
use super::backend_system as system;
use super::{PlatformError, PlatformResult};
use crate::db::Db;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tgdrive_contract::{ByteCount, error_code, validation::*};

const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_APPS: usize = 4096;
#[derive(Clone, Serialize, Deserialize)]
struct Blob {
    size: ByteCount,
    sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    host: Blob,
    apps: BTreeMap<String, Option<Blob>>,
}
fn failure() -> PlatformError {
    PlatformError::new(
        error_code::STORAGE_UNAVAILABLE,
        "备份归档或应用数据库校验失败",
    )
}
fn data_root(path: &str) -> PathBuf {
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = if parent.is_absolute() {
        parent.to_owned()
    } else {
        std::env::current_dir()
            .expect("宿主目录可读取")
            .join(parent)
    };
    parent.join("apps-v1/data")
}
async fn blob(path: &Path) -> PlatformResult<Blob> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut sha = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > MAX_BYTES {
            return Err(failure());
        }
        sha.update(&buffer[..read]);
    }
    Ok(Blob {
        size: size.into(),
        sha256: crate::engine::upload::to_hex(&sha.finalize()),
    })
}
pub(crate) async fn has_databases(db: &Db, db_path: &str) -> PlatformResult<bool> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT app_id FROM app_runtime_identities")
        .fetch_all(&db.pool)
        .await?;
    Ok(ids.into_iter().any(|id| {
        valid_app_id(&id)
            && std::fs::symlink_metadata(
                data_root(db_path)
                    .join(id)
                    .join(tgdrive_contract::manifest::DATABASE_RELATIVE_PATH),
            )
            .is_ok()
    }))
}
async fn namespaces(main: &Path) -> PlatformResult<BTreeSet<String>> {
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(main)
            .read_only(true)
            .pragma("trusted_schema", "OFF"),
    )
    .await?;
    let mut ids = BTreeSet::new();
    let rows: Vec<String> = sqlx::query_scalar("SELECT id FROM app_installations WHERE json_type(manifest,'$.backend')='object' UNION SELECT app_id FROM app_runtime_identities").fetch_all(&mut connection).await?;
    connection.close().await?;
    for id in rows {
        if !valid_app_id(&id) {
            return Err(failure());
        }
        ids.insert(id);
    }
    if ids.len() > MAX_APPS {
        return Err(failure());
    }
    Ok(ids)
}
/// 各数据库各自一致，安装轮次变化则拒绝本轮，而不是配对不同版本的应用 schema。
pub(crate) async fn snapshot_apps(
    db: &Db,
    db_path: &str,
    main: &Path,
    output: &Path,
) -> PlatformResult<()> {
    tokio::fs::create_dir_all(output).await?;
    use sqlx::Connection;
    let mut snapshot_connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(main)
            .read_only(true)
            .pragma("trusted_schema", "OFF"),
    )
    .await?;
    let generations: Vec<(String, String)> =
        sqlx::query_as("SELECT id,generation FROM app_installations")
            .fetch_all(&mut snapshot_connection)
            .await?;
    snapshot_connection.close().await?;
    let generations = generations.into_iter().collect::<BTreeMap<_, _>>();
    let mut apps = BTreeMap::new();
    for id in namespaces(main).await? {
        let before: Option<String> =
            sqlx::query_scalar("SELECT generation FROM app_installations WHERE id=?")
                .bind(&id)
                .fetch_optional(&db.pool)
                .await?;
        if before.as_ref() != generations.get(&id) {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "主库快照之后应用安装轮次已改变",
            ));
        }
        let path = output.join(format!("{id}.sqlite3"));
        let exists =
            super::snapshot::snapshot(db, &id, &data_root(db_path).join(&id), &path).await?;
        let after: Option<String> =
            sqlx::query_scalar("SELECT generation FROM app_installations WHERE id=?")
                .bind(&id)
                .fetch_optional(&db.pool)
                .await?;
        if before != after {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "应用安装轮次在快照期间改变",
            ));
        }
        apps.insert(
            id,
            if exists {
                Some(blob(&path).await?)
            } else {
                None
            },
        );
    }
    let manifest = Manifest {
        format: 1,
        host: blob(main).await?,
        apps,
    };
    tokio::fs::write(output.join("manifest.json"), serde_json::to_vec(&manifest)?).await?;
    Ok(())
}
pub(crate) async fn create(
    db: &Db,
    db_path: &str,
    main: &Path,
    destination: &Path,
) -> PlatformResult<()> {
    let staging = tempfile::tempdir()?;
    snapshot_apps(db, db_path, main, staging.path()).await?;
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(staging.path().join("manifest.json"))?)?;
    let mut total = manifest.host.size.0;
    for value in manifest.apps.values().flatten() {
        total = total.checked_add(value.size.0).ok_or_else(failure)?;
    }
    if total > MAX_BYTES {
        return Err(failure());
    }
    let source = main.to_owned();
    let destination = destination.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut archive = tar::Builder::new(std::fs::File::create(destination)?);
        append(
            &mut archive,
            "manifest.json",
            &staging.path().join("manifest.json"),
        )?;
        append(&mut archive, "host.sqlite3", &source)?;
        for (id, value) in &manifest.apps {
            if value.is_some() {
                append(
                    &mut archive,
                    &format!("apps/{id}/app.sqlite3"),
                    &staging.path().join(format!("{id}.sqlite3")),
                )?;
            }
        }
        let file = archive.into_inner()?;
        file.sync_all()?;
        Ok::<_, PlatformError>(())
    })
    .await
    .map_err(|_| failure())??;
    Ok(())
}
fn append(
    archive: &mut tar::Builder<std::fs::File>,
    name: &str,
    source: &Path,
) -> PlatformResult<()> {
    let mut file = std::fs::File::open(source)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(file.metadata()?.len());
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    archive.append_data(&mut header, name, &mut file)?;
    Ok(())
}
/// 旧 SQLite 单文件仍可恢复；归档只接受固定普通文件，不调用 tar 的任意路径解包。
pub(crate) async fn extract(source: &Path, main: &Path, apps: &Path) -> PlatformResult<bool> {
    let mut header = [0u8; 16];
    std::fs::File::open(source)?.read_exact(&mut header)?;
    if &header == b"SQLite format 3\0" {
        std::fs::copy(source, main)?;
        return Ok(false);
    }
    std::fs::create_dir_all(apps)?;
    let source = source.to_owned();
    let main = main.to_owned();
    let directory = apps.to_owned();
    let verify_main = main.clone();
    tokio::task::spawn_blocking(move || {
        let mut archive = tar::Archive::new(std::fs::File::open(source)?);
        let mut names = BTreeSet::new();
        let mut total = 0u64;
        for entry in archive.entries()?.raw(true) {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                return Err(failure());
            }
            let path = entry.path()?.to_str().ok_or_else(failure)?.to_owned();
            if !names.insert(path.clone()) || names.len() > MAX_APPS + 2 {
                return Err(failure());
            }
            let size = entry.header().size()?;
            total = total.checked_add(size).ok_or_else(failure)?;
            if total > MAX_BYTES {
                return Err(failure());
            }
            let output = match path.as_str() {
                "host.sqlite3" => main.clone(),
                "manifest.json" => {
                    if size > 1024 * 1024 {
                        return Err(failure());
                    }
                    directory.join("manifest.json")
                }
                _ => {
                    let id = path
                        .strip_prefix("apps/")
                        .and_then(|path| path.strip_suffix("/app.sqlite3"))
                        .filter(|id| valid_app_id(id))
                        .ok_or_else(failure)?;
                    directory.join(format!("{id}.sqlite3"))
                }
            };
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            let copied = std::io::copy(&mut entry, &mut file)?;
            if copied != size {
                return Err(failure());
            }
            file.flush()?;
            file.sync_all()?;
        }
        if !names.contains("host.sqlite3") || !names.contains("manifest.json") {
            return Err(failure());
        }
        let bytes = std::fs::read(directory.join("manifest.json"))?;
        let value =
            tgdrive_contract::validation::parse_unique_json(&bytes).map_err(|_| failure())?;
        let manifest: Manifest = serde_json::from_value(value)?;
        if manifest.format != 1 || manifest.apps.len() > MAX_APPS {
            return Err(failure());
        }
        for (id, data) in &manifest.apps {
            if !valid_app_id(id)
                || data
                    .as_ref()
                    .is_some_and(|data| !valid_sha256(&data.sha256) || data.size.0 == 0)
            {
                return Err(failure());
            }
            if data.is_some() && !names.contains(&format!("apps/{id}/app.sqlite3")) {
                return Err(failure());
            }
            if data.is_none() && names.contains(&format!("apps/{id}/app.sqlite3")) {
                return Err(failure());
            }
        }
        if names.len() != 2 + manifest.apps.values().filter(|data| data.is_some()).count() {
            return Err(failure());
        }
        Ok::<_, PlatformError>(())
    })
    .await
    .map_err(|_| failure())??;
    verify(verify_main.as_path(), apps).await?;
    Ok(true)
}
async fn verify(main: &Path, apps: &Path) -> PlatformResult<()> {
    let value = tgdrive_contract::validation::parse_unique_json(&std::fs::read(
        apps.join("manifest.json"),
    )?)
    .map_err(|_| failure())?;
    let manifest: Manifest = serde_json::from_value(value)?;
    let host = blob(main).await?;
    if manifest.host.sha256 != host.sha256 || manifest.host.size != host.size {
        return Err(failure());
    }
    let known = namespaces(main).await?;
    if manifest.apps.keys().cloned().collect::<BTreeSet<_>>() != known {
        return Err(failure());
    }
    for (id, expected) in manifest.apps {
        if let Some(expected) = expected {
            let path = apps.join(format!("{id}.sqlite3"));
            let actual = blob(&path).await?;
            if actual.sha256 != expected.sha256 || actual.size != expected.size {
                return Err(failure());
            }
            validate_application_database(&path).await?;
        }
    }
    Ok(())
}
pub(super) async fn validate_application_database(path: &Path) -> PlatformResult<()> {
    use sqlx::Connection;
    let mut header = [0u8; 16];
    std::fs::File::open(path)?.read_exact(&mut header)?;
    if &header != b"SQLite format 3\0" {
        return Err(failure());
    }
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .pragma("trusted_schema", "OFF")
            .pragma("query_only", "ON"),
    )
    .await?;
    let checks: Vec<(String,)> = sqlx::query_as("PRAGMA quick_check(1)")
        .fetch_all(&mut connection)
        .await?;
    connection.close().await?;
    if checks.len() != 1 || checks[0].0 != "ok" {
        return Err(failure());
    }
    Ok(())
}
pub(crate) async fn bundle_digest(main: &Path, apps: &Path) -> PlatformResult<String> {
    let mut sha = Sha256::new();
    sha.update(blob(main).await?.sha256);
    let bytes = std::fs::read(apps.join("manifest.json"))?;
    sha.update(&bytes);
    let manifest: Manifest = serde_json::from_value(
        tgdrive_contract::validation::parse_unique_json(&bytes).map_err(|_| failure())?,
    )?;
    for (id, expected) in manifest.apps {
        if let Some(expected) = expected {
            let actual = blob(&apps.join(format!("{id}.sqlite3"))).await?;
            if actual.size != expected.size || actual.sha256 != expected.sha256 {
                return Err(failure());
            }
            sha.update(id);
            sha.update(actual.sha256);
        }
    }
    Ok(crate::engine::upload::to_hex(&sha.finalize()))
}
pub(crate) async fn restore_apps(
    db: &Db,
    db_path: &str,
    apps: Option<&Path>,
) -> PlatformResult<()> {
    let mut entries = if let Some(apps) = apps {
        let manifest: Manifest = serde_json::from_value(
            tgdrive_contract::validation::parse_unique_json(&std::fs::read(
                apps.join("manifest.json"),
            )?)
            .map_err(|_| failure())?,
        )?;
        manifest.apps
    } else {
        namespaces(Path::new(db_path))
            .await?
            .into_iter()
            .map(|id| (id, None))
            .collect()
    };
    let root = data_root(db_path);
    if root.exists() {
        for id in super::safe_fs::Directory::open(&root, false)?.names()? {
            if valid_app_id(&id) {
                entries.entry(id).or_insert(None);
            }
        }
    }
    for (id, data) in entries {
        if !valid_app_id(&id) {
            return Err(failure());
        }
        let app = root.join(&id);
        let old_owner = std::fs::symlink_metadata(&app)
            .ok()
            .map(|metadata| metadata.gid());
        super::safe_fs::remove_tree_owner(&app, "state", old_owner)?;
        let identity = system::identity(db, &id).await?;
        system::prepare_layout(&root, &id, identity)?;
        if let Some(expected) = data {
            let source = apps.ok_or_else(failure)?.join(format!("{id}.sqlite3"));
            let actual = blob(&source).await?;
            if actual.size != expected.size || actual.sha256 != expected.sha256 {
                return Err(failure());
            }
            validate_application_database(&source).await?;
            let directory = super::safe_fs::Directory::open(&app, false)?.child_owner(
                "state",
                false,
                Some(identity.uid),
            )?;
            let mut input = std::fs::File::open(source)?;
            directory.write_atomic_reader("app.sqlite3", &mut input)?;
            directory.file_permissions("app.sqlite3", identity.uid, identity.gid, 0o600)?;
        }
    }
    Ok(())
}
