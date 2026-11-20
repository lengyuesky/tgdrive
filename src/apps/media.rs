//! 限时表示票据、可撤销读取租约、统一 Range/条件响应与有界原始字节流。

use super::runtime::CallerSession;
use super::store;
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tgdrive_contract::data::*;
use tgdrive_contract::files::*;
use tgdrive_contract::http::*;
use tgdrive_contract::validation::{
    resolve_single_range, valid_content_type, valid_drive_name, valid_kv_key, validate_content_ref,
};
use tgdrive_contract::{ByteCount, Timestamp, error_code};
use tokio::io::AsyncReadExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

pub(crate) struct Lease {
    pub id: String,
    pub session: Arc<CallerSession>,
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
    session: Arc<CallerSession>,
    expires_at: i64,
    kind: TicketKind,
}
struct LocalTicket {
    session: Arc<CallerSession>,
    file: ContentRef,
    scope_revision: String,
    expires_at: i64,
    lease: Option<Arc<Lease>>,
    revoked: CancellationToken,
    budget: Arc<crate::telegram::ReadBudget>,
    requests: AtomicU64,
}
#[derive(Default)]
struct FileRead {
    expiry: Option<i64>,
    head: bool,
    control: Option<(StreamDirective, String)>,
    local: Option<Arc<LocalTicket>>,
    local_remaining: u64,
}
struct MediaInner {
    leases: BTreeMap<String, Arc<Lease>>,
    tickets: BTreeMap<String, Ticket>,
    local: BTreeMap<String, Arc<LocalTicket>>,
}
pub(crate) struct MediaState {
    inner: Mutex<MediaInner>,
    loopback_port: tokio::sync::OnceCell<u16>,
    shutdown: CancellationToken,
}
impl MediaState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MediaInner {
                leases: BTreeMap::new(),
                tickets: BTreeMap::new(),
                local: BTreeMap::new(),
            }),
            loopback_port: tokio::sync::OnceCell::new(),
            shutdown: CancellationToken::new(),
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
        inner.local.retain(|_, ticket| {
            let valid = ticket.expires_at > at
                && !ticket.session.cancelled.is_cancelled()
                && !ticket.revoked.is_cancelled();
            if !valid {
                ticket.revoked.cancel();
            }
            valid
        });
    }
    fn lease(&self, session: &CallerSession, id: &str) -> PlatformResult<Arc<Lease>> {
        let inner = self.inner.lock().unwrap();
        let lease = inner
            .leases
            .get(id)
            .cloned()
            .filter(|lease| {
                lease.session.app_id == session.app_id
                    && lease.session.generation == session.generation
                    && (!lease.session.is_page()
                        || !session.is_page()
                        || lease.session.owner_epoch == session.owner_epoch)
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
        if (inner
            .tickets
            .values()
            .filter(|value| value.session.app_id == ticket.session.app_id)
            .count()
            + inner
                .local
                .values()
                .filter(|value| value.session.app_id == ticket.session.app_id)
                .count()) as u64
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

impl Drop for MediaState {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl AppPlatform {
    async fn loopback_port(self: &Arc<Self>) -> PlatformResult<u16> {
        let port = self.media.loopback_port.get_or_try_init(|| async {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let port = listener.local_addr()?.port();
            let weak = Arc::downgrade(self);
            let stop = self.media.shutdown.clone();
            let host_stop = self.host.restore.shutdown.clone();
            let routes = axum::Router::new()
                .route(&super::endpoint_path("local.ticket", &[]), axum::routing::any(local_http))
                .with_state(weak);
            tokio::spawn(async move {
                let _ = axum::serve(listener, routes)
                    .with_graceful_shutdown(async move { tokio::select! { _ = stop.cancelled() => {}, _ = host_stop.cancelled() => {} } })
                    .await;
            });
            Ok::<_, PlatformError>(port)
        }).await?;
        Ok(*port)
    }
    pub async fn local_url(
        self: &Arc<Self>,
        session: &Arc<CallerSession>,
        request: LocalFileUrlRequest,
    ) -> PlatformResult<LocalFileUrl> {
        if session.is_page() {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "本机地址仅供应用后端",
            ));
        }
        validate_content_ref(&request.file)?;
        let ttl = u64::from(
            request
                .ttl_seconds
                .unwrap_or(DEFAULT_LOCAL_TICKET_SECONDS as u16),
        );
        let bytes = request
            .byte_budget
            .unwrap_or(ByteCount(DEFAULT_LOCAL_BYTE_BUDGET))
            .0;
        let requests = u64::from(
            request
                .request_budget
                .unwrap_or(DEFAULT_LOCAL_REQUEST_BUDGET as u16),
        );
        if !(1..=MAX_LOCAL_TICKET_SECONDS).contains(&ttl)
            || !(1..=MAX_LOCAL_BYTE_BUDGET).contains(&bytes)
            || !(1..=MAX_LOCAL_REQUEST_BUDGET).contains(&requests)
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "本机地址 TTL 或预算无效",
            ));
        }
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        content_node(&mut tx, &app, &request.file).await?;
        let lease = request
            .lease_id
            .as_deref()
            .map(|id| self.media.lease(session, id))
            .transpose()?;
        let mut expires = now().0 + ttl as i64 * 1000;
        if let Some(lease) = &lease {
            expires = expires.min(lease.expires_at.load(Ordering::Acquire));
        }
        tx.commit().await?;
        let port = self.loopback_port().await?;
        let id = opaque();
        let mut inner = self.media.inner.lock().unwrap();
        MediaState::prune(&mut inner);
        let count = inner
            .tickets
            .values()
            .filter(|ticket| ticket.session.app_id == session.app_id)
            .count()
            + inner
                .local
                .values()
                .filter(|ticket| ticket.session.app_id == session.app_id)
                .count();
        if count as u64 >= MAX_MEDIA_TICKETS {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "本应用票据数量已满",
            ));
        }
        inner.local.insert(
            id.clone(),
            Arc::new(LocalTicket {
                session: session.clone(),
                file: request.file,
                scope_revision: app.scope_revision,
                expires_at: expires,
                lease,
                revoked: CancellationToken::new(),
                budget: Arc::new(crate::telegram::ReadBudget::new(bytes)),
                requests: AtomicU64::new(requests),
            }),
        );
        Ok(LocalFileUrl {
            url: format!(
                "http://127.0.0.1:{port}{}",
                super::endpoint_path("local.ticket", &[("ticket", &id)])
            ),
            id,
            expires_at: Timestamp(expires),
            byte_budget: ByteCount(bytes),
            request_budget: requests as u16,
        })
    }
    pub fn revoke_local_url(&self, session: &CallerSession, id: &str) -> PlatformResult<()> {
        if session.is_page() {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "本机地址仅供应用后端",
            ));
        }
        if !tgdrive_contract::validation::valid_opaque_id(id) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "本机地址 ID 无效",
            ));
        }
        let mut inner = self.media.inner.lock().unwrap();
        if inner.local.get(id).is_some_and(|ticket| {
            ticket.session.app_id == session.app_id && ticket.session.id == session.id
        }) && let Some(ticket) = inner.local.remove(id)
        {
            ticket.revoked.cancel();
        }
        Ok(())
    }
    pub(super) async fn local_bytes(
        self: &Arc<Self>,
        id: &str,
        headers: &HeaderMap,
        head: bool,
    ) -> PlatformResult<Response> {
        let ticket = self
            .media
            .inner
            .lock()
            .unwrap()
            .local
            .get(id)
            .cloned()
            .ok_or_else(|| PlatformError::new(error_code::TICKET_EXPIRED, "本机地址已撤销"))?;
        if ticket.revoked.is_cancelled()
            || ticket.session.cancelled.is_cancelled()
            || ticket.expires_at <= now().0
        {
            return Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "本机地址已失效",
            ));
        }
        let left = ticket.budget.remaining();
        if left == 0 {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "本机地址字节预算已耗尽",
            ));
        }
        #[allow(deprecated)]
        ticket
            .requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .map_err(|_| {
                PlatformError::new(error_code::QUOTA_EXCEEDED, "本机地址请求预算已耗尽")
            })?;
        let file = FileTicket {
            file: ticket.file.clone(),
            representation: FileRepresentation::Original,
            filename: String::new(),
            content_type: "application/octet-stream".into(),
            disposition: Disposition::Inline,
            thumbnail: None,
            lease: ticket.lease.clone(),
            scope_revision: ticket.scope_revision.clone(),
        };
        self.file_response(
            ticket.session.clone(),
            file,
            headers,
            FileRead {
                expiry: Some(ticket.expires_at),
                head,
                local: Some(ticket),
                local_remaining: left,
                control: None,
            },
        )
        .await
    }
    pub async fn create_lease(
        &self,
        session: &Arc<CallerSession>,
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
        session: &Arc<CallerSession>,
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
    pub fn revoke_lease(&self, session: &CallerSession, id: &str) -> PlatformResult<()> {
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
        session: &Arc<CallerSession>,
        request: MediaUrlRequest,
        _origin: &str,
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
            url: super::endpoint_path("media.ticket", &[("ticket", &id)]),
            expires_at: Timestamp(expires),
            file: request.file,
            representation: request.representation,
        })
    }
    pub async fn cache_lookup(
        self: &Arc<Self>,
        session: &Arc<CallerSession>,
        request: CacheLookupRequest,
        _origin: &str,
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
                url: super::endpoint_path("cache.ticket", &[("ticket", &ticket)]),
                expires_at: Timestamp(expiry),
            });
        }
        Ok(CacheLookupResponse { hits })
    }
    pub async fn direct_bytes(
        self: &Arc<Self>,
        session: &Arc<CallerSession>,
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
        self.file_response(
            session.clone(),
            specification,
            headers,
            FileRead {
                head,
                ..Default::default()
            },
        )
        .await
    }
    pub(crate) fn validate_backend_ticket(
        &self,
        caller: &CallerSession,
        id: &str,
    ) -> PlatformResult<()> {
        let ticket = self.media.ticket(id)?;
        if ticket.session.is_page()
            || ticket.session.app_id != caller.app_id
            || ticket.session.id != caller.id
            || ticket.session.generation != caller.generation
        {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "套接字只能读取本应用本轮后端票据",
            ));
        }
        Ok(())
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
                self.file_response(
                    ticket.session,
                    file,
                    headers,
                    FileRead {
                        expiry: Some(ticket.expires_at),
                        head,
                        ..Default::default()
                    },
                )
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
    pub async fn directive_response(
        self: &Arc<Self>,
        session: Arc<CallerSession>,
        directive: StreamDirective,
        headers: &HeaderMap,
        method: &axum::http::Method,
        request_id: String,
    ) -> PlatformResult<Response> {
        let lease = directive
            .lease_id
            .as_deref()
            .map(|id| self.media.lease(&session, id))
            .transpose()?;
        let ticket = FileTicket {
            file: directive.file.clone(),
            representation: FileRepresentation::Original,
            filename: directive.filename.clone().unwrap_or_default(),
            content_type: directive
                .content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into()),
            disposition: directive.disposition,
            thumbnail: None,
            lease,
            scope_revision: String::new(),
        };
        self.file_response(
            session,
            ticket,
            headers,
            FileRead {
                head: *method == axum::http::Method::HEAD,
                control: Some((directive, request_id)),
                ..Default::default()
            },
        )
        .await
    }
    async fn file_response(
        self: &Arc<Self>,
        session: Arc<CallerSession>,
        mut ticket: FileTicket,
        headers: &HeaderMap,
        options: FileRead,
    ) -> PlatformResult<Response> {
        let FileRead {
            expiry,
            head,
            control,
            local,
            local_remaining,
        } = options;
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
        let (mut size, mut etag) = if let Some(thumbnail) = &ticket.thumbnail {
            (thumbnail.size, thumbnail.etag.clone())
        } else {
            (node.size.0, format!("\"v{}\"", ticket.file.content_version))
        };
        let mut offset = 0u64;
        let mut effective = headers.clone();
        if let Some((directive, _)) = &control {
            tgdrive_contract::validation::validate_stream_directive(directive, node.size.0)?;
            if let Some(window) = &directive.window {
                offset = window.offset.0;
                size = window.length.0;
                etag = format!("\"v{}:{}:{}\"", ticket.file.content_version, offset, size);
            }
            if directive.range == RangePolicy::Ignore {
                effective.remove(header::RANGE);
                effective.remove(header::IF_RANGE);
            }
        }
        let plan = match plan_budget(
            &effective,
            size,
            &etag,
            local.as_ref().map(|_| local_remaining),
        ) {
            Ok(plan) => plan,
            Err(error) => return Ok(range_error(error, size)),
        };
        let mut guard = ReadGuard {
            platform: self.clone(),
            session: session.clone(),
            expiry,
            additional_cancel: local.as_ref().map(|ticket| ticket.revoked.clone()),
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
            let absolute_start = offset + plan.range.start;
            let budget = local
                .as_ref()
                .map(|ticket| ticket.budget.clone())
                .unwrap_or_else(|| {
                    Arc::new(crate::telegram::ReadBudget::new(strict_budget(
                        &chunks,
                        absolute_start,
                        plan.range.length,
                    )))
                });
            let reader = crate::engine::reader::ChunkedReader::new(
                self.host.vfs.tg.clone(),
                &chunks,
                absolute_start,
            )
            .with_read_limit(plan.range.length)
            .with_strict_ranges(Some(budget.clone()));
            let observation = control.as_ref().and_then(|(directive, request_id)| {
                directive.correlation_id.as_ref().map(|correlation| {
                    Observation::new(
                        self.clone(),
                        &session.app_id,
                        request_id.clone(),
                        correlation.clone(),
                        ticket.file.clone(),
                        ByteWindow {
                            offset: ByteCount(absolute_start),
                            length: ByteCount(plan.range.length),
                        },
                        directive.lease_id.clone(),
                    )
                })
            });
            let body = stream_body_observed(
                Box::pin(reader),
                guard,
                permit,
                plan.range.length,
                observation,
            )
            .await?;
            if local.is_none() {
                debug_assert!(
                    budget.used() <= strict_budget(&chunks, absolute_start, plan.range.length)
                );
            }
            body
        };
        let mut result = response(
            body,
            &plan,
            size,
            &etag,
            &ticket.content_type,
            Some((&ticket.filename, ticket.disposition)),
        )?;
        if let Some((directive, request_id)) = &control {
            result.headers_mut().insert(
                "x-tgdrive-request-id",
                HeaderValue::from_str(request_id).expect("关联请求 ID 为 ASCII"),
            );
            result.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(if directive.cache == StreamCachePolicy::NoStore {
                    "no-store"
                } else {
                    "private, no-cache"
                }),
            );
            if directive.range == RangePolicy::Ignore {
                result
                    .headers_mut()
                    .insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
            }
        }
        Ok(result)
    }
    pub async fn cache_response(
        self: &Arc<Self>,
        session: Arc<CallerSession>,
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
            additional_cancel: None,
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
async fn local_http(
    axum::extract::State(weak): axum::extract::State<std::sync::Weak<AppPlatform>>,
    axum::extract::Path(ticket): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> Response {
    let Some(platform) = weak.upgrade() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let result = if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        Err(PlatformError::new(
            error_code::METHOD_NOT_ALLOWED,
            "本机地址只支持 GET/HEAD",
        ))
    } else {
        platform
            .local_bytes(
                &ticket,
                request.headers(),
                *request.method() == axum::http::Method::HEAD,
            )
            .await
    };
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(EXTERNAL_CSP),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response.headers_mut().insert(
        "x-tgdrive-request-id",
        HeaderValue::from_str(&opaque()).expect("随机 ID 为 ASCII"),
    );
    if *request.method() == axum::http::Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
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
#[cfg(test)]
fn plan(headers: &HeaderMap, size: u64, etag: &str) -> PlatformResult<ReadPlan> {
    plan_budget(headers, size, etag, None)
}
fn plan_budget(
    headers: &HeaderMap,
    size: u64,
    etag: &str,
    local_budget: Option<u64>,
) -> PlatformResult<ReadPlan> {
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
    let range = if let Some(budget) = local_budget {
        tgdrive_contract::validation::resolve_local_range(range, size, budget)?
    } else {
        resolve_single_range(range, size)?
    };
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
    session: Arc<CallerSession>,
    expiry: Option<i64>,
    additional_cancel: Option<CancellationToken>,
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
        if self
            .additional_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "本机地址已撤销",
            ));
        }
        if self.expiry.is_some_and(|expiry| expiry <= at) {
            return Err(PlatformError::new(
                error_code::TICKET_EXPIRED,
                "读取票据已过期",
            ));
        }
        if self.session.cancelled.is_cancelled()
            || self.session.expires_at.load(Ordering::Acquire) <= at
            || (self.session.is_page()
                && self.session.owner_epoch != self.platform.host.auth.device_epoch())
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
    let extra = guard.additional_cancel.clone().unwrap_or_default();
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
            _=extra.cancelled()=>return Err(PlatformError::new(error_code::TICKET_EXPIRED,"本机地址已撤销")),
            _=shutdown.cancelled()=>return Err(PlatformError::new(error_code::HOST_MAINTENANCE,"宿主维护中")),
            _=revocations.changed(),if guard.session.is_page()=>return Err(PlatformError::new(error_code::SESSION_EXPIRED,"主人登录已撤销")),
            _=changes.changed()=>{guard.check().await?;},
            _=tokio::time::sleep(deadline)=>{guard.check().await?;},
            _=tokio::time::sleep_until(idle)=>return Err(PlatformError::new(error_code::REQUEST_TIMEOUT,"字节读取空闲超时")),
            result=reader.read(buffer)=>return result.map_err(|_|upstream()),
        }
    }
}

