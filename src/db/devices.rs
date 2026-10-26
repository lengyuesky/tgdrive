//! 原生设备会话的持久层；所有刷新轮换与重放撤销都在同一写事务内完成。
use super::*;

#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize)]
pub struct DeviceSession {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub auth_epoch: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub expires_at: i64,
    #[serde(skip)]
    pub revoked_at: Option<i64>,
}

impl Db {
    pub async fn client_server_id(&self) -> AppResult<String> {
        Ok(
            sqlx::query_scalar("SELECT server_id FROM client_identity WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn create_device_session(
        &self,
        session: &DeviceSession,
        refresh_hash: &str,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM device_sessions WHERE expires_at <= ? OR auth_epoch != ? OR revoked_at IS NOT NULL")
            .bind(now_ts()).bind(&session.auth_epoch).execute(&mut *tx).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_sessions")
            .fetch_one(&mut *tx)
            .await?;
        if count >= 32 {
            return Err(AppError::BadRequest(
                "最多保留 32 个设备，请先撤销不用的设备".into(),
            ));
        }
        sqlx::query("INSERT INTO device_sessions(id,name,auth_epoch,created_at,last_used_at,expires_at) VALUES(?,?,?,?,?,?)")
            .bind(&session.id).bind(&session.name).bind(&session.auth_epoch)
            .bind(session.created_at).bind(session.last_used_at).bind(session.expires_at)
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO device_refresh_tokens(token_hash,session_id) VALUES(?,?)")
            .bind(refresh_hash)
            .bind(&session.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn active_device_session(
        &self,
        id: &str,
        epoch: &str,
    ) -> AppResult<Option<DeviceSession>> {
        Ok(sqlx::query_as("SELECT * FROM device_sessions WHERE id = ? AND auth_epoch = ? AND expires_at > ? AND revoked_at IS NULL")
            .bind(id).bind(epoch).bind(now_ts()).fetch_optional(&self.pool).await?)
    }

    pub async fn device_sessions(&self, epoch: &str) -> AppResult<Vec<DeviceSession>> {
        Ok(sqlx::query_as("SELECT * FROM device_sessions WHERE auth_epoch = ? AND expires_at > ? AND revoked_at IS NULL ORDER BY last_used_at DESC, id")
            .bind(epoch).bind(now_ts()).fetch_all(&self.pool).await?)
    }

    pub async fn revoke_device_session(&self, id: &str) -> AppResult<()> {
        sqlx::query("UPDATE device_sessions SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
            .bind(now_ts())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn rotate_device_refresh(
        &self,
        old_hash: &str,
        new_hash: &str,
        epoch: &str,
    ) -> AppResult<DeviceSession> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let token: Option<(String, Option<i64>)> = sqlx::query_as(
            "SELECT session_id, used_at FROM device_refresh_tokens WHERE token_hash = ?",
        )
        .bind(old_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, used_at)) = token else {
            return Err(AppError::Unauthorized);
        };
        let session: DeviceSession = sqlx::query_as("SELECT * FROM device_sessions WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        let now = now_ts();
        // 已消费的凭据也需要保留：不能仅删除旧哈希，否则无法检测被盗凭据的重放。
        if used_at.is_some()
            || session.revoked_at.is_some()
            || session.expires_at <= now
            || session.auth_epoch != epoch
        {
            sqlx::query(
                "UPDATE device_sessions SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?",
            )
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(AppError::Unauthorized);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM device_refresh_tokens WHERE session_id = ?")
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= 4096 {
            sqlx::query("UPDATE device_sessions SET revoked_at = ? WHERE id = ?")
                .bind(now)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(AppError::Unauthorized);
        }
        sqlx::query("UPDATE device_refresh_tokens SET used_at = ? WHERE token_hash = ?")
            .bind(now)
            .bind(old_hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO device_refresh_tokens(token_hash,session_id) VALUES(?,?)")
            .bind(new_hash)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE device_sessions SET last_used_at = ? WHERE id = ?")
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(DeviceSession {
            last_used_at: now,
            ..session
        })
    }
}
