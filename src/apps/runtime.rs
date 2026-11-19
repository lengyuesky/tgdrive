//! 页面会话、撤销信号、令牌桶、并发槽与第二阶段统一接入点。

use super::cursor::{CursorCodec, CursorData};
use super::store::Installation;
use super::{PlatformError, PlatformResult, now, opaque};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tgdrive_contract::http::*;
use tgdrive_contract::runtime::*;
use tgdrive_contract::{ByteCount, CONTRACT_VERSION, error_code};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

struct TokenBucket {
    tokens: f64,
    last: Instant,
}

pub(crate) struct PageSession {
    pub id: String,
    pub app_id: String,
    pub generation: String,
    pub owner_epoch: String,
    pub expires_at: AtomicI64,
    pub cancelled: CancellationToken,
    pub control: Arc<Semaphore>,
    pub bytes: Arc<Semaphore>,
    pub events: Arc<Semaphore>,
    bucket: Mutex<TokenBucket>,
}
impl PageSession {
    pub fn rate_limit(&self) -> PlatformResult<()> {
        let mut bucket = self.bucket.lock().unwrap();
        let now = Instant::now();
        bucket.tokens = (bucket.tokens
            + now.duration_since(bucket.last).as_secs_f64() * PAGE_CONTROL_RATE as f64)
            .min(PAGE_CONTROL_BURST as f64);
        bucket.last = now;
        if bucket.tokens < 1.0 {
            return Err(PlatformError::limited(
                error_code::RATE_LIMITED,
                "页面控制请求过于频繁",
                (1000 / PAGE_CONTROL_RATE + 1) as u32,
            ));
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
    pub fn control_slot(&self) -> PlatformResult<OwnedSemaphorePermit> {
        self.control.clone().try_acquire_owned().map_err(|_| {
            PlatformError::limited(error_code::RATE_LIMITED, "页面控制请求并发已满", 50)
        })
    }
    pub fn byte_slot(&self) -> PlatformResult<OwnedSemaphorePermit> {
        self.bytes.clone().try_acquire_owned().map_err(|_| {
            PlatformError::limited(error_code::RATE_LIMITED, "页面字节读取并发已满", 50)
        })
    }
}

pub(crate) struct Sessions {
    sessions: Mutex<BTreeMap<String, Arc<PageSession>>>,
}
impl Sessions {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn create(&self, app: &Installation, owner_epoch: String) -> Arc<PageSession> {
        let session = Arc::new(PageSession {
            id: opaque(),
            app_id: app.id.clone(),
            generation: app.generation.clone(),
            owner_epoch,
            expires_at: AtomicI64::new(now().0 + PAGE_SESSION_SECONDS as i64 * 1000),
            cancelled: CancellationToken::new(),
            control: Arc::new(Semaphore::new(PAGE_CONTROL_CONCURRENCY as usize)),
            bytes: Arc::new(Semaphore::new(PAGE_BYTE_CONCURRENCY as usize)),
            events: Arc::new(Semaphore::new(MAX_EVENT_CONNECTIONS_PER_CALLER as usize)),
            bucket: Mutex::new(TokenBucket {
                tokens: PAGE_CONTROL_BURST as f64,
                last: Instant::now(),
            }),
        });
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| {
            let valid = !s.cancelled.is_cancelled()
                && s.expires_at.load(Ordering::Acquire) > now().0
                && s.owner_epoch == session.owner_epoch;
            if !valid {
                s.cancelled.cancel();
            }
            valid
        });
        sessions.insert(session.id.clone(), session.clone());
        session
    }
    pub fn token(codec: &CursorCodec, session: &PageSession) -> PlatformResult<String> {
        codec.encode(&CursorData {
            kind: "session".into(),
            app: session.app_id.clone(),
            generation: session.generation.clone(),
            scope: None,
            expires_at: None,
            binding: session.id.clone(),
            position: serde_json::Value::Null,
            scan: None,
            epoch: None,
            high_water: None,
        })
    }
    pub fn authenticate(
        &self,
        codec: &CursorCodec,
        token: &str,
        owner_epoch: &str,
    ) -> PlatformResult<Arc<PageSession>> {
        let data = codec.decode(token).map_err(|error| {
            if error.code == error_code::CURSOR_EXPIRED {
                PlatformError::new(
                    error_code::SESSION_EXPIRED,
                    "页面会话已过期，请从应用中心重新打开",
                )
            } else {
                PlatformError::new(error_code::UNAUTHORIZED, "应用页面会话无效")
            }
        })?;
        if data.kind != "session" {
            return Err(PlatformError::new(
                error_code::UNAUTHORIZED,
                "应用页面会话无效",
            ));
        }
        let session = self
            .sessions
            .lock()
            .unwrap()
            .get(&data.binding)
            .cloned()
            .ok_or_else(|| {
                PlatformError::new(error_code::SESSION_EXPIRED, "页面会话已撤销，请重新打开")
            })?;
        if session.cancelled.is_cancelled()
            || session.expires_at.load(Ordering::Acquire) <= now().0
            || session.owner_epoch != owner_epoch
        {
            return Err(PlatformError::new(
                error_code::SESSION_EXPIRED,
                "页面会话已撤销，请重新打开",
            ));
        }
        if data.app != session.app_id || data.generation != session.generation {
            return Err(PlatformError::new(
                error_code::UNAUTHORIZED,
                "应用页面会话无效",
            ));
        }
        Ok(session)
    }
    pub fn revoke(&self, id: &str) {
        if let Some(session) = self.sessions.lock().unwrap().remove(id) {
            session.cancelled.cancel();
        }
    }
    pub fn revoke_other_generations(&self, id: &str, current: &str) {
        self.sessions.lock().unwrap().retain(|_, session| {
            if session.app_id == id && session.generation != current {
                session.cancelled.cancel();
                false
            } else {
                true
            }
        });
    }
    pub fn revoke_app(&self, id: &str) {
        self.sessions.lock().unwrap().retain(|_, session| {
            if session.app_id == id {
                session.cancelled.cancel();
                false
            } else {
                true
            }
        });
    }
    pub fn revoke_all(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        for session in sessions.values() {
            session.cancelled.cancel();
        }
        sessions.clear();
    }
}

/// 第一阶段不启动进程；第二阶段从此处接上后端能力和真实强制状态。
pub(crate) fn backend_unavailable() -> PlatformError {
    PlatformError::new(
        error_code::CAPABILITY_UNAVAILABLE,
        "当前宿主尚未提供应用后端能力",
    )
}

pub(crate) fn capabilities() -> Vec<&'static str> {
    CAPABILITIES
        .iter()
        .copied()
        .filter(|name| {
            !matches!(
                *name,
                "files.local_url"
                    | "files.mutate"
                    | "files.upload"
                    | "kv.device"
                    | "backend.proxy"
                    | "backend.keepalive"
                    | "backend.external"
            )
        })
        .collect()
}