struct Observation {
    platform: Arc<AppPlatform>,
    app: String,
    request_id: String,
    correlation: String,
    file: ContentRef,
    range: ByteWindow,
    lease: Option<String>,
    started: bool,
    bytes: u64,
    outcome: tgdrive_contract::events::StreamOutcome,
    error: Option<String>,
}
impl Observation {
    fn new(
        platform: Arc<AppPlatform>,
        app: &str,
        request_id: String,
        correlation: String,
        file: ContentRef,
        range: ByteWindow,
        lease: Option<String>,
    ) -> Self {
        Self {
            platform,
            app: app.into(),
            request_id,
            correlation,
            file,
            range,
            lease,
            started: false,
            bytes: 0,
            outcome: tgdrive_contract::events::StreamOutcome::Cancelled,
            error: None,
        }
    }
    fn emitted(&mut self, count: usize) {
        if !self.started && count > 0 {
            self.started = true;
            let _ = self.platform.events.emit(
                &self.app,
                tgdrive_contract::events::event_name::STREAM_STARTED,
                &tgdrive_contract::events::StreamStartedEvent {
                    request_id: self.request_id.clone(),
                    correlation_id: self.correlation.clone(),
                    file: self.file.clone(),
                    range: self.range.clone(),
                    lease_id: self.lease.clone(),
                },
            );
        }
        self.bytes += count as u64;
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if self.started {
            let _ = self.platform.events.emit(
                &self.app,
                tgdrive_contract::events::event_name::STREAM_FINISHED,
                &tgdrive_contract::events::StreamFinishedEvent {
                    request_id: self.request_id.clone(),
                    correlation_id: self.correlation.clone(),
                    file: self.file.clone(),
                    bytes_sent: ByteCount(self.bytes),
                    outcome: self.outcome,
                    error_code: self.error.clone(),
                },
            );
        }
    }
}

