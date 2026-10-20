//! reader 的替换规则属于宿主，不能由不可信的插件清单任意指定。
use super::{AppReplacement, AppReplacementPreview, Db};
use crate::error::AppResult;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use std::path::Path;

pub(super) async fn replacements(conn: &mut SqliteConnection) -> AppResult<Vec<AppReplacement>> {
    Ok(sqlx::query_as(
        "WITH legacy(id, name) AS (VALUES ('books', '图书'), ('comics', '漫画')),
         preview AS (
           SELECT legacy.id, COALESCE(CASE WHEN json_valid(apps.manifest) THEN json_extract(apps.manifest, '$.name') END, legacy.name) AS name,
             apps.id IS NOT NULL AS installed, apps.revision,
             (EXISTS(SELECT 1 FROM app_settings WHERE app_id = legacy.id)
              OR EXISTS(SELECT 1 FROM app_data WHERE app_id = legacy.id)
              OR EXISTS(SELECT 1 FROM app_lifecycle WHERE app_id = legacy.id)
              OR EXISTS(SELECT 1 FROM app_recovery WHERE app_id = legacy.id)) AS has_data
           FROM legacy LEFT JOIN apps ON apps.id = legacy.id)
         SELECT id, name, installed, has_data, revision FROM preview WHERE installed OR has_data ORDER BY id",
    ).fetch_all(conn).await?)
}

/// 只绑定展示范围和安装身份，不绑定进度值，正常阅读不会使确认失效。
pub(super) fn replacement_revision(rows: &[AppReplacement]) -> AppResult<String> {
    let scope: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                &row.id,
                &row.name,
                row.installed,
                row.has_data,
                &row.revision,
            )
        })
        .collect();
    let bytes =
        serde_json::to_vec(&("reader-replacements-v1", scope)).map_err(anyhow::Error::from)?;
    Ok(crate::engine::upload::to_hex(&Sha256::digest(bytes)))
}

impl Db {
    pub async fn app_replacement_preview(&self, id: &str) -> AppResult<AppReplacementPreview> {
        if id != "reader" {
            return Ok(AppReplacementPreview::default());
        }
        // 列表和隐藏修订来自同一查询结果，不再用另一连接重新查指纹。
        let replacements = replacements(&mut *self.pool.acquire().await?).await?;
        let replacement_revision = Some(replacement_revision(&replacements)?);
        Ok(AppReplacementPreview {
            replacements,
            replacement_revision,
        })
    }

    /// 调用者持有宿主 INSTALL_LOCK 和 CACHE_LOCK，防止安装的物化阶段与删除交叉。
    /// 每项任务持有主库写事务直到外部删除完成，跨连接安装也不能在检查后抢先提交。
    /// 外部删除幂等；任何失败保留任务，下次维护或重启继续。
    pub(crate) async fn cleanup_app_caches(&self, root: &Path) -> AppResult<()> {
        let jobs: Vec<(String, String)> =
            sqlx::query_as("SELECT app_id, token FROM app_cleanup_jobs ORDER BY app_id")
                .fetch_all(&self.pool)
                .await?;
        let mut first_error = None;
        for (id, token) in jobs {
            let result: AppResult<()> =
                async {
                    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
                    let current: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM app_cleanup_jobs WHERE app_id = ? AND token = ?)",
                ).bind(&id).bind(&token).fetch_one(&mut *tx).await?;
                    if !current {
                        return Ok(());
                    }
                    let installed: bool =
                        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM apps WHERE id = ?)")
                            .bind(&id)
                            .fetch_one(&mut *tx)
                            .await?;
                    if !installed {
                        self.cover_clear(&id).await?;
                        if let Err(error) = tokio::fs::remove_dir_all(root.join(&id)).await
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(error.into());
                        }
                    }
                    sqlx::query("DELETE FROM app_cleanup_jobs WHERE app_id = ? AND token = ?")
                        .bind(&id)
                        .bind(&token)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    Ok(())
                }
                .await;
            if let Err(error) = result {
                tracing::warn!(app_id = %id, %error, "旧应用缓存清理失败，保留任务重试");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
