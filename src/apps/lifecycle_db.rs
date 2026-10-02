//! 应用更新恢复、迁移租约与有界诊断。恢复只作用于应用私有数据。
use super::{Db, now_ts, random_token};
use crate::error::{AppError, AppResult};
use serde_json::{Value, json};

impl Db {
    pub async fn set_app_scope_mode(
        &self,
        id: &str,
        mode: &str,
        paths: &str,
        revision: &str,
    ) -> AppResult<()> {
        let result = sqlx::query("UPDATE apps SET scope_mode = ?, scope_paths = ?, updated_at = ? WHERE id = ? AND revision = ?")
            .bind(mode).bind(paths).bind(now_ts()).bind(id).bind(revision).execute(&self.pool).await?;
        self.apps_cache.invalidate(id);
        if result.rows_affected() != 1 {
            return Err(AppError::AppChanged);
        }
        Ok(())
    }

    pub async fn app_health(&self, id: &str) -> AppResult<Value> {
        let lifecycle: Option<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT schema_version, phase, message, updated_at FROM app_lifecycle WHERE app_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let recovery: Option<(String, i64, i64)> = sqlx::query_as("SELECT version, created_at, length(package) + length(CAST(data AS BLOB)) FROM app_recovery WHERE app_id = ?")
            .bind(id).fetch_optional(&self.pool).await?;
        Ok(
            json!({"lifecycle": lifecycle.map(|(schema, phase, message, at)| json!({"schema_version":schema,"phase":phase,"message":message,"updated_at":at})),
            "recovery": recovery.map(|(version, at, bytes)| json!({"version":version,"created_at":at,"bytes":bytes}))}),
        )
    }

    pub async fn report_app_health(
        &self,
        id: &str,
        revision: &str,
        phase: &str,
        message: &str,
    ) -> AppResult<()> {
        sqlx::query("INSERT INTO app_lifecycle(app_id, phase, message, updated_at) SELECT id, ?, ?, ? FROM apps WHERE id = ? AND revision = ? ON CONFLICT(app_id) DO UPDATE SET phase = excluded.phase, message = excluded.message, updated_at = excluded.updated_at WHERE app_lifecycle.phase != 'migrating'")
            .bind(phase).bind(message).bind(now_ts()).bind(id).bind(revision).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn app_migration(
        &self,
        id: &str,
        revision: &str,
        action: &str,
        target: u32,
        owner: &str,
        message: &str,
    ) -> AppResult<Value> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM apps WHERE id = ? AND revision = ? AND enabled = 1)",
        )
        .bind(id)
        .bind(revision)
        .fetch_one(&mut *tx)
        .await?;
        if !active {
            return Err(AppError::AppChanged);
        }
        sqlx::query("INSERT OR IGNORE INTO app_lifecycle(app_id, updated_at) VALUES(?, ?)")
            .bind(id)
            .bind(now_ts())
            .execute(&mut *tx)
            .await?;
        let (version, held, until): (i64, String, i64) = sqlx::query_as(
            "SELECT schema_version, owner, lease_until FROM app_lifecycle WHERE app_id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if version > i64::from(target) {
            return Err(AppError::Conflict(
                "应用数据版本高于程序支持版本，请更新应用或恢复完整快照".into(),
            ));
        }
        let now = now_ts();
        if action == "begin" && version == i64::from(target) {
            return Ok(json!({"needed":false,"version":version}));
        }
        if !held.is_empty() && held != owner && until > now {
            return Err(AppError::Conflict(
                "另一页面正在迁移应用数据，请稍后重试".into(),
            ));
        }
        match action {
            "begin" => {
                sqlx::query("UPDATE app_lifecycle SET phase = 'migrating', message = '', owner = ?, lease_until = ?, updated_at = ? WHERE app_id = ?")
                    .bind(owner).bind(now + 60).bind(now).bind(id).execute(&mut *tx).await?;
            }
            "heartbeat" | "complete" | "failed" => {
                if held != owner || until <= now {
                    return Err(AppError::Conflict("迁移租约已失效，请重新打开重试".into()));
                }
                let done = action != "heartbeat";
                sqlx::query("UPDATE app_lifecycle SET schema_version = ?, phase = ?, message = ?, owner = ?, lease_until = ?, updated_at = ? WHERE app_id = ?")
                    .bind(if action == "complete" { i64::from(target) } else { version })
                    .bind(if action == "complete" { "ready" } else if action == "failed" { "failed" } else { "migrating" })
                    .bind(message).bind(if done { "" } else { owner }).bind(if done { 0 } else { now + 60 }).bind(now).bind(id).execute(&mut *tx).await?;
            }
            _ => return Err(AppError::BadRequest("迁移操作无效".into())),
        }
        tx.commit().await?;
        Ok(json!({"needed":true,"version":version}))
    }

    pub async fn app_recovery_package(&self, id: &str) -> AppResult<Vec<u8>> {
        sqlx::query_scalar("SELECT package FROM app_recovery WHERE app_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub async fn recover_app(&self, id: &str, revision: &str, digest: &str) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: Option<String> = sqlx::query_scalar("SELECT revision FROM apps WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if current.as_deref() != Some(revision) {
            return Err(AppError::AppChanged);
        }
        let snapshot: Option<(String, String, String, Vec<u8>, String, String, String, i64)> = sqlx::query_as("SELECT manifest, version, digest, package, source, settings, data, schema_version FROM app_recovery WHERE app_id = ? AND digest = ?")
            .bind(id).bind(digest).fetch_optional(&mut *tx).await?;
        let (manifest, version, digest, package, source, settings, data, schema) =
            snapshot.ok_or(AppError::NotFound)?;
        sqlx::query("UPDATE apps SET manifest = ?, version = ?, digest = ?, package = ?, source = ?, revision = ?, updated_at = ? WHERE id = ?")
            .bind(manifest).bind(version).bind(digest).bind(package).bind(source).bind(random_token(32)).bind(now_ts()).bind(id).execute(&mut *tx).await?;
        sqlx::query("INSERT OR REPLACE INTO app_settings(app_id, settings) VALUES(?, ?)")
            .bind(id)
            .bind(settings)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM app_data WHERE app_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO app_data(app_id, key, value, revision, updated_at) SELECT ?, json_extract(value, '$.key'), json_extract(value, '$.value'), json_extract(value, '$.revision'), json_extract(value, '$.updated_at') FROM json_each(?)")
            .bind(id).bind(data).execute(&mut *tx).await?;
        sqlx::query("INSERT OR REPLACE INTO app_lifecycle(app_id, schema_version, phase, updated_at) VALUES(?, ?, 'restored', ?)").bind(id).bind(schema).bind(now_ts()).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM app_recovery WHERE app_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.apps_cache.invalidate(id);
        Ok(())
    }
}
