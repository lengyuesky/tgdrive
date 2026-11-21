//! 有界上传句柄；正文复用 Telegram 管线，只有条件元数据事务能使结果可见。
use super::runtime::CallerSession;
use super::store::{self, Installation};
use super::write;
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use axum::body::Body;
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tgdrive_contract::{ByteCount, Timestamp, error_code, files::*, http::*, validation::*};
use tokio_util::sync::CancellationToken;

pub(crate) struct UploadWork {
    owner: Arc<CallerSession>,
    scope_revision: String,
    scope_revoked: AtomicBool,
    revoked: CancellationToken,
    received: AtomicU64,
}
impl UploadWork {
    fn revocation_error(&self) -> PlatformError {
        if self.scope_revoked.load(Ordering::Acquire) {
            PlatformError::new(error_code::SCOPE_CHANGED, "上传所属目录授权已改变")
        } else {
            expired()
        }
    }
}
pub(crate) struct Uploads {
    work: Mutex<BTreeMap<String, Arc<UploadWork>>>,
    cursor: Mutex<String>,
    work_cursor: Mutex<String>,
}
impl Uploads {
    pub fn new() -> Self {
        Self {
            work: Mutex::new(BTreeMap::new()),
            cursor: Mutex::new(String::new()),
            work_cursor: Mutex::new(String::new()),
        }
    }
    fn get(&self, id: &str) -> Option<Arc<UploadWork>> {
        self.work.lock().unwrap().get(id).cloned()
    }
    fn revoke_scope(&self, app_id: &str, revision: &str) {
        for work in self.work.lock().unwrap().values() {
            if work.owner.app_id == app_id && work.scope_revision != revision {
                work.scope_revoked.store(true, Ordering::Release);
                work.revoked.cancel();
            }
        }
    }
}
#[derive(Clone, sqlx::FromRow)]
pub(crate) struct UploadRecord {
    pub id: String,
    pub app_id: String,
    pub generation: String,
    pub caller_id: String,
    pub caller_kind: String,
    pub idempotency_key: String,
    pub target: String,
    pub scope_revision: String,
    pub size: i64,
    pub sha256: Option<String>,
    pub received_bytes: i64,
    pub state: String,
    pub staging_node_id: Option<i64>,
    pub result: Option<String>,
    pub error_code: Option<String>,
    pub expires_at: i64,
}
impl UploadRecord {
    pub fn target(&self) -> PlatformResult<UploadTarget> {
        Ok(serde_json::from_str(&self.target)?)
    }
    fn info(&self, work: Option<&UploadWork>) -> PlatformResult<UploadInfo> {
        Ok(UploadInfo {
            id: self.id.clone(),
            state: serde_json::from_value(serde_json::Value::String(self.state.clone()))?,
            size: ByteCount(self.size as u64),
            received_bytes: ByteCount(work.map_or(self.received_bytes as u64, |work| {
                work.received
                    .load(Ordering::Acquire)
                    .max(self.received_bytes as u64)
            })),
            expires_at: Timestamp(self.expires_at),
            result: self
                .result
                .as_ref()
                .map(|result| serde_json::from_str(result))
                .transpose()?,
            error_code: self.error_code.clone(),
        })
    }
}
fn expired() -> PlatformError {
    PlatformError::new(error_code::UPLOAD_EXPIRED, "上传句柄或其创建者已失效")
}
async fn target_info(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    target: &UploadTarget,
    check_condition: bool,
) -> PlatformResult<(i64, String, Option<i64>)> {
    write::declared(app)?;
    let (parent, name, replace) = match target {
        UploadTarget::Create { parent_id, name } => {
            if !valid_drive_name(name) {
                return Err(PlatformError::new(
                    error_code::INVALID_REQUEST,
                    "上传名称无效",
                ));
            }
            let parent = store::reference(
                connection,
                app,
                &NodeReference::Id {
                    id: parent_id.clone(),
                    content_version: None,
                },
            )
            .await?;
            if parent.kind != NodeKind::Directory {
                return Err(PlatformError::new(
                    error_code::NOT_DIRECTORY,
                    "上传父节点不是目录",
                ));
            }
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND name=? AND status='ready')").bind(store::node_id(&parent.id)?).bind(name).fetch_one(&mut *connection).await?;
            if exists {
                return Err(PlatformError::new(
                    error_code::NAME_CONFLICT,
                    "目标已有同名节点",
                ));
            }
            if (if parent.path == "/" {
                0
            } else {
                parent.path.len()
            }) + 1
                + name.len()
                > MAX_DRIVE_PATH_BYTES as usize
            {
                return Err(PlatformError::new(
                    error_code::INVALID_PATH,
                    "上传路径超过上限",
                ));
            }
            (store::node_id(&parent.id)?, name.clone(), None)
        }
        UploadTarget::Replace { node } => {
            if !valid_opaque_id(&node.revision) {
                return Err(PlatformError::new(
                    error_code::INVALID_REQUEST,
                    "覆盖条件无效",
                ));
            }
            let current = store::reference(
                connection,
                app,
                &NodeReference::Id {
                    id: node.id.clone(),
                    content_version: None,
                },
            )
            .await?;
            if current.kind != NodeKind::File {
                return Err(PlatformError::new(
                    error_code::IS_DIRECTORY,
                    "不能用上传覆盖目录",
                ));
            }
            if check_condition && current.revision != node.revision {
                return Err(PlatformError::new(
                    error_code::NODE_CHANGED,
                    "覆盖目标已改变",
                ));
            }
            let id = store::node_id(&current.id)?;
            write::write_access(connection, app, id).await?;
            (
                store::node_id(current.parent_id.as_ref().ok_or_else(expired)?)?,
                current.name,
                Some(id),
            )
        }
    };
    write::write_access(connection, app, parent).await?;
    Ok((parent, name, replace))
}

