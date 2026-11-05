//! 复用应用私有存储及其更新快照；所有内部写入仍检查修订、授权快照和配置 CAS。

use super::{APP_ID, Config, Context};
use crate::{
    db::Db,
    error::{AppError, AppResult},
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Sqlite, Transaction};

pub(super) const CONFIG_KEY: &str = "fntv:config";
pub(super) async fn config(db: &Db) -> AppResult<(Config, Option<String>)> {
    let record = db.app_data_get(APP_ID, CONFIG_KEY).await?;
    match record {
        Some(r) => Ok((
            serde_json::from_str(&r.value)
                .map_err(|_| AppError::Conflict("影视配置损坏，请恢复应用数据".into()))?,
            Some(r.revision),
        )),
        None => Ok((Config::default(), None)),
    }
}

pub(crate) async fn stop_in_transaction(tx: &mut Transaction<'_, Sqlite>) -> AppResult<()> {
    sqlx::query("UPDATE app_data SET value = json_set(value, '$.running', json('false')), revision = ?, updated_at = ? WHERE app_id = 'fntv' AND key = 'fntv:config'")
        .bind(crate::db::random_token(32)).bind(crate::db::now_ts()).execute(&mut **tx).await?;
    Ok(())
}

async fn check(tx: &mut Transaction<'_, Sqlite>, context: &Context) -> AppResult<()> {
    context.lease.check()?;
    let active: Option<(String, bool, String, String)> = sqlx::query_as(
        "SELECT revision, enabled, scope_mode, scope_paths FROM apps WHERE id = 'fntv'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    let valid = active.is_some_and(|(revision, enabled, mode, paths)| {
        enabled
            && revision == context.app.revision
            && mode == context.app.scope_mode
            && serde_json::from_str::<Vec<String>>(&paths).ok().as_ref() == Some(&context.app.scope)
    });
    if !valid {
        return Err(AppError::AppChanged);
    }
    let revision: Option<String> = sqlx::query_scalar(
        "SELECT revision FROM app_data WHERE app_id = 'fntv' AND key = 'fntv:config'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    if revision != context.config_revision {
        return Err(AppError::AppDataConflict);
    }
    Ok(())
}

async fn put(tx: &mut Transaction<'_, Sqlite>, key: &str, value: &str) -> AppResult<()> {
    if key.len() > 128 || value.len() > 32 * 1024 {
        return Err(AppError::BadRequest("影视数据单项超过上限".into()));
    }
    sqlx::query("INSERT INTO app_data(app_id,key,value,revision,updated_at) VALUES('fntv',?,?,?,?) ON CONFLICT(app_id,key) DO UPDATE SET value=excluded.value,revision=excluded.revision,updated_at=excluded.updated_at")
        .bind(key).bind(value).bind(crate::db::random_token(32)).bind(crate::db::now_ts()).execute(&mut **tx).await?;
    Ok(())
}

async fn quota(tx: &mut Transaction<'_, Sqlite>) -> AppResult<()> {
    let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(length(CAST(value AS BLOB))+length(key)),0) FROM app_data WHERE app_id = 'fntv'")
        .fetch_one(&mut **tx).await?;
    if count > 10000 || bytes > 8 * 1024 * 1024 {
        return Err(AppError::BadRequest(
            "影视私有数据达到 10000 条或 8 MiB 上限，请缩小媒体库或明确清除应用数据".into(),
        ));
    }
    Ok(())
}

pub(super) async fn get<T: DeserializeOwned>(db: &Db, key: &str) -> AppResult<Option<T>> {
    db.app_data_get(APP_ID, key)
        .await?
        .map(|r| {
            serde_json::from_str(&r.value)
                .map_err(|_| AppError::Conflict("影视私有数据损坏".into()))
        })
        .transpose()
}

