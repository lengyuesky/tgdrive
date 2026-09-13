//! 应用安装记录与配置；安装、更新、卸载均在短事务内提交。

use super::{Db, now_ts, random_token};
use crate::error::{AppError, AppResult};

mod data;
pub use data::*;

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
        Ok(sqlx::query_as("SELECT id, manifest, version, digest, revision, source, enabled, pinned, installed_at, updated_at FROM apps ORDER BY installed_at, id")
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn app(&self, id: &str) -> AppResult<Option<AppRecord>> {
        Ok(sqlx::query_as("SELECT id, manifest, version, digest, revision, source, enabled, pinned, installed_at, updated_at FROM apps WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
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
