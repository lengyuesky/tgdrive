//! 应用安装记录与配置；安装、更新、卸载均在短事务内提交。
//! 安装记录另有进程内缓存：运行页面的每次 RPC、资源与媒体请求都要核对启用状态和修订，
//! 命中缓存后不再占用数据库连接；本模块是 `apps` 表唯一的写入者，写入后同步失效。

use super::{Db, now_ts, random_token};
use crate::error::{AppError, AppResult};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

mod covers_db;
mod data;
pub use covers_db::*;
pub use data::*;

/// 已安装记录缓存。代次计数保证"查库期间发生写入"的读者不会把陈旧行写回：
/// 读者在查库前记下代次，回填时代次已变则放弃；写入者提交后先递增代次再移除条目。
#[derive(Default)]
pub struct AppCache {
    inner: Mutex<AppCacheInner>,
}

#[derive(Default)]
struct AppCacheInner {
    generation: u64,
    records: HashMap<String, AppRecord>,
}

impl AppCache {
    fn lock(&self) -> MutexGuard<'_, AppCacheInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 命中返回记录；未命中返回当前代次，供回填时校验。
    fn lookup(&self, id: &str) -> Result<AppRecord, u64> {
        let inner = self.lock();
        inner.records.get(id).cloned().ok_or(inner.generation)
    }

    fn store(&self, generation: u64, record: AppRecord) {
        let mut inner = self.lock();
        if inner.generation == generation {
            inner.records.insert(record.id.clone(), record);
        }
    }

    fn invalidate(&self, id: &str) {
        let mut inner = self.lock();
        inner.generation = inner.generation.wrapping_add(1);
        inner.records.remove(id);
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppRecord {
    pub id: String,
    pub manifest: String,
    pub version: String,
    pub digest: String,
    pub revision: String,
    pub source: String,
    pub enabled: bool,
    pub pinned: bool,
    pub installed_at: i64,
    pub updated_at: i64,
    /// JSON 路径数组；空数组表示全盘。
    pub scope_paths: String,
}

pub struct AppInstall<'a> {
    pub id: &'a str,
    pub manifest: &'a str,
    pub version: &'a str,
    pub digest: &'a str,
    pub package: &'a [u8],
    pub source: &'a str,
    pub expected_revision: Option<&'a str>,
}

impl Db {
    pub async fn apps(&self) -> AppResult<Vec<AppRecord>> {
        Ok(sqlx::query_as("SELECT id, manifest, version, digest, revision, source, enabled, pinned, installed_at, updated_at, scope_paths FROM apps ORDER BY installed_at, id")
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn app(&self, id: &str) -> AppResult<Option<AppRecord>> {
        let generation = match self.apps_cache.lookup(id) {
            Ok(record) => return Ok(Some(record)),
            Err(generation) => generation,
        };
        let record: Option<AppRecord> = sqlx::query_as("SELECT id, manifest, version, digest, revision, source, enabled, pinned, installed_at, updated_at, scope_paths FROM apps WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        // 未安装不做负缓存：安装后第一次访问必须看到新记录。
        if let Some(record) = &record {
            self.apps_cache.store(generation, record.clone());
        }
        Ok(record)
    }

    /// 只更新目录范围；不轮换修订版，避免打断正在运行的页面（逐次调用强制校验）。
    pub async fn set_app_scope(&self, id: &str, paths: &str) -> AppResult<()> {
        let changed = sqlx::query("UPDATE apps SET scope_paths = ?, updated_at = ? WHERE id = ?")
            .bind(paths)
            .bind(now_ts())
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.apps_cache.invalidate(id);
        if changed.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    pub async fn app_package(&self, id: &str, digest: &str) -> AppResult<Vec<u8>> {
        sqlx::query_scalar("SELECT package FROM apps WHERE id = ? AND digest = ?")
            .bind(id)
            .bind(digest)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub async fn install_app(&self, input: AppInstall<'_>) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<(String, String)> =
            sqlx::query_as("SELECT revision, version FROM apps WHERE id = ?")
                .bind(input.id)
                .fetch_optional(&mut *tx)
                .await?;
        if previous.as_ref().map(|v| v.0.as_str()) != input.expected_revision {
            return Err(AppError::Conflict(
                "应用状态已经改变，请刷新后重新安装".into(),
            ));
        }
        if let Some((_, version)) = previous {
            let old = semver::Version::parse(&version).map_err(|e| AppError::Internal(e.into()))?;
            let new =
                semver::Version::parse(input.version).map_err(|e| AppError::Internal(e.into()))?;
            if new <= old {
                return Err(AppError::Conflict("更新包版本必须高于已安装版本".into()));
            }
        }
        let now = now_ts();
        sqlx::query(
            "INSERT INTO apps (id, manifest, version, digest, revision, package, source, installed_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET manifest = excluded.manifest, version = excluded.version,
             digest = excluded.digest, revision = excluded.revision, package = excluded.package,
             source = excluded.source, updated_at = excluded.updated_at",
        )
        .bind(input.id)
        .bind(input.manifest)
        .bind(input.version)
        .bind(input.digest)
        .bind(random_token(32))
        .bind(input.package)
        .bind(input.source)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.apps_cache.invalidate(input.id);
        Ok(())
    }

    pub async fn set_app_state(
        &self,
        id: &str,
        enabled: Option<bool>,
        pinned: Option<bool>,
    ) -> AppResult<()> {
        // 任何启停操作都轮换运行凭据，旧页面不能在重新启用后恢复访问。
        let changed = sqlx::query(
            "UPDATE apps SET enabled = COALESCE(?, enabled), pinned = COALESCE(?, pinned),
             revision = CASE WHEN ? IS NULL THEN revision ELSE ? END, updated_at = ? WHERE id = ?",
        )
        .bind(enabled)
        .bind(pinned)
        .bind(enabled)
        .bind(random_token(32))
        .bind(now_ts())
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.apps_cache.invalidate(id);
        if changed.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    pub async fn uninstall_app(&self, id: &str, purge_data: bool) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let deleted = sqlx::query("DELETE FROM apps WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        if purge_data {
            sqlx::query("DELETE FROM app_data WHERE app_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM app_settings WHERE app_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.apps_cache.invalidate(id);
        Ok(())
    }

    pub async fn app_settings(&self, id: &str) -> AppResult<String> {
        Ok(
            sqlx::query_scalar("SELECT settings FROM app_settings WHERE app_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or_else(|| "{}".into()),
        )
    }

    pub async fn patch_app_settings(&self, id: &str, revision: &str, patch: &str) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: Option<String> = sqlx::query_scalar("SELECT revision FROM apps WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if current.as_deref() != Some(revision) {
            return Err(AppError::Conflict("应用状态已经改变，请重新打开".into()));
        }
        // 在数据库中合并，避免静音偏好与目录设置的并发保存互相覆盖。
        sqlx::query(
            "INSERT INTO app_settings (app_id, settings) VALUES (?, ?)
             ON CONFLICT(app_id) DO UPDATE SET settings = json_patch(app_settings.settings, excluded.settings)",
        )
        .bind(id)
        .bind(patch)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
