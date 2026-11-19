//! 限时表示票据、可撤销读取租约、统一 Range/条件响应与有界原始字节流。

use super::runtime::PageSession;
use super::store;
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tgdrive_contract::data::*;
use tgdrive_contract::files::*;
use tgdrive_contract::http::*;
use tgdrive_contract::validation::{
    resolve_single_range, valid_content_type, valid_drive_name, valid_kv_key, validate_content_ref,
};
use tgdrive_contract::{Timestamp, error_code};
use tokio::io::AsyncReadExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

pub(crate) struct Lease {
    pub id: String,
    pub session: Arc<PageSession>,
    pub expires_at: AtomicI64,
    pub revoked: CancellationToken,
}
#[derive(Clone)]
struct Thumbnail {
    file_id: String,
    size: u64,
    content_type: String,
    etag: String,
}
#[derive(Clone)]
struct FileTicket {
    file: ContentRef,
    representation: FileRepresentation,
    filename: String,
    content_type: String,
    disposition: Disposition,
    thumbnail: Option<Thumbnail>,
    lease: Option<Arc<Lease>>,
    scope_revision: String,
}
#[derive(Clone)]
enum TicketKind {
    File(FileTicket),
    Cache { key: String, revision: String },
}
#[derive(Clone)]
struct Ticket {
    session: Arc<PageSession>,
    expires_at: i64,
    kind: TicketKind,
}
struct MediaInner {
    leases: BTreeMap<String, Arc<Lease>>,
    tickets: BTreeMap<String, Ticket>,
}
pub(crate) struct MediaState {
    inner: Mutex<MediaInner>,
}
impl MediaState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MediaInner {
                leases: BTreeMap::new(),
                tickets: BTreeMap::new(),
            }),
        }
    }
    fn prune(inner: &mut MediaInner) {
        let at = now().0;
        inner.leases.retain(|_, lease| {
            lease.expires_at.load(Ordering::Acquire) > at
                && lease.session.expires_at.load(Ordering::Acquire) > at
                && !lease.revoked.is_cancelled()
                && !lease.session.cancelled.is_cancelled()
        });
        inner
            .tickets
            .retain(|_, ticket| ticket.expires_at > at && !ticket.session.cancelled.is_cancelled());
    }
    fn lease(&self, session: &PageSession, id: &str) -> PlatformResult<Arc<Lease>> {
        let inner = self.inner.lock().unwrap();
        let lease = inner
            .leases
            .get(id)
            .cloned()
            .filter(|lease| {
                lease.session.app_id == session.app_id
                    && lease.session.generation == session.generation
                    && lease.session.owner_epoch == session.owner_epoch
            })
            .ok_or_else(|| {
                PlatformError::new(error_code::LEASE_EXPIRED, "读取租约不存在或已撤销")
            })?;
        if lease.expires_at.load(Ordering::Acquire) <= now().0
            || lease.session.expires_at.load(Ordering::Acquire) <= now().0
            || lease.revoked.is_cancelled()
            || lease.session.cancelled.is_cancelled()
        {
            return Err(PlatformError::new(
                error_code::LEASE_EXPIRED,
                "读取租约已失效",
            ));
        }
        Ok(lease)
    }
    fn insert_ticket(&self, ticket: Ticket) -> PlatformResult<String> {
        let mut inner = self.inner.lock().unwrap();
        Self::prune(&mut inner);
        if inner
            .tickets
            .values()
            .filter(|value| value.session.app_id == ticket.session.app_id)
            .count() as u64
            >= MAX_MEDIA_TICKETS
        {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "应用限时票据数量超过上限",
            ));
        }
        let id = opaque();
        inner.tickets.insert(id.clone(), ticket);
        Ok(id)
    }
    fn ticket(&self, id: &str) -> PlatformResult<Ticket> {
        let inner = self.inner.lock().unwrap();
        let ticket = inner.tickets.get(id).cloned().ok_or_else(|| {
            PlatformError::new(error_code::TICKET_EXPIRED, "限时票据已过期或撤销")
        })?;
        if ticket.expires_at <= now().0 || ticket.session.cancelled.is_cancelled() {
            return Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "限时票据已过期或撤销",
            ));
        }
        Ok(ticket)
    }
}