impl AppPlatform {
    pub(crate) async fn update_scope(
        &self,
        app_id: &str,
        request: tgdrive_contract::runtime::ScopeUpdateRequest,
    ) -> PlatformResult<tgdrive_contract::files::ScopeInfo> {
        let result = store::update_scope(&self.host.vfs.db, app_id, request).await?;
        // 返回主人之前同步撤销旧授权句柄，空闲输入也不必先等待 SQLite 重核或 SSE。
        self.uploads.revoke_scope(app_id, &result.revision);
        Ok(result)
    }
    pub(crate) async fn upload_record(
        &self,
        connection: &mut sqlx::SqliteConnection,
        app: &str,
        id: &str,
    ) -> PlatformResult<UploadRecord> {
        if !valid_opaque_id(id) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "上传标识无效",
            ));
        }
        sqlx::query_as("SELECT * FROM app_uploads WHERE id=? AND app_id=?")
            .bind(id)
            .bind(app)
            .fetch_optional(connection)
            .await?
            .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "上传句柄不存在"))
    }
    pub(crate) fn upload_owner_live(&self, upload: &UploadRecord) -> PlatformResult<()> {
        if upload.expires_at <= now().0 {
            return Err(expired());
        }
        let work = self.uploads.get(&upload.id).ok_or_else(expired)?;
        if work.scope_revoked.load(Ordering::Acquire) {
            return Err(work.revocation_error());
        }
        if work.revoked.is_cancelled()
            || work.owner.id != upload.caller_id
            || work.owner.generation != upload.generation
            || (if work.owner.is_page() {
                "page"
            } else {
                "backend"
            }) != upload.caller_kind
        {
            return Err(expired());
        }
        write::caller_live(self, &work.owner).map_err(|_| expired())
    }
    pub(crate) async fn validate_upload_commit(
        &self,
        connection: &mut sqlx::SqliteConnection,
        app: &Installation,
        upload: &UploadRecord,
    ) -> PlatformResult<()> {
        self.upload_owner_live(upload)?;
        if upload.generation != app.generation || upload.scope_revision != app.scope_revision {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "上传授权或安装轮次已改变",
            ));
        }
        if upload.state != "ready" {
            return Err(PlatformError::new(
                error_code::UPLOAD_STATE,
                "只有 ready 上传可以提交",
            ));
        }
        target_info(connection, app, &upload.target()?, true).await?;
        Ok(())
    }
    pub(crate) async fn commit_upload_in(
        &self,
        connection: &mut sqlx::SqliteConnection,
        app: &Installation,
        upload: &UploadRecord,
    ) -> PlatformResult<ContentRef> {
        self.upload_owner_live(upload)?;
        let (parent, name, replace) =
            target_info(connection, app, &upload.target()?, false).await?;
        let staging = upload.staging_node_id.ok_or_else(expired)?;
        let node =
            crate::db::Db::commit_app_staging_in(connection, staging, parent, &name, replace)
                .await
                .map_err(write::storage_error)?;
        let current = store::node_row(connection, node).await?;
        let result = ContentRef {
            id: node.to_string(),
            content_version: current.content_version.ok_or_else(expired)?,
        };
        sqlx::query("UPDATE app_uploads SET state='committed',result=?,staging_node_id=NULL,received_bytes=size,expires_at=? WHERE id=? AND state='ready'")
            .bind(serde_json::to_string(&result)?).bind(now().0 + MUTATION_RECEIPT_SECONDS as i64 * 1000).bind(&upload.id).execute(connection).await?;
        Ok(result)
    }
    pub(crate) fn forget_committed_uploads<'a>(&self, ids: impl Iterator<Item = &'a String>) {
        let mut work = self.uploads.work.lock().unwrap();
        for id in ids {
            work.remove(id);
        }
    }
    pub async fn create_upload(
        self: &Arc<Self>,
        caller: &Arc<CallerSession>,
        key: &str,
        request: CreateUploadRequest,
    ) -> PlatformResult<UploadInfo> {
        write::idempotency_key(key)?;
        if request.size.0 > i64::MAX as u64
            || !valid_opaque_id(&request.scope_revision)
            || request
                .sha256
                .as_deref()
                .is_some_and(|sha| !valid_sha256(sha))
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "上传大小、摘要或授权修订无效",
            ));
        }
        write::caller_live(self, caller)?;
        self.prune_uploads().await?;
        let canonical = serde_json::to_string(&request)?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, &caller.app_id).await?;
        app.check(&caller.generation)?;
        if let Some(id) = write::existing_request(&mut tx, &app, key, "upload", &canonical).await? {
            let id: String = serde_json::from_str(&id)?;
            let record = self.upload_record(&mut tx, &app.id, &id).await?;
            if record.expires_at <= now().0 {
                return Err(expired());
            }
            return record.info(self.uploads.get(&id).as_deref());
        }
        if app.scope_revision != request.scope_revision {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "上传授权修订已改变",
            ));
        }
        target_info(&mut tx, &app, &request.target, true).await?;
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM app_uploads WHERE app_id=? AND generation=? AND state IN('awaiting_body','receiving','ready') AND expires_at>?").bind(&app.id).bind(&app.generation).bind(now().0).fetch_one(&mut *tx).await?;
        if active as u64 >= MAX_UPLOADS {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "未提交上传数量已满",
            ));
        }
        let id = opaque();
        let expiry = now().0 + UPLOAD_RETENTION_SECONDS as i64 * 1000;
        sqlx::query("INSERT INTO app_uploads(id,app_id,generation,caller_id,caller_kind,idempotency_key,target,scope_revision,size,sha256,state,expires_at,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,'awaiting_body',?,?)")
            .bind(&id).bind(&app.id).bind(&app.generation).bind(&caller.id).bind(if caller.is_page() { "page" } else { "backend" }).bind(key).bind(serde_json::to_string(&request.target)?).bind(&request.scope_revision).bind(request.size.0 as i64).bind(&request.sha256).bind(expiry).bind(now().0).execute(&mut *tx).await?;
        write::bind_request(
            &mut tx,
            &app,
            key,
            "upload",
            &canonical,
            &serde_json::to_string(&id)?,
            now().0 + MUTATION_RECEIPT_SECONDS as i64 * 1000,
        )
        .await?;
        write::caller_live(self, caller)?;
        self.uploads.work.lock().unwrap().insert(
            id.clone(),
            Arc::new(UploadWork {
                owner: caller.clone(),
                scope_revision: request.scope_revision.clone(),
                scope_revoked: AtomicBool::new(false),
                revoked: CancellationToken::new(),
                received: AtomicU64::new(0),
            }),
        );
        tx.commit().await?;
        self.upload_status(caller, &id).await
    }
    pub async fn upload_status(
        &self,
        caller: &CallerSession,
        id: &str,
    ) -> PlatformResult<UploadInfo> {
        let mut connection = self.host.vfs.db.pool.acquire().await?;
        let record = self
            .upload_record(&mut connection, &caller.app_id, id)
            .await?;
        if record.expires_at <= now().0 {
            return Err(expired());
        }
        record.info(self.uploads.get(id).as_deref())
    }
    pub async fn cancel_upload(&self, caller: &CallerSession, id: &str) -> PlatformResult<()> {
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let record = self.upload_record(&mut tx, &caller.app_id, id).await?;
        if record.state == "committed" {
            return Err(PlatformError::new(
                error_code::UPLOAD_STATE,
                "已提交上传不能取消为删除文件",
            ));
        }
        let work = self.uploads.get(id);
        if let Some(work) = &work {
            work.revoked.cancel();
        }
        sqlx::query("UPDATE app_uploads SET state='cancelled',received_bytes=? WHERE id=? AND state<>'committed'")
            .bind(work.as_ref().map_or(record.received_bytes, |work| work.received.load(Ordering::Acquire) as i64)).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        if let Some(node) = record.staging_node_id {
            self.host
                .vfs
                .db
                .abort_upload(node)
                .await
                .map_err(write::storage_error)?;
        }
        self.uploads.work.lock().unwrap().remove(id);
        Ok(())
    }
    pub async fn upload_body(
        self: &Arc<Self>,
        caller: &Arc<CallerSession>,
        id: &str,
        body: Body,
        headers: &HeaderMap,
    ) -> PlatformResult<UploadInfo> {
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some("application/octet-stream")
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "上传正文需要 application/octet-stream",
            ));
        }
        if !headers.contains_key(header::CONTENT_LENGTH)
            && !headers.contains_key(header::TRANSFER_ENCODING)
        {
            return Err(PlatformError::new(
                error_code::LENGTH_REQUIRED,
                "上传正文需要长度或分块编码",
            ));
        }
        let _permit = caller.byte_slot()?;
        write::caller_live(self, caller)?;
        let work = self.uploads.get(id).ok_or_else(expired)?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, &caller.app_id).await?;
        app.check(&caller.generation)?;
        let record = self.upload_record(&mut tx, &app.id, id).await?;
        self.upload_owner_live(&record)?;
        if record.state != "awaiting_body" {
            return Err(PlatformError::new(
                error_code::UPLOAD_STATE,
                "此上传句柄已经接收过正文",
            ));
        }
        if app.scope_revision != record.scope_revision || app.generation != record.generation {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "上传授权或安装轮次已改变",
            ));
        }
        let (parent, name, _) = target_info(&mut tx, &app, &record.target()?, true).await?;
        let staging = crate::db::Db::create_app_staging_in(&mut tx, parent)
            .await
            .map_err(write::storage_error)?;
        sqlx::query("UPDATE app_uploads SET state='receiving',staging_node_id=? WHERE id=? AND state='awaiting_body'").bind(staging).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        let result = self
            .receive_upload(caller, &work, &record, staging, &name, body)
            .await;
        if let Err(error) = result {
            let uncertain = error.code == "upload_uncertain";
            sqlx::query("UPDATE app_uploads SET state=CASE WHEN state='cancelled' THEN state WHEN ? THEN 'receiving' ELSE 'failed' END,error_code=?,received_bytes=? WHERE id=? AND state<>'committed'")
                .bind(uncertain).bind(if uncertain { error_code::UPSTREAM_UNAVAILABLE } else { &error.code }).bind(work.received.load(Ordering::Acquire) as i64).bind(id).execute(&self.host.vfs.db.pool).await?;
            self.host
                .vfs
                .db
                .abort_upload(staging)
                .await
                .map_err(write::storage_error)?;
            return Err(if uncertain {
                PlatformError::new(
                    error_code::UPSTREAM_UNAVAILABLE,
                    "上传结果未知，请核实句柄状态，不要重复正文",
                )
            } else {
                error
            });
        }
        self.upload_status(caller, id).await
    }
    async fn receive_upload(
        self: &Arc<Self>,
        caller: &Arc<CallerSession>,
        work: &Arc<UploadWork>,
        record: &UploadRecord,
        staging: i64,
        name: &str,
        body: Body,
    ) -> PlatformResult<()> {
        let handle = self
            .host
            .vfs
            .uploader
            .start_staging(staging, name)
            .await
            .map_err(write::storage_error)?;
        let mut stream = body.into_data_stream();
        let mut changes = self.host.vfs.db.app_changes.subscribe();
        let mut checked_epoch = u64::MAX;
        let mut persisted = 0u64;
        loop {
            write::caller_live(self, caller)?;
            self.upload_owner_live(record)?;
            if work.revoked.is_cancelled() {
                return Err(expired());
            }
            let epoch = self.host.vfs.db.app_epoch.load(Ordering::Acquire);
            if epoch != checked_epoch {
                let mut connection = self.host.vfs.db.pool.acquire().await?;
                let current = store::installation(&mut connection, &record.app_id).await?;
                current.check(&record.generation)?;
                if current.scope_revision != record.scope_revision {
                    return Err(PlatformError::new(
                        error_code::SCOPE_CHANGED,
                        "接收期间目录授权改变",
                    ));
                }
                target_info(&mut connection, &current, &record.target()?, true).await?;
                checked_epoch = epoch;
            }
            let timeout = Duration::from_millis((record.expires_at - now().0).max(0) as u64)
                .min(Duration::from_secs(STREAM_IDLE_TIMEOUT_SECONDS));
            let chunk = tokio::select! {
                _ = work.revoked.cancelled() => return Err(work.revocation_error()),
                _ = work.owner.cancelled.cancelled() => return Err(expired()),
                _ = caller.cancelled.cancelled() => return Err(expired()),
                _ = self.host.restore.shutdown.cancelled() => return Err(expired()),
                _ = changes.changed() => continue,
                chunk = tokio::time::timeout(timeout, stream.next()) => chunk.map_err(|_| PlatformError::new(error_code::REQUEST_TIMEOUT, "上传输入空闲超时"))?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let bytes = chunk
                .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "上传输入中断"))?;
            let received = work
                .received
                .load(Ordering::Acquire)
                .saturating_add(bytes.len() as u64);
            if received > record.size as u64 {
                return Err(PlatformError::new(
                    error_code::PAYLOAD_TOO_LARGE,
                    "上传实际长度超过声明",
                ));
            }
            work.received.store(received, Ordering::Release);
            tokio::select! {
                _ = work.revoked.cancelled() => return Err(work.revocation_error()),
                _ = work.owner.cancelled.cancelled() => return Err(expired()),
                _ = caller.cancelled.cancelled() => return Err(expired()),
                _ = self.host.restore.shutdown.cancelled() => return Err(expired()),
                result = handle.feed(bytes) => result.map_err(write::storage_error)?,
            }
            if received - persisted >= 1024 * 1024 {
                sqlx::query(
                    "UPDATE app_uploads SET received_bytes=? WHERE id=? AND state='receiving'",
                )
                .bind(received as i64)
                .bind(&record.id)
                .execute(&self.host.vfs.db.pool)
                .await?;
                persisted = received;
            }
        }
        if work.received.load(Ordering::Acquire) != record.size as u64 {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "上传实际长度不足",
            ));
        }
        let node = handle.finish().await.map_err(|error| {
            if matches!(error, crate::error::AppError::TelegramUncertain(_)) {
                PlatformError::new("upload_uncertain", "上传结果未知")
            } else {
                write::storage_error(error)
            }
        })?;
        if record
            .sha256
            .as_ref()
            .is_some_and(|sha| node.sha256.as_ref() != Some(sha))
        {
            return Err(PlatformError::new(
                error_code::CONTENT_CHANGED,
                "上传摘要不匹配",
            ));
        }
        self.upload_owner_live(record)?;
        write::caller_live(self, caller)?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = store::installation(&mut tx, &record.app_id).await?;
        current.check(&record.generation)?;
        if current.scope_revision != record.scope_revision {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "上传结束前目录授权改变",
            ));
        }
        target_info(&mut tx, &current, &record.target()?, true).await?;
        let updated = sqlx::query("UPDATE app_uploads SET state='ready',received_bytes=size WHERE id=? AND state='receiving'").bind(&record.id).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(expired());
        }
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn prune_uploads(&self) -> PlatformResult<()> {
        let cursor = self.uploads.cursor.lock().unwrap().clone();
        let records: Vec<UploadRecord> = sqlx::query_as("SELECT * FROM app_uploads WHERE state IN('awaiting_body','receiving','ready') AND id>? ORDER BY id LIMIT 256")
            .bind(cursor).fetch_all(&self.host.vfs.db.pool).await?;
        *self.uploads.cursor.lock().unwrap() = if records.len() == 256 {
            records.last().expect("有满页").id.clone()
        } else {
            String::new()
        };
        for record in records {
            let installation: Option<(String, bool, String)> = sqlx::query_as(
                "SELECT generation,enabled,scope_revision FROM app_installations WHERE id=?",
            )
            .bind(&record.app_id)
            .fetch_optional(&self.host.vfs.db.pool)
            .await?;
            let valid = installation.is_some_and(|(generation, enabled, scope)| {
                enabled && generation == record.generation && scope == record.scope_revision
            }) && self.upload_owner_live(&record).is_ok();
            if !valid {
                if let Some(work) = self.uploads.get(&record.id) {
                    work.revoked.cancel();
                }
                sqlx::query("UPDATE app_uploads SET state='cancelled',error_code=? WHERE id=? AND state IN('awaiting_body','receiving','ready')").bind(error_code::UPLOAD_EXPIRED).bind(&record.id).execute(&self.host.vfs.db.pool).await?;
                if let Some(node) = record.staging_node_id {
                    self.host
                        .vfs
                        .db
                        .abort_upload(node)
                        .await
                        .map_err(write::storage_error)?;
                }
                self.uploads.work.lock().unwrap().remove(&record.id);
            }
        }
        sqlx::query("DELETE FROM app_write_requests WHERE expires_at<=?")
            .bind(now().0)
            .execute(&self.host.vfs.db.pool)
            .await?;
        sqlx::query("DELETE FROM app_uploads WHERE (state='committed' AND expires_at<=?) OR (state IN('failed','cancelled') AND expires_at<=?)")
            .bind(now().0).bind(now().0 - MUTATION_RECEIPT_SECONDS as i64 * 1000).execute(&self.host.vfs.db.pool).await?;
        let last = self.uploads.work_cursor.lock().unwrap().clone();
        let ids = self
            .uploads
            .work
            .lock()
            .unwrap()
            .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
            .take(256)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        *self.uploads.work_cursor.lock().unwrap() = if ids.len() == 256 {
            ids.last().expect("有满页").clone()
        } else {
            String::new()
        };
        if !ids.is_empty() {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "SELECT id,state FROM app_uploads WHERE id IN(",
            );
            let mut values = query.separated(",");
            for id in &ids {
                values.push_bind(id);
            }
            values.push_unseparated(")");
            let states: Vec<(String, String)> = query
                .build_query_as()
                .fetch_all(&self.host.vfs.db.pool)
                .await?;
            let states = states.into_iter().collect::<BTreeMap<_, _>>();
            let mut work = self.uploads.work.lock().unwrap();
            for id in ids {
                if states.get(&id).is_none_or(|state| {
                    matches!(state.as_str(), "committed" | "failed" | "cancelled")
                }) {
                    work.remove(&id);
                }
            }
        }
        Ok(())
    }
}