pub(crate) fn limits(app: &Installation) -> PlatformResult<RuntimeLimits> {
    let backend = app.manifest()?.backend.map(|backend| {
        let requested = backend.resources;
        BackendResourceLimits {
            memory_bytes: requested
                .memory_bytes
                .unwrap_or(ByteCount(DEFAULT_BACKEND_MEMORY_BYTES)),
            cpu_millis: requested
                .cpu_millis
                .unwrap_or(DEFAULT_BACKEND_CPU_MILLIS as u32),
            pids: requested.pids.unwrap_or(DEFAULT_BACKEND_PIDS as u16),
            open_files: DEFAULT_BACKEND_OPEN_FILES as u32,
            cache_bytes: requested
                .cache_bytes
                .unwrap_or(ByteCount(DEFAULT_BACKEND_CACHE_BYTES)),
            database_bytes: requested
                .database_bytes
                .unwrap_or(ByteCount(DEFAULT_BACKEND_DATABASE_BYTES)),
            enforced: BackendLimitEnforcement {
                memory: false,
                cpu: false,
                pids: false,
                open_files: false,
            },
        }
    });
    let cache_byte_limit = backend
        .as_ref()
        .map_or(ByteCount(DEFAULT_PAGE_CACHE_BYTES), |limits| {
            limits.cache_bytes
        });
    Ok(RuntimeLimits {
        json_request_bytes: ByteCount(MAX_JSON_REQUEST_BYTES),
        json_response_bytes: ByteCount(MAX_JSON_RESPONSE_BYTES),
        json_page_bytes: ByteCount(MAX_JSON_PAGE_BYTES),
        file_page_limit: MAX_PAGE_LIMIT as u16,
        query_root_limit: MAX_QUERY_ROOTS as u16,
        extension_limit: MAX_QUERY_EXTENSIONS as u16,
        mutation_operation_limit: MAX_MUTATION_OPERATIONS as u16,
        mutation_assertion_limit: MAX_MUTATION_ASSERTIONS as u16,
        random_limit: MAX_RANDOM_LIMIT as u16,
        random_exclusion_limit: MAX_RANDOM_EXCLUSIONS as u16,
        kv_key_bytes: MAX_KV_KEY_BYTES as u16,
        kv_value_bytes: ByteCount(MAX_KV_VALUE_BYTES),
        kv_entry_limit: DEFAULT_KV_ENTRIES as u32,
        kv_byte_limit: ByteCount(DEFAULT_KV_BYTES),
        cache_object_bytes: ByteCount(MAX_CACHE_OBJECT_BYTES),
        cache_metadata_bytes: ByteCount(MAX_CACHE_METADATA_BYTES),
        cache_lookup_limit: MAX_CACHE_LOOKUP_KEYS as u16,
        cache_byte_limit,
        control_concurrency: PAGE_CONTROL_CONCURRENCY as u16,
        byte_stream_concurrency: PAGE_BYTE_CONCURRENCY as u16,
        control_requests_per_second: PAGE_CONTROL_RATE as u16,
        control_burst: PAGE_CONTROL_BURST as u16,
        read_lease_limit: MAX_READ_LEASES as u16,
        media_ticket_limit: MAX_MEDIA_TICKETS as u32,
        upload_limit: MAX_UPLOADS as u16,
        backend,
    })
}

pub(crate) fn info(
    app: &Installation,
    scope: tgdrive_contract::files::ScopeInfo,
) -> PlatformResult<RuntimeInfo> {
    let manifest = app.manifest()?;
    Ok(RuntimeInfo {
        contract: CONTRACT_VERSION,
        app: app.identity()?,
        caller: RuntimeCaller::Page,
        capabilities: capabilities().into_iter().map(String::from).collect(),
        files: manifest.files,
        scope,
        limits: limits(app)?,
        backend: app.backend()?,
    })
}