impl AppPlatform {
    pub async fn create_lease(
        &self,
        session: &Arc<PageSession>,
        request: CreateReadLeaseRequest,
    ) -> PlatformResult<ReadLease> {
        lease_ttl(request.ttl_seconds)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        store::query_roots(&mut tx, &app, &[]).await?;
        tx.commit().await?;
        let mut inner = self.media.inner.lock().unwrap();
        MediaState::prune(&mut inner);
        if inner
            .leases
            .values()
            .filter(|lease| lease.session.app_id == session.app_id)
            .count() as u64
            >= MAX_READ_LEASES
        {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "读取租约数量超过上限",
            ));
        }
        let lease = Arc::new(Lease {
            id: opaque(),
            session: session.clone(),
            expires_at: AtomicI64::new(now().0 + i64::from(request.ttl_seconds) * 1000),
            revoked: CancellationToken::new(),
        });
        let result = ReadLease {
            id: lease.id.clone(),
            expires_at: Timestamp(lease.expires_at.load(Ordering::Acquire)),
        };
        inner.leases.insert(lease.id.clone(), lease);
        Ok(result)
    }
    pub async fn renew_lease(
        &self,
        session: &Arc<PageSession>,
        id: &str,
        request: CreateReadLeaseRequest,
    ) -> PlatformResult<ReadLease> {
        lease_ttl(request.ttl_seconds)?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        store::query_roots(&mut tx, &app, &[]).await?;
        let lease = self.media.lease(session, id)?;
        let expires = now().0 + i64::from(request.ttl_seconds) * 1000;
        lease.expires_at.store(expires, Ordering::Release);
        tx.commit().await?;
        Ok(ReadLease {
            id: id.into(),
            expires_at: Timestamp(expires),
        })
    }
    pub fn revoke_lease(&self, session: &PageSession, id: &str) -> PlatformResult<()> {
        if !tgdrive_contract::validation::valid_opaque_id(id) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "读取租约 ID 无效",
            ));
        }
        let mut inner = self.media.inner.lock().unwrap();
        if inner
            .leases
            .get(id)
            .is_some_and(|lease| lease.session.app_id == session.app_id)
            && let Some(lease) = inner.leases.remove(id)
        {
            lease.revoked.cancel();
        }
        Ok(())
    }
    pub async fn media_url(
        self: &Arc<Self>,
        session: &Arc<PageSession>,
        request: MediaUrlRequest,
        origin: &str,
    ) -> PlatformResult<MediaUrl> {
        validate_content_ref(&request.file)?;
        let ttl = u64::from(
            request
                .ttl_seconds
                .unwrap_or(DEFAULT_MEDIA_TICKET_SECONDS as u32),
        );
        if ttl == 0 || ttl > MAX_MEDIA_TICKET_SECONDS {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "媒体票据 TTL 无效",
            ));
        }
        if request
            .filename
            .as_deref()
            .is_some_and(|name| !valid_drive_name(name))
            || request
                .content_type
                .as_deref()
                .is_some_and(|mime| !valid_content_type(mime))
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "文件名或 MIME 无效",
            ));
        }
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        let node = content_node(&mut tx, &app, &request.file).await?;
        if node.kind != NodeKind::File {
            return Err(PlatformError::new(
                error_code::IS_DIRECTORY,
                "目录没有字节表示",
            ));
        }
        let lease = request
            .lease_id
            .as_deref()
            .map(|id| self.media.lease(session, id))
            .transpose()?;
        let scope_revision = app.scope_revision;
        tx.commit().await?;
        let thumbnail = if request.representation == FileRepresentation::Thumbnail {
            let _permit = session.byte_slot()?;
            let first = self
                .host
                .vfs
                .db
                .first_chunk(store::node_id(&node.id)?)
                .await
                .map_err(|_| upstream())?
                .filter(|chunk| chunk.tg_thumbnail_file_id.is_some())
                .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "原文件没有附带缩略图"))?;
            let (mime, bytes) = crate::api::thumbnail_bytes(&self.host, &first)
                .await
                .map_err(|_| upstream())?;
            use sha2::{Digest, Sha256};
            Some(Thumbnail {
                file_id: first.tg_thumbnail_file_id.expect("已检查附件存在"),
                size: bytes.len() as u64,
                content_type: mime.into(),
                etag: format!(
                    "\"t{}\"",
                    crate::engine::upload::to_hex(&Sha256::digest(bytes))
                ),
            })
        } else {
            None
        };
        let mut expires =
            (now().0 + ttl as i64 * 1000).min(session.expires_at.load(Ordering::Acquire));
        if let Some(lease) = &lease {
            expires = expires.min(lease.expires_at.load(Ordering::Acquire));
        }
        let ticket = Ticket {
            session: session.clone(),
            expires_at: expires,
            kind: TicketKind::File(FileTicket {
                file: request.file.clone(),
                representation: request.representation,
                filename: request.filename.unwrap_or(node.name),
                content_type: thumbnail
                    .as_ref()
                    .map(|thumb| thumb.content_type.clone())
                    .unwrap_or_else(|| {
                        request
                            .content_type
                            .unwrap_or_else(|| "application/octet-stream".into())
                    }),
                disposition: request.disposition,
                thumbnail,
                lease,
                scope_revision,
            }),
        };
        let id = self.media.insert_ticket(ticket)?;
        Ok(MediaUrl {
            url: format!(
                "{origin}{}",
                super::endpoint_path("media.ticket", &[("ticket", &id)])
            ),
            expires_at: Timestamp(expires),
            file: request.file,
            representation: request.representation,
        })
    }
    pub async fn cache_lookup(
        self: &Arc<Self>,
        session: &Arc<PageSession>,
        request: CacheLookupRequest,
        origin: &str,
    ) -> PlatformResult<CacheLookupResponse> {
        let mut unique = std::collections::BTreeSet::new();
        if request.keys.is_empty()
            || request.keys.len() as u64 > MAX_CACHE_LOOKUP_KEYS
            || request
                .keys
                .iter()
                .any(|key| !valid_kv_key(key) || !unique.insert(key))
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "缓存查询键无效、重复或超过上限",
            ));
        }
        let mut hits = Vec::new();
        for key in request.keys {
            let hit = match self.cache_read(session, &key, false, None).await {
                Ok(hit) => hit,
                Err(error) if error.code == error_code::NOT_FOUND => continue,
                Err(error) => return Err(error),
            };
            let expiry = (now().0 + CACHE_TICKET_SECONDS as i64 * 1000)
                .min(session.expires_at.load(Ordering::Acquire));
            let ticket = self.media.insert_ticket(Ticket {
                session: session.clone(),
                expires_at: expiry,
                kind: TicketKind::Cache {
                    key,
                    revision: hit.entry.revision.clone(),
                },
            })?;
            hits.push(CacheHit {
                entry: hit.entry,
                url: format!(
                    "{origin}{}",
                    super::endpoint_path("cache.ticket", &[("ticket", &ticket)])
                ),
                expires_at: Timestamp(expiry),
            });
        }
        Ok(CacheLookupResponse { hits })
    }
    pub async fn direct_bytes(
        self: &Arc<Self>,
        session: &Arc<PageSession>,
        file: ContentRef,
        headers: &HeaderMap,
        head: bool,
    ) -> PlatformResult<Response> {
        let specification = FileTicket {
            file,
            representation: FileRepresentation::Original,
            filename: String::new(),
            content_type: "application/octet-stream".into(),
            disposition: Disposition::Inline,
            thumbnail: None,
            lease: None,
            scope_revision: String::new(),
        };
        self.file_response(session.clone(), specification, None, headers, head)
            .await
    }
    pub async fn ticket_bytes(
        self: &Arc<Self>,
        id: &str,
        cache: bool,
        headers: &HeaderMap,
        head: bool,
    ) -> PlatformResult<Response> {
        let ticket = self.media.ticket(id)?;
        ticket.session.rate_limit()?;
        match ticket.kind {
            TicketKind::File(file) if !cache => {
                self.file_response(ticket.session, file, Some(ticket.expires_at), headers, head)
                    .await
            }
            TicketKind::Cache { key, revision } if cache => {
                self.cache_response(
                    ticket.session,
                    &key,
                    Some(&revision),
                    Some(ticket.expires_at),
                    headers,
                    head,
                )
                .await
            }
            _ => Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "票据用途不匹配",
            )),
        }
    }
    async fn file_response(
        self: &Arc<Self>,
        session: Arc<PageSession>,
        mut ticket: FileTicket,
        expiry: Option<i64>,
        headers: &HeaderMap,
        head: bool,
    ) -> PlatformResult<Response> {
        validate_content_ref(&ticket.file)?;
        let permit = session.byte_slot()?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        if !ticket.scope_revision.is_empty() && ticket.scope_revision != app.scope_revision {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "票据所属目录授权已改变",
            ));
        }
        let node = content_node(&mut tx, &app, &ticket.file).await?;
        if node.kind != NodeKind::File {
            return Err(PlatformError::new(
                error_code::IS_DIRECTORY,
                "目录没有原始字节",
            ));
        }
        if ticket.scope_revision.is_empty() {
            ticket.scope_revision = app.scope_revision;
        }
        if ticket.filename.is_empty() {
            ticket.filename = node.name;
        }
        tx.commit().await?;
        let (size, etag) = if let Some(thumbnail) = &ticket.thumbnail {
            (thumbnail.size, thumbnail.etag.clone())
        } else {
            (node.size.0, format!("\"v{}\"", ticket.file.content_version))
        };
        let plan = match plan(headers, size, &etag) {
            Ok(plan) => plan,
            Err(error) => return Ok(range_error(error, size)),
        };
        let mut guard = ReadGuard {
            platform: self.clone(),
            session: session.clone(),
            expiry,
            lease: ticket.lease.clone(),
            sources: vec![ticket.file.clone()],
            scope_revision: Some(ticket.scope_revision.clone()),
            cache: None,
            thumbnail: ticket.thumbnail.as_ref().map(|thumb| {
                (
                    store::node_id(&ticket.file.id).unwrap_or(0),
                    thumb.file_id.clone(),
                )
            }),
            checked_epoch: u64::MAX,
        };
        guard.check().await?;
        let body = if head || plan.status == StatusCode::NOT_MODIFIED || plan.range.length == 0 {
            Body::empty()
        } else if ticket.representation == FileRepresentation::Thumbnail {
            let first = self
                .host
                .vfs
                .db
                .first_chunk(store::node_id(&ticket.file.id)?)
                .await
                .map_err(|_| upstream())?
                .ok_or_else(upstream)?;
            let (mime, bytes) = crate::api::thumbnail_bytes(&self.host, &first)
                .await
                .map_err(|_| upstream())?;
            use sha2::{Digest, Sha256};
            let current = format!(
                "\"t{}\"",
                crate::engine::upload::to_hex(&Sha256::digest(&bytes))
            );
            if current != etag || mime != ticket.content_type || bytes.len() as u64 != size {
                return Err(PlatformError::new(
                    error_code::CONTENT_CHANGED,
                    "缩略图表示已改变",
                ));
            }
            guard.check().await?;
            let end = plan.range.start + plan.range.length;
            let reader: crate::telegram::ByteReader = Box::pin(std::io::Cursor::new(
                bytes[plan.range.start as usize..end as usize].to_vec(),
            ));
            stream_body(reader, guard, permit, plan.range.length).await?
        } else {
            let chunks = self
                .host
                .vfs
                .db
                .chunks_of(store::node_id(&ticket.file.id)?)
                .await
                .map_err(|_| upstream())?;
            let budget = Arc::new(crate::telegram::ReadBudget::new(strict_budget(
                &chunks,
                plan.range.start,
                plan.range.length,
            )));
            let reader = crate::engine::reader::ChunkedReader::new(
                self.host.vfs.tg.clone(),
                &chunks,
                plan.range.start,
            )
            .with_read_limit(plan.range.length)
            .with_strict_ranges(Some(budget.clone()));
            let body = stream_body(Box::pin(reader), guard, permit, plan.range.length).await?;
            debug_assert!(
                budget.used() <= strict_budget(&chunks, plan.range.start, plan.range.length)
            );
            body
        };
        response(
            body,
            &plan,
            size,
            &etag,
            &ticket.content_type,
            Some((&ticket.filename, ticket.disposition)),
        )
    }
    pub async fn cache_response(
        self: &Arc<Self>,
        session: Arc<PageSession>,
        key: &str,
        revision: Option<&str>,
        expiry: Option<i64>,
        headers: &HeaderMap,
        head: bool,
    ) -> PlatformResult<Response> {
        let permit = session.byte_slot()?;
        let entry = self.cache_read(&session, key, false, revision).await?;
        let etag = format!("\"c{}\"", entry.entry.revision);
        let not_modified = super::http::etag_matches(headers, &etag);
        let mut guard = ReadGuard {
            platform: self.clone(),
            session: session.clone(),
            expiry,
            lease: None,
            sources: entry.sources.clone(),
            scope_revision: None,
            cache: Some((key.into(), entry.entry.revision.clone())),
            thumbnail: None,
            checked_epoch: u64::MAX,
        };
        guard.check().await?;
        let body = if head || not_modified {
            Body::empty()
        } else {
            let cached = self
                .cache_read(&session, key, true, Some(&entry.entry.revision))
                .await?;
            stream_body(
                Box::pin(std::io::Cursor::new(cached.bytes)),
                guard,
                permit,
                entry.entry.size.0,
            )
            .await?
        };
        let mut response = Response::builder()
            .status(if not_modified {
                StatusCode::NOT_MODIFIED
            } else {
                StatusCode::OK
            })
            .header(header::ETAG, etag)
            .header(header::CONTENT_TYPE, &entry.entry.content_type)
            .header(header::CACHE_CONTROL, "private, no-cache")
            .body(body)
            .expect("缓存头已校验");
        if !not_modified {
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&entry.entry.size.0.to_string()).expect("字节数为 ASCII"),
            );
        }
        use base64::Engine;
        tgdrive_contract::validation::validate_cache_entry_header(&entry.entry)?;
        let meta = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&entry.entry)?);
        if meta.len() as u64 > CacheEntry::HEADER_VALUE_MAX_BYTES {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "缓存元信息响应头超过上限",
            ));
        }
        response.headers_mut().insert(
            CacheEntry::METADATA_HEADER,
            HeaderValue::from_str(&meta).expect("元数据为 base64url"),
        );
        Ok(response)
    }
}
async fn content_node(
    connection: &mut sqlx::SqliteConnection,
    app: &store::Installation,
    file: &ContentRef,
) -> PlatformResult<FileNode> {
    let node = store::reference(
        connection,
        app,
        &NodeReference::Id {
            id: file.id.clone(),
            content_version: None,
        },
    )
    .await?;
    if node.kind == NodeKind::Directory {
        return Err(PlatformError::new(
            error_code::IS_DIRECTORY,
            "目录没有字节表示",
        ));
    }
    if node.content_version.as_ref() != Some(&file.content_version) {
        return Err(PlatformError::new(
            error_code::CONTENT_CHANGED,
            "文件内容版本已改变",
        ));
    }
    Ok(node)
}
fn lease_ttl(seconds: u32) -> PlatformResult<()> {
    if seconds == 0 || u64::from(seconds) > MAX_READ_LEASE_SECONDS {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "读取租约 TTL 无效",
        ));
    }
    Ok(())
}
fn upstream() -> PlatformError {
    PlatformError::new(
        error_code::UPSTREAM_UNAVAILABLE,
        "Telegram 无法满足严格范围读取",
    )
}

