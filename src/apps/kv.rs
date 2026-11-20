//! 应用命名空间 KV：事务内 CAS、不可复用修订、净增长配额与键集分页。

use super::cursor::{CursorCodec, CursorData, binding};
use super::store;
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use tgdrive_contract::data::*;
use tgdrive_contract::error_code;
use tgdrive_contract::http::*;
use tgdrive_contract::validation::{valid_kv_key, valid_opaque_id};

#[derive(sqlx::FromRow)]
struct KvRow {
    key: String,
    value: String,
    revision: String,
    updated_at: i64,
    bytes: i64,
}
impl KvRow {
    fn record(&self) -> PlatformResult<KvRecord> {
        Ok(KvRecord {
            key: self.key.clone(),
            value: serde_json::from_str(&self.value)?,
            revision: self.revision.clone(),
            updated_at: tgdrive_contract::Timestamp(self.updated_at),
        })
    }
}
fn key(value: &str) -> PlatformResult<()> {
    if !valid_kv_key(value) {
        return Err(PlatformError::new(error_code::INVALID_REQUEST, "键名无效"));
    }
    Ok(())
}
fn conflict(revision: Option<&str>) -> PlatformError {
    let mut error = PlatformError::new(error_code::STORAGE_CONFLICT, "键值记录已改变，请重新读取");
    error.details = Some(serde_json::json!({"current_revision":revision}));
    error
}

