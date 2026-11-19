//! 主库应用表随既有快照备份；恢复标记在开库时一次性失效运行身份与游标。

use super::store::Installation;
use super::{PlatformError, PlatformResult};
use crate::db::Db;
use sqlx::{SqliteConnection, SqlitePool};
use tgdrive_contract::{error_code, http::*, validation::*};

pub(crate) async fn mark_snapshot(connection: &mut SqliteConnection) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='app_change_state')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if exists {
        sqlx::query("UPDATE app_change_state SET restore_pending=1 WHERE id=1")
            .execute(connection)
            .await?;
    }
    Ok(())
}

pub(crate) async fn apply_pending(pool: &SqlitePool, path: &str) -> anyhow::Result<()> {
    let pending: bool =
        sqlx::query_scalar("SELECT restore_pending FROM app_change_state WHERE id=1")
            .fetch_one(pool)
            .await?;
    if !pending {
        return Ok(());
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let records: Vec<Installation> = sqlx::query_as("SELECT * FROM app_installations")
        .fetch_all(&mut *tx)
        .await?;
    for record in records {
        let manifest = parse_manifest(record.manifest.as_bytes())?;
        anyhow::ensure!(
            record.id == manifest.id
                && valid_sha256(&record.package_sha256)
                && record.package_size > 0
                && record.package_size as u64 <= MAX_PACKAGE_BYTES,
            "恢复的应用安装记录无效"
        );
        let target: tgdrive_contract::manifest::PackageTarget =
            serde_json::from_value(serde_json::Value::String(record.target.clone()))?;
        anyhow::ensure!(
            manifest.backend.is_some()
                != (target == tgdrive_contract::manifest::PackageTarget::Web),
            "恢复的包目标与清单不一致"
        );
        validate_scope(&record.directory_scope()?, &manifest.files)?;
        // 后端轮次留给第二阶段；新平台不恢复任何在途句柄或未知写入。
        sqlx::query("UPDATE app_installations SET generation=?,state_revision=?,scope_revision=? WHERE id=?").bind(super::opaque()).bind(super::opaque()).bind(super::opaque()).bind(record.id).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM app_file_changes")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM app_install_previews")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM app_package_uploads")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM app_change_batches")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE app_change_state SET epoch=?,trimmed_seq=COALESCE((SELECT seq FROM sqlite_sequence WHERE name='app_file_changes'),0),restore_pending=0 WHERE id=1").bind(super::opaque()).execute(&mut *tx).await?;
    // 数据根由宿主数据库路径推导；从已验证的根 FD 清缓存，不跟随应用路径链接。
    let parent = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let root = parent.join("apps-v1/data");
    if root.exists() {
        let directory = super::safe_fs::Directory::open(&root, false)?;
        for id in directory.names()? {
            if valid_app_id(&id) {
                let app = root.join(id);
                super::safe_fs::remove_tree(&app, "cache")?;
                super::safe_fs::remove_tree(&app, "tmp")?;
                super::safe_fs::remove_tree(&app, "run")?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// 只合并不可变发布事实，不把当前安装状态、授权或业务 KV 塞回旧备份。
pub(crate) async fn preserve_history(
    connection: &mut SqliteConnection,
    live: &Db,
) -> anyhow::Result<()> {
    // 合并签名域后保留当前实例的活动密钥：旧实例/备份游标可验签，但恢复 epoch 阻止复活。
    let mut keys = live.app_cursor_keys.as_ref().clone();
    let candidate: String =
        sqlx::query_scalar("SELECT cursor_key FROM app_change_state WHERE id=1")
            .fetch_one(&mut *connection)
            .await?;
    keys.push(candidate);
    for key in keys {
        sqlx::query("INSERT INTO app_cursor_keys(key) VALUES(?) ON CONFLICT(key) DO NOTHING")
            .bind(key)
            .execute(&mut *connection)
            .await?;
    }
    sqlx::query("UPDATE app_change_state SET cursor_key=? WHERE id=1")
        .bind(&live.app_cursor_key)
        .execute(&mut *connection)
        .await?;
    let bindings: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
        "SELECT app_id,version,target,manifest,sha256,size FROM app_package_bindings",
    )
    .fetch_all(&live.pool)
    .await?;
    for (id, version, target, manifest, sha, size) in bindings {
        let existing:Option<(String,String,i64)>=sqlx::query_as("SELECT manifest,sha256,size FROM app_package_bindings WHERE app_id=? AND version=? AND target=?").bind(&id).bind(&version).bind(&target).fetch_optional(&mut *connection).await?;
        if let Some((old, old_sha, old_size)) = existing {
            anyhow::ensure!(
                parse_unique_json(old.as_bytes())? == parse_unique_json(manifest.as_bytes())?
                    && old_sha == sha
                    && old_size == size,
                "备份与当前实例的不可变包绑定冲突"
            );
        }
        sqlx::query("INSERT INTO app_package_bindings(app_id,version,target,manifest,sha256,size) VALUES(?,?,?,?,?,?) ON CONFLICT(app_id,version,target) DO NOTHING").bind(id).bind(version).bind(target).bind(manifest).bind(sha).bind(size).execute(&mut *connection).await?;
    }
    let history: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT repository,app_id,version,manifest,packages FROM app_release_history",
    )
    .fetch_all(&live.pool)
    .await?;
    for (repository, id, version, manifest, packages) in history {
        let existing:Option<(String,String)>=sqlx::query_as("SELECT manifest,packages FROM app_release_history WHERE repository=? AND app_id=? AND version=?").bind(&repository).bind(&id).bind(&version).fetch_optional(&mut *connection).await?;
        let mut packages: std::collections::BTreeMap<
            String,
            tgdrive_contract::manifest::PackageArtifact,
        > = serde_json::from_value(parse_unique_json(packages.as_bytes())?)?;
        if let Some((old, old_packages)) = existing {
            anyhow::ensure!(
                parse_unique_json(old.as_bytes())? == parse_unique_json(manifest.as_bytes())?,
                "备份与当前实例的不可变清单冲突"
            );
            let old_packages: std::collections::BTreeMap<
                String,
                tgdrive_contract::manifest::PackageArtifact,
            > = serde_json::from_value(parse_unique_json(old_packages.as_bytes())?)?;
            for (target, artifact) in old_packages {
                if let Some(current) = packages.get(&target) {
                    anyhow::ensure!(current == &artifact, "备份与当前实例的不可变架构包冲突");
                } else {
                    packages.insert(target, artifact);
                }
            }
        }
        sqlx::query("INSERT INTO app_release_history(repository,app_id,version,manifest,packages) VALUES(?,?,?,?,?) ON CONFLICT(repository,app_id,version) DO UPDATE SET packages=excluded.packages").bind(repository).bind(id).bind(version).bind(manifest).bind(serde_json::to_string(&packages)?).execute(&mut *connection).await?;
    }
    mark_snapshot(connection).await?;
    Ok(())
}

pub(crate) async fn prune_changes(db: &Db) -> PlatformResult<()> {
    if db.background_blocked() {
        return Ok(());
    }
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let cutoff = super::now().0 - (FILE_CHANGE_RETENTION_SECONDS * 1000) as i64;
    let trim:i64=sqlx::query_scalar("SELECT COALESCE(max(seq),0) FROM app_file_changes INDEXED BY app_file_changes_time WHERE occurred_at<?").bind(cutoff).fetch_one(&mut *tx).await?;
    sqlx::query("UPDATE app_change_state SET trimmed_seq=max(trimmed_seq,?) WHERE id=1")
        .bind(trim)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM app_file_changes WHERE occurred_at<?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

impl From<PlatformError> for crate::error::AppError {
    fn from(error: PlatformError) -> Self {
        if error.code == error_code::HOST_MAINTENANCE {
            Self::Forbidden("宿主维护中".into())
        } else {
            Self::Internal(anyhow::anyhow!("应用平台维护失败"))
        }
    }
}