struct ReadPlan {
    status: StatusCode,
    range: ResolvedRange,
}
fn plan(headers: &HeaderMap, size: u64, etag: &str) -> PlatformResult<ReadPlan> {
    if super::http::etag_matches(headers, etag) {
        return Ok(ReadPlan {
            status: StatusCode::NOT_MODIFIED,
            range: ResolvedRange {
                start: 0,
                length: 0,
                partial: false,
            },
        });
    }
    let if_range = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok());
    let range = if headers.contains_key(header::IF_RANGE) && if_range != Some(etag) {
        None
    } else {
        headers
            .get(header::RANGE)
            .map(|value| {
                value.to_str().map_err(|_| {
                    PlatformError::new(error_code::RANGE_NOT_SATISFIABLE, "Range 格式无效")
                })
            })
            .transpose()?
    };
    let range = resolve_single_range(range, size)?;
    Ok(ReadPlan {
        status: if range.partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        },
        range,
    })
}
fn response(
    body: Body,
    plan: &ReadPlan,
    size: u64,
    etag: &str,
    mime: &str,
    disposition: Option<(&str, Disposition)>,
) -> PlatformResult<Response> {
    let mut response = Response::builder()
        .status(plan.status)
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-cache")
        .body(body)
        .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "字节响应头无效"))?;
    let headers = response.headers_mut();
    if plan.status != StatusCode::NOT_MODIFIED {
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&plan.range.length.to_string()).expect("长度为 ASCII"),
        );
    }
    if plan.range.partial {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{size}",
                plan.range.start,
                plan.range.start + plan.range.length - 1
            ))
            .expect("范围为 ASCII"),
        );
    }
    if let Some((filename, disposition)) = disposition {
        if !valid_drive_name(filename) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "显示文件名无效",
            ));
        }
        let encoded =
            percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC);
        let kind = if disposition == Disposition::Attachment {
            "attachment"
        } else {
            "inline"
        };
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("{kind}; filename*=UTF-8''{encoded}"))
                .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "显示文件名无效"))?,
        );
    }
    Ok(response)
}
fn strict_budget(chunks: &[crate::db::Chunk], start: u64, length: u64) -> u64 {
    let part = crate::mtproto::REQUEST_SIZE as u64;
    let end = start.saturating_add(length);
    let mut base = 0u64;
    let mut budget = 0u64;
    for chunk in chunks {
        let size = chunk.size.max(0) as u64;
        let left = start.max(base);
        let right = end.min(base.saturating_add(size));
        if left < right {
            let offset = left - base;
            let length = right - left;
            budget = budget.saturating_add(
                (offset + length)
                    .div_ceil(part)
                    .saturating_sub(offset / part)
                    .saturating_mul(part),
            );
        }
        base = base.saturating_add(size);
    }
    budget
}

