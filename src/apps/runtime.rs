//! 页面会话、撤销信号、令牌桶、并发槽与第二阶段统一接入点。

use super::cursor::{CursorCodec, CursorData};
use super::store::Installation;
use super::{PlatformError, PlatformResult, now, opaque};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
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

pub(crate) struct CallerSession {
    pub kind: RuntimeCaller,
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
impl CallerSession {
    pub fn backend(app: &Installation, run_id: String) -> Arc<Self> {
        Arc::new(Self {
            kind: RuntimeCaller::Backend,
            id: run_id,
            app_id: app.id.clone(),
            generation: app.generation.clone(),
            owner_epoch: String::new(),
            expires_at: AtomicI64::new(MAX_TIMESTAMP_MILLIS as i64),
            cancelled: CancellationToken::new(),
            control: Arc::new(Semaphore::new(BACKEND_CONTROL_CONCURRENCY as usize)),
            bytes: Arc::new(Semaphore::new(BACKEND_BYTE_CONCURRENCY as usize)),
            events: Arc::new(Semaphore::new(MAX_EVENT_CONNECTIONS_PER_CALLER as usize)),
            bucket: Mutex::new(TokenBucket {
                tokens: BACKEND_CONTROL_BURST as f64,
                last: Instant::now(),
            }),
        })
    }
    pub fn is_page(&self) -> bool {
        self.kind == RuntimeCaller::Page
    }
    pub fn rate_limit(&self) -> PlatformResult<()> {
        let mut bucket = self.bucket.lock().unwrap();
        let now = Instant::now();
        let rate = if self.is_page() {
            PAGE_CONTROL_RATE
        } else {
            BACKEND_CONTROL_RATE
        };
        let burst = if self.is_page() {
            PAGE_CONTROL_BURST
        } else {
            BACKEND_CONTROL_BURST
        };
        bucket.tokens = (bucket.tokens
            + now.duration_since(bucket.last).as_secs_f64() * rate as f64)
            .min(burst as f64);
        bucket.last = now;
        if bucket.tokens < 1.0 {
            return Err(PlatformError::limited(
                error_code::RATE_LIMITED,
                "页面控制请求过于频繁",
                (1000 / rate + 1) as u32,
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

#[derive(Clone)]
pub(crate) struct TrustedBackendCaller(pub Arc<CallerSession>);

pub(crate) struct Sessions {
    sessions: Mutex<BTreeMap<String, Arc<CallerSession>>>,
}
impl Sessions {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn create(&self, app: &Installation, owner_epoch: String) -> Arc<CallerSession> {
        let session = Arc::new(CallerSession {
            kind: RuntimeCaller::Page,
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
    pub fn token(codec: &CursorCodec, session: &CallerSession) -> PlatformResult<String> {
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
    ) -> PlatformResult<Arc<CallerSession>> {
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
    pub fn owns(&self, codec: &CursorCodec, token: &str) -> bool {
        codec.decode(token).is_ok_and(|data| data.kind == "session")
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
        .filter(|name| !matches!(*name, "kv.device"))
        .filter(|name| !name.starts_with("backend.") || super::backend_system::available())
        .collect()
}

/// 宿主设置里唯一一组应用资源限制：全部应用后端合计共用，0 表示不限制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedLimitValues {
    pub memory_bytes: u64,
    pub cpu_millis: u32,
}
impl Default for SharedLimitValues {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_SHARED_BACKEND_MEMORY_BYTES,
            cpu_millis: DEFAULT_SHARED_BACKEND_CPU_MILLIS as u32,
        }
    }
}
impl SharedLimitValues {
    pub fn validate(self) -> PlatformResult<Self> {
        let memory = self.memory_bytes == 0
            || (MIN_SHARED_BACKEND_MEMORY_BYTES..=MAX_SHARED_BACKEND_MEMORY_BYTES)
                .contains(&self.memory_bytes);
        let cpu = self.cpu_millis == 0
            || (MIN_SHARED_BACKEND_CPU_MILLIS..=MAX_SHARED_BACKEND_CPU_MILLIS)
                .contains(&u64::from(self.cpu_millis));
        if memory && cpu {
            Ok(self)
        } else {
            Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "应用内存上限需为 0 或 64 MiB～1 TiB，CPU 上限需为 0 或 0.1～256 核",
            ))
        }
    }
}

pub(crate) const SHARED_MEMORY_KEY: &str = "apps.shared_memory_bytes";
pub(crate) const SHARED_CPU_KEY: &str = "apps.shared_cpu_millis";

/// 当前生效的总体限制及其在 cgroup 上的实际强制状态。
pub(crate) struct SharedLimits {
    memory_bytes: AtomicU64,
    cpu_millis: AtomicU32,
    memory_enforced: AtomicBool,
    cpu_enforced: AtomicBool,
    /// 串行化保存，使设置表、内存值与 cgroup 的写入顺序一致。
    pub update: tokio::sync::Mutex<()>,
}
impl SharedLimits {
    pub fn new(values: SharedLimitValues) -> Self {
        let limits = Self {
            memory_bytes: AtomicU64::new(0),
            cpu_millis: AtomicU32::new(0),
            memory_enforced: AtomicBool::new(false),
            cpu_enforced: AtomicBool::new(false),
            update: tokio::sync::Mutex::new(()),
        };
        limits.set(values);
        limits
    }
    pub fn values(&self) -> SharedLimitValues {
        SharedLimitValues {
            memory_bytes: self.memory_bytes.load(Ordering::Acquire),
            cpu_millis: self.cpu_millis.load(Ordering::Acquire),
        }
    }
    /// 记录新值并立刻写入根组，运行中的后端无需重启。
    pub fn set(&self, values: SharedLimitValues) {
        self.memory_bytes
            .store(values.memory_bytes, Ordering::Release);
        self.cpu_millis.store(values.cpu_millis, Ordering::Release);
        let (memory, cpu) = super::backend_system::cgroup_root().map_or((false, false), |root| {
            super::backend_system::apply_shared_limits(
                std::path::Path::new(&root),
                values.memory_bytes,
                values.cpu_millis,
            )
        });
        self.memory_enforced.store(memory, Ordering::Release);
        self.cpu_enforced.store(cpu, Ordering::Release);
    }
    pub fn report(&self) -> AppResourceLimits {
        let values = self.values();
        AppResourceLimits {
            memory_bytes: ByteCount(values.memory_bytes),
            cpu_millis: values.cpu_millis,
            memory_enforced: self.memory_enforced.load(Ordering::Acquire),
            cpu_enforced: self.cpu_enforced.load(Ordering::Acquire),
        }
    }
}

/// 内存与 CPU 来自宿主的总体设置；其余各项是宿主对每个应用统一使用的固定值，应用不能声明。
pub(crate) fn limits(
    app: &Installation,
    shared: SharedLimitValues,
) -> PlatformResult<RuntimeLimits> {
    let backend = app.manifest()?.backend.map(|_| BackendResourceLimits {
        memory_bytes: ByteCount(shared.memory_bytes),
        cpu_millis: shared.cpu_millis,
        pids: DEFAULT_BACKEND_PIDS as u16,
        open_files: DEFAULT_BACKEND_OPEN_FILES as u32,
        cache_bytes: ByteCount(DEFAULT_BACKEND_CACHE_BYTES),
        database_bytes: ByteCount(DEFAULT_BACKEND_DATABASE_BYTES),
        enforced: BackendLimitEnforcement {
            memory: false,
            cpu: false,
            pids: false,
            open_files: false,
        },
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
    caller: RuntimeCaller,
    shared: SharedLimitValues,
) -> PlatformResult<RuntimeInfo> {
    let manifest = app.manifest()?;
    let mut actual_limits = limits(app, shared)?;
    if caller == RuntimeCaller::Backend {
        actual_limits.control_concurrency = BACKEND_CONTROL_CONCURRENCY as u16;
        actual_limits.byte_stream_concurrency = BACKEND_BYTE_CONCURRENCY as u16;
        actual_limits.control_requests_per_second = BACKEND_CONTROL_RATE as u16;
        actual_limits.control_burst = BACKEND_CONTROL_BURST as u16;
    }
    Ok(RuntimeInfo {
        contract: CONTRACT_VERSION,
        app: app.identity()?,
        caller,
        capabilities: capabilities()
            .into_iter()
            .filter(|name| {
                if name.starts_with("backend.") && manifest.backend.is_none() {
                    return false;
                }
                if *name == "backend.external"
                    && !manifest
                        .backend
                        .as_ref()
                        .is_some_and(|backend| backend.external_service)
                {
                    return false;
                }
                if caller == RuntimeCaller::Page {
                    *name != "backend.keepalive" && *name != "files.local_url"
                } else {
                    !name.starts_with("ui.") && *name != "session.refresh"
                }
            })
            .map(String::from)
            .collect(),
        files: manifest.files,
        scope,
        limits: actual_limits,
        backend: app.backend()?,
        external_service: None,
    })
}