impl AppPlatform {
    pub async fn kv_get(
        &self,
        app_id: &str,
        generation: &str,
        value: &str,
    ) -> PlatformResult<KvGetResponse> {
        key(value)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let row: Option<KvRow> = sqlx::query_as(
            "SELECT key,value,revision,updated_at,bytes FROM app_kv WHERE app_id=? AND key=?",
        )
        .bind(app_id)
        .bind(value)
        .fetch_optional(&mut *tx)
        .await?;
        let record = row.map(|row| row.record()).transpose()?;
        tx.commit().await?;
        Ok(KvGetResponse { record })
    }
    pub async fn kv_put(
        &self,
        app_id: &str,
        generation: &str,
        value: &str,
        request: KvPutRequest,
    ) -> PlatformResult<KvRecord> {
        key(value)?;
        let json = serde_json::to_string(&request.value)?;
        if json.len() as u64 > MAX_KV_VALUE_BYTES {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "单个键值超过上限",
            ));
        }
        if let KvCondition::Revision { revision } = &request.condition
            && !valid_opaque_id(revision)
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "期望修订无效",
            ));
        }
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let old: Option<KvRow> = sqlx::query_as(
            "SELECT key,value,revision,updated_at,bytes FROM app_kv WHERE app_id=? AND key=?",
        )
        .bind(app_id)
        .bind(value)
        .fetch_optional(&mut *tx)
        .await?;
        let matches = match (&request.condition, &old) {
            (KvCondition::Absent, None) => true,
            (KvCondition::Revision { revision }, Some(old)) => revision == &old.revision,
            _ => false,
        };
        if !matches {
            return Err(conflict(old.as_ref().map(|old| old.revision.as_str())));
        }
        let usage: Option<(i64, i64)> =
            sqlx::query_as("SELECT entries,bytes FROM app_kv_usage WHERE app_id=?")
                .bind(app_id)
                .fetch_optional(&mut *tx)
                .await?;
        let (entries, used) = usage.unwrap_or((0, 0));
        let bytes = (value.len() + json.len()) as i64;
        if entries + i64::from(old.is_none()) > DEFAULT_KV_ENTRIES as i64
            || used - old.as_ref().map_or(0, |old| old.bytes) + bytes > DEFAULT_KV_BYTES as i64
        {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "应用键值存储配额已满",
            ));
        }
        let revision = opaque();
        let at = now();
        sqlx::query("INSERT INTO app_kv(app_id,key,value,revision,updated_at,bytes) VALUES(?,?,?,?,?,?) ON CONFLICT(app_id,key) DO UPDATE SET value=excluded.value,revision=excluded.revision,updated_at=excluded.updated_at,bytes=excluded.bytes")
            .bind(app_id).bind(value).bind(json).bind(&revision).bind(at.0).bind(bytes).execute(&mut *tx).await?;
        tx.commit().await?;
        self.events.emit(
            app_id,
            tgdrive_contract::events::event_name::KV_CHANGED,
            &tgdrive_contract::events::KvChangedEvent {
                key: value.into(),
                revision: Some(revision.clone()),
            },
        )?;
        Ok(KvRecord {
            key: value.into(),
            value: request.value,
            revision,
            updated_at: at,
        })
    }
    pub async fn kv_delete(
        &self,
        app_id: &str,
        generation: &str,
        value: &str,
        request: KvDeleteRequest,
    ) -> PlatformResult<()> {
        key(value)?;
        if !valid_opaque_id(&request.revision) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "期望修订无效",
            ));
        }
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let old: Option<(String,)> =
            sqlx::query_as("SELECT revision FROM app_kv WHERE app_id=? AND key=?")
                .bind(app_id)
                .bind(value)
                .fetch_optional(&mut *tx)
                .await?;
        if old.as_ref().map(|(revision,)| revision) != Some(&request.revision) {
            return Err(conflict(old.as_ref().map(|(revision,)| revision.as_str())));
        }
        sqlx::query("DELETE FROM app_kv WHERE app_id=? AND key=?")
            .bind(app_id)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.events.emit(
            app_id,
            tgdrive_contract::events::event_name::KV_CHANGED,
            &tgdrive_contract::events::KvChangedEvent {
                key: value.into(),
                revision: None,
            },
        )?;
        Ok(())
    }
    pub async fn kv_list(
        &self,
        app_id: &str,
        generation: &str,
        mut request: KvListRequest,
    ) -> PlatformResult<KvPage> {
        if request.limit == 0
            || u64::from(request.limit) > MAX_KV_PAGE_LIMIT
            || (!request.prefix.is_empty() && !valid_kv_key(&request.prefix))
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "键值分页参数无效",
            ));
        }
        let token = request.cursor.take();
        let fingerprint = binding(&request)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        app.check(generation)?;
        let cursor = token
            .as_ref()
            .map(|token| self.cursors.decode(token))
            .transpose()?;
        if let Some(cursor) = &cursor {
            CursorCodec::check(cursor, "kv", &app, &fingerprint, false)?;
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT key,value,revision,updated_at,bytes FROM app_kv WHERE app_id=",
        );
        query.push_bind(app_id.to_owned());
        if !request.prefix.is_empty() {
            let mut upper = request.prefix.as_bytes().to_vec();
            *upper.last_mut().expect("前缀非空") += 1;
            let upper = String::from_utf8(upper).expect("键名只含 ASCII");
            query
                .push(" AND key>=")
                .push_bind(request.prefix.clone())
                .push(" AND key<")
                .push_bind(upper);
        }
        if let Some(cursor) = &cursor {
            let last = cursor
                .position
                .as_str()
                .ok_or_else(|| PlatformError::new(error_code::INVALID_CURSOR, "键值游标无效"))?;
            query.push(" AND key>").push_bind(last.to_owned());
        }
        query
            .push(" ORDER BY key COLLATE BINARY LIMIT ")
            .push_bind(i64::from(request.limit) + 1);
        #[cfg(test)]
        super::query::capture_plan(&mut query, &mut tx, "kv/list", &app.generation).await?;
        let rows: Vec<KvRow> = query.build_query_as().fetch_all(&mut *tx).await?;
        let mut records = rows
            .iter()
            .map(KvRow::record)
            .collect::<PlatformResult<Vec<_>>>()?;
        let count = tgdrive_contract::validation::json_page_prefix_count(
            &records,
            usize::from(request.limit),
            MAX_JSON_PAGE_BYTES,
        )?;
        let more = records.len() > count;
        records.truncate(count);
        let next_cursor = if more {
            Some(self.cursors.encode(&CursorData {
                kind: "kv".into(),
                app: app.id,
                generation: app.generation,
                scope: None,
                expires_at: Some(now().0 + PAGE_CURSOR_SECONDS as i64 * 1000),
                binding: fingerprint,
                position: serde_json::json!(records.last().expect("有非空页").key),
                scan: None,
                epoch: None,
                high_water: None,
            })?)
        } else {
            None
        };
        tx.commit().await?;
        Ok(KvPage {
            records,
            next_cursor,
        })
    }
    pub async fn data_usage(&self, app_id: &str, generation: &str) -> PlatformResult<DataUsage> {
        self.owner_or_page_usage(app_id, Some(generation)).await
    }
    pub(crate) async fn owner_or_page_usage(
        &self,
        app_id: &str,
        generation: Option<&str>,
    ) -> PlatformResult<DataUsage> {
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, app_id).await?;
        if let Some(generation) = generation {
            app.check(generation)?;
        }
        let usage: Option<(i64, i64)> =
            sqlx::query_as("SELECT entries,bytes FROM app_kv_usage WHERE app_id=?")
                .bind(app_id)
                .fetch_optional(&mut *tx)
                .await?;
        let limits = super::runtime::limits(&app)?;
        let (entries, bytes) = usage.unwrap_or((0, 0));
        tx.commit().await?;
        let mut cache = self.cache_usage(app_id).await?;
        let database = if limits.backend.is_some() {
            let (database, combined) = self.backend_disk_usage(app_id).await?;
            cache = combined;
            Some(tgdrive_contract::ByteCount(database))
        } else {
            None
        };
        Ok(DataUsage {
            kv_entries: entries as u32,
            kv_bytes: tgdrive_contract::ByteCount(bytes as u64),
            kv_entry_limit: limits.kv_entry_limit,
            kv_byte_limit: limits.kv_byte_limit,
            cache_bytes: tgdrive_contract::ByteCount(cache),
            cache_byte_limit: limits.cache_byte_limit,
            database_bytes: database,
            database_byte_limit: limits.backend.map(|b| b.database_bytes),
        })
    }
}