struct ReadGuard {
    platform: Arc<AppPlatform>,
    session: Arc<PageSession>,
    expiry: Option<i64>,
    lease: Option<Arc<Lease>>,
    sources: Vec<ContentRef>,
    scope_revision: Option<String>,
    cache: Option<(String, String)>,
    thumbnail: Option<(i64, String)>,
    checked_epoch: u64,
}
impl ReadGuard {
    async fn check(&mut self) -> PlatformResult<()> {
        let at = now().0;
        if self.expiry.is_some_and(|expiry| expiry <= at) {
            return Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "读取票据已过期",
            ));
        }
        if self.session.cancelled.is_cancelled()
            || self.session.expires_at.load(Ordering::Acquire) <= at
            || self.session.owner_epoch != self.platform.host.auth.device_epoch()
        {
            return Err(PlatformError::new(
                error_code::SESSION_EXPIRED,
                "读取所属页面会话已撤销",
            ));
        }
        if self.platform.host.restore.maintenance()
            || self.platform.host.vfs.db.background_blocked()
        {
            return Err(PlatformError::new(
                error_code::HOST_MAINTENANCE,
                "宿主处于维护状态",
            ));
        }
        if self.lease.as_ref().is_some_and(|lease| {
            lease.revoked.is_cancelled()
                || lease.expires_at.load(Ordering::Acquire) <= at
                || lease.session.cancelled.is_cancelled()
        }) {
            return Err(PlatformError::new(
                error_code::LEASE_EXPIRED,
                "读取租约已撤销或过期",
            ));
        }
        let epoch = self.platform.host.vfs.db.app_epoch.load(Ordering::Acquire);
        if epoch != self.checked_epoch {
            let mut tx = self.platform.host.vfs.db.pool.begin().await?;
            let app = store::installation(&mut tx, &self.session.app_id).await?;
            app.check(&self.session.generation)?;
            if self
                .scope_revision
                .as_deref()
                .is_some_and(|revision| revision != app.scope_revision)
            {
                return Err(PlatformError::new(
                    error_code::SCOPE_CHANGED,
                    "读取所属目录授权已改变",
                ));
            }
            for source in &self.sources {
                store::reference(
                    &mut tx,
                    &app,
                    &NodeReference::Id {
                        id: source.id.clone(),
                        content_version: Some(source.content_version.clone()),
                    },
                )
                .await?;
            }
            tx.commit().await?;
            if let Some((id, file_id)) = &self.thumbnail {
                let chunk = self
                    .platform
                    .host
                    .vfs
                    .db
                    .first_chunk(*id)
                    .await
                    .map_err(|_| upstream())?
                    .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "缩略图不存在"))?;
                if chunk.tg_thumbnail_file_id.as_ref() != Some(file_id) {
                    return Err(PlatformError::new(
                        error_code::CONTENT_CHANGED,
                        "缩略图表示已改变",
                    ));
                }
            }
            if let Some((key, revision)) = &self.cache {
                self.platform
                    .cache_read(&self.session, key, false, Some(revision))
                    .await?;
            }
            self.checked_epoch = epoch;
        }
        Ok(())
    }
}
async fn read_guarded(
    reader: &mut crate::telegram::ByteReader,
    guard: &mut ReadGuard,
    buffer: &mut [u8],
) -> PlatformResult<usize> {
    let cancelled = guard.session.cancelled.clone();
    let lease = guard
        .lease
        .as_ref()
        .map(|lease| lease.revoked.clone())
        .unwrap_or_default();
    let shutdown = guard.platform.host.restore.shutdown.clone();
    let mut changes = guard.platform.host.vfs.db.app_changes.subscribe();
    let mut revocations = guard.platform.host.auth.revocation_signal();
    let idle = tokio::time::Instant::now() + Duration::from_secs(STREAM_IDLE_TIMEOUT_SECONDS);
    loop {
        guard.check().await?;
        let mut expires = guard.session.expires_at.load(Ordering::Acquire);
        if let Some(expiry) = guard.expiry {
            expires = expires.min(expiry);
        }
        if let Some(lease) = &guard.lease {
            expires = expires.min(lease.expires_at.load(Ordering::Acquire));
        }
        let deadline = Duration::from_millis(expires.saturating_sub(now().0).max(0) as u64);
        tokio::select! {
            _=cancelled.cancelled()=>return Err(PlatformError::new(error_code::SESSION_EXPIRED,"页面会话已撤销")),
            _=lease.cancelled()=>return Err(PlatformError::new(error_code::LEASE_EXPIRED,"读取租约已撤销")),
            _=shutdown.cancelled()=>return Err(PlatformError::new(error_code::HOST_MAINTENANCE,"宿主维护中")),
            _=revocations.changed()=>return Err(PlatformError::new(error_code::SESSION_EXPIRED,"主人登录已撤销")),
            _=changes.changed()=>{guard.check().await?;},
            _=tokio::time::sleep(deadline)=>{guard.check().await?;},
            _=tokio::time::sleep_until(idle)=>return Err(PlatformError::new(error_code::REQUEST_TIMEOUT,"字节读取空闲超时")),
            result=reader.read(buffer)=>return result.map_err(|_|upstream()),
        }
    }
}