struct StreamState {
    observation: Option<Observation>,
    reader: crate::telegram::ByteReader,
    guard: ReadGuard,
    _permit: OwnedSemaphorePermit,
    _activity: Option<super::backend::Activity>,
    remaining: u64,
    first: Option<Bytes>,
}
async fn stream_body(
    reader: crate::telegram::ByteReader,
    guard: ReadGuard,
    permit: OwnedSemaphorePermit,
    length: u64,
) -> PlatformResult<Body> {
    stream_body_observed(reader, guard, permit, length, None).await
}
async fn stream_body_observed(
    mut reader: crate::telegram::ByteReader,
    mut guard: ReadGuard,
    permit: OwnedSemaphorePermit,
    length: u64,
    observation: Option<Observation>,
) -> PlatformResult<Body> {
    if length == 0 {
        return Ok(Body::empty());
    }
    guard.check().await?;
    let activity = (!guard.session.is_page())
        .then(|| guard.platform.backends.run(&guard.session.app_id))
        .flatten()
        .filter(|run| run.caller.id == guard.session.id)
        .map(|run| run.activity());
    // 本机窗口只在请求开始裁剪，后续并发消耗只能中断流；不能改成新的配额 JSON。
    let first = if guard.additional_cancel.is_some() {
        None
    } else {
        let mut buffer = vec![0u8; length.min(64 * 1024) as usize];
        let read = read_guarded(&mut reader, &mut guard, &mut buffer).await?;
        if read == 0 {
            return Err(upstream());
        }
        buffer.truncate(read);
        guard.check().await?;
        Some(Bytes::from(buffer))
    };
    let stream = futures_util::stream::try_unfold(
        StreamState {
            observation,
            reader,
            guard,
            _permit: permit,
            _activity: activity,
            remaining: length,
            first,
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
                let read =
                    match read_guarded(&mut state.reader, &mut state.guard, &mut buffer).await {
                        Ok(read) => read,
                        Err(error) => {
                            if let Some(observation) = &mut state.observation {
                                observation.error = Some(error.code.clone());
                                if error.code == error_code::UPSTREAM_UNAVAILABLE
                                    || error.code == error_code::REQUEST_TIMEOUT
                                {
                                    observation.outcome =
                                        tgdrive_contract::events::StreamOutcome::Failed;
                                }
                            }
                            return Err(std::io::Error::other("读取已经撤销或上游中断"));
                        }
                    };
                if read == 0 {
                    if let Some(observation) = &mut state.observation {
                        observation.outcome = tgdrive_contract::events::StreamOutcome::Failed;
                        observation.error = Some(error_code::UPSTREAM_UNAVAILABLE.into());
                    }
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
            if let Some(observation) = &mut state.observation {
                observation.emitted(bytes.len());
            }
            state.remaining -= bytes.len() as u64;
            if state.remaining == 0 {
                if let Some(observation) = &mut state.observation {
                    observation.outcome = tgdrive_contract::events::StreamOutcome::Completed;
                }
            }
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