pub(super) async fn list<T: DeserializeOwned>(db: &Db, prefix: &str) -> AppResult<Vec<T>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT value FROM app_data WHERE app_id = 'fntv' AND substr(key,1,length(?)) = ? ORDER BY key LIMIT 10001")
        .bind(prefix).bind(prefix).fetch_all(&db.pool).await?;
    if rows.len() > 10000 {
        return Err(AppError::BadRequest("影视数据超出读取预算".into()));
    }
    rows.into_iter()
        .map(|v| {
            serde_json::from_str(&v)
                .map_err(|_| AppError::Conflict("影视索引损坏，请刷新媒体库".into()))
        })
        .collect()
}

pub(super) async fn save<T: Serialize>(
    db: &Db,
    context: &Context,
    key: &str,
    value: &T,
) -> AppResult<()> {
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    check(&mut tx, context).await?;
    put(
        &mut tx,
        key,
        &serde_json::to_string(value).map_err(anyhow::Error::from)?,
    )
    .await?;
    quota(&mut tx).await?;
    context.lease.check()?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn save_config(db: &Db, context: &Context, config: &Config) -> AppResult<()> {
    save(db, context, CONFIG_KEY, config).await
}

pub(super) async fn publish_index(
    db: &Db,
    context: &Context,
    rows: &[(String, super::index::Entry)],
) -> AppResult<()> {
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    check(&mut tx, context).await?;
    sqlx::query("DELETE FROM app_data WHERE app_id = 'fntv' AND key LIKE 'fntv:index:%'")
        .execute(&mut *tx)
        .await?;
    for (key, row) in rows {
        put(
            &mut tx,
            key,
            &serde_json::to_string(row).map_err(anyhow::Error::from)?,
        )
        .await?;
    }
    // 探测结果可重新生成，离库或内容版本失效的缓存不占用业务记录的配额。
    // 观看位置、收藏和已看状态仍按原文件身份保留。
    let current: std::collections::HashMap<&str, &super::index::Entry> = rows
        .iter()
        .map(|(_, entry)| (entry.media_guid.as_str(), entry))
        .collect();
    let probes: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM app_data WHERE app_id = 'fntv' AND key LIKE 'fntv:probe:%'",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (key, value) in probes {
        let valid = key
            .strip_prefix("fntv:probe:")
            .and_then(|media| current.get(media))
            .is_some_and(|entry| {
                serde_json::from_str::<super::probe::MediaInfo>(&value)
                    .is_ok_and(|info| info.valid(entry))
            });
        if !valid {
            sqlx::query("DELETE FROM app_data WHERE app_id = 'fntv' AND key = ?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
    }
    quota(&mut tx).await?;
    context.lease.check()?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn activate_play(db: &Db, context: &Context, media: &str) -> AppResult<u64> {
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    check(&mut tx, context).await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_data WHERE app_id='fntv' AND key='fntv:serial'")
            .fetch_optional(&mut *tx)
            .await?;
    let serial = current
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| AppError::Conflict("播放代次耗尽".into()))?;
    put(&mut tx, "fntv:serial", &serial.to_string()).await?;
    put(
        &mut tx,
        &format!("fntv:authority:{media}"),
        &serial.to_string(),
    )
    .await?;
    quota(&mut tx).await?;
    context.lease.check()?;
    tx.commit().await?;
    Ok(serial)
}

pub(super) async fn progress<T: Serialize>(
    db: &Db,
    context: &Context,
    media: &str,
    serial: u64,
    value: &T,
) -> AppResult<bool> {
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    check(&mut tx, context).await?;
    let authority: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_data WHERE app_id='fntv' AND key=?")
            .bind(format!("fntv:authority:{media}"))
            .fetch_optional(&mut *tx)
            .await?;
    if authority.as_deref() != Some(serial.to_string().as_str()) {
        return Ok(false);
    }
    put(
        &mut tx,
        &format!("fntv:progress:{media}"),
        &serde_json::to_string(value).map_err(anyhow::Error::from)?,
    )
    .await?;
    quota(&mut tx).await?;
    context.lease.check()?;
    tx.commit().await?;
    Ok(true)
}