struct StreamState {
    reader: crate::telegram::ByteReader,
    guard: ReadGuard,
    _permit: OwnedSemaphorePermit,
    remaining: u64,
    first: Option<Bytes>,
}
async fn stream_body(
    mut reader: crate::telegram::ByteReader,
    mut guard: ReadGuard,
    permit: OwnedSemaphorePermit,
    length: u64,
) -> PlatformResult<Body> {
    if length == 0 {
        return Ok(Body::empty());
    }
    guard.check().await?;
    let mut buffer = vec![0u8; length.min(64 * 1024) as usize];
    let read = read_guarded(&mut reader, &mut guard, &mut buffer).await?;
    if read == 0 {
        return Err(upstream());
    }
    buffer.truncate(read);
    guard.check().await?;
    let stream = futures_util::stream::try_unfold(
        StreamState {
            reader,
            guard,
            _permit: permit,
            remaining: length,
            first: Some(Bytes::from(buffer)),
        },
        |mut state| async move {
            if state.remaining == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            state
                .guard
                .check()
                .await
                .map_err(|_| std::io::Error::other("读取已经撤销"))?;
            let bytes = if let Some(first) = state.first.take() {
                first
            } else {
                let mut buffer = vec![0u8; state.remaining.min(64 * 1024) as usize];
                let read = read_guarded(&mut state.reader, &mut state.guard, &mut buffer)
                    .await
                    .map_err(|_| std::io::Error::other("读取已经撤销或上游中断"))?;
                if read == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "字节流提前结束",
                    ));
                }
                buffer.truncate(read);
                Bytes::from(buffer)
            };
            state
                .guard
                .check()
                .await
                .map_err(|_| std::io::Error::other("读取已经撤销"))?;
            state.remaining -= bytes.len() as u64;
            Ok(Some((bytes, state)))
        },
    );
    Ok(Body::from_stream(stream))
}

pub(crate) fn range_error(error: PlatformError, size: u64) -> Response {
    let mut response = error.into_response();
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{size}")).expect("长度为 ASCII"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weak_lists_if_range_and_single_ranges_share_one_interpretation() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=1-999"));
        let result = plan(&headers, 10, "\"v1\"").unwrap();
        assert_eq!(result.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(result.range.length, 9);
        for range in [
            "bytes=5-1",
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=-0",
            "bytes=18446744073709551616-",
        ] {
            headers.insert(header::RANGE, HeaderValue::from_str(range).unwrap());
            assert!(plan(&headers, 10, "\"v1\"").is_err());
        }
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-0"));
        headers.insert(header::IF_RANGE, HeaderValue::from_static("W/\"v1\""));
        assert_eq!(plan(&headers, 10, "\"v1\"").unwrap().status, StatusCode::OK);
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"other\", W/\"v1\""),
        );
        assert_eq!(
            plan(&headers, 10, "\"v1\"").unwrap().status,
            StatusCode::NOT_MODIFIED
        );
    }
}
