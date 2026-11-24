//! 后端生命周期：合并启动、独享监听身份、就绪协商与进程组回收。
use super::backend_system::{self as system, Identity, LaunchConfig};
use super::runtime::CallerSession;
use super::store;
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use std::time::Duration;
use tgdrive_contract::{CONTRACT_VERSION, error_code, http::*, manifest::RunMode, runtime::*};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) struct Manager {
    slots: StdMutex<BTreeMap<String, Arc<Slot>>>,
}
struct Slot {
    gate: Mutex<()>,
    state: RwLock<Option<BackendStatus>>,
    run: RwLock<Option<Arc<Run>>>,
    retiring_runs: StdMutex<Vec<Weak<Run>>>,
    // 崩溃记录只驻留内存，用单调时钟避免整秒截断和系统时钟跳变。
    failures: StdMutex<Vec<Instant>>,
    crash_blocked: AtomicBool,
    pending_starts: Arc<tokio::sync::Semaphore>,
}
pub(crate) struct Run {
    pub caller: Arc<CallerSession>,
    pub identity: Identity,
    pub pid: u32,
    process: Option<std::os::fd::OwnedFd>,
    pub socket: String,
    pub data_dir: String,
    cgroup: Option<String>,
    pub negotiated: RwLock<tgdrive_contract::ContractVersion>,
    pub limits: RwLock<BackendResourceLimits>,
    pub active: AtomicUsize,
    pub proxy_slots: Arc<tokio::sync::Semaphore>,
    pub last_activity: AtomicI64,
    pub keep_until: AtomicI64,
    pub finished: CancellationToken,
    ready: AtomicBool,
}
impl Run {
    /// 用当前总体限制刷新本轮对外报告的限额；只有进程确实在共同的 cgroup 里才算被强制。
    fn sync_shared_limits(&self, shared: &AppResourceLimits) {
        let joined = self
            .cgroup
            .as_deref()
            .is_some_and(|cgroup| system::in_cgroup(cgroup, self.pid));
        let mut limits = self.limits.write().unwrap();
        limits.memory_bytes = shared.memory_bytes;
        limits.cpu_millis = shared.cpu_millis;
        limits.enforced.memory = joined && shared.memory_enforced;
        limits.enforced.cpu = joined && shared.cpu_enforced;
    }
    pub fn activity(self: &Arc<Self>) -> Activity {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.touch();
        Activity(self.clone())
    }
    pub fn touch(&self) {
        self.last_activity.store(now().0, Ordering::Release);
    }
}
pub(crate) struct Activity(Arc<Run>);
impl Drop for Activity {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.touch();
    }
}
impl Manager {
    pub fn new() -> Self {
        Self {
            slots: StdMutex::new(BTreeMap::new()),
        }
    }
    fn slot(&self, id: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap()
            .entry(id.into())
            .or_insert_with(|| {
                Arc::new(Slot {
                    gate: Mutex::new(()),
                    state: RwLock::new(None),
                    run: RwLock::new(None),
                    retiring_runs: StdMutex::new(Vec::new()),
                    failures: StdMutex::new(Vec::new()),
                    crash_blocked: AtomicBool::new(false),
                    pending_starts: Arc::new(tokio::sync::Semaphore::new(
                        BACKEND_CONTROL_CONCURRENCY as usize,
                    )),
                })
            })
            .clone()
    }
    pub fn run(&self, id: &str) -> Option<Arc<Run>> {
        self.slot(id)
            .run
            .read()
            .unwrap()
            .clone()
            .filter(|run| !run.caller.cancelled.is_cancelled())
    }
    pub fn status(&self, id: &str, mode: RunMode) -> BackendStatus {
        self.slot(id)
            .state
            .read()
            .unwrap()
            .clone()
            .unwrap_or(BackendStatus {
                state: BackendState::Stopped,
                run_mode: mode,
                run_id: None,
                error_code: None,
            })
    }
    pub async fn ensure(&self, platform: &Arc<AppPlatform>, id: &str) -> PlatformResult<Arc<Run>> {
        let slot = self.slot(id);
        let _pending = slot
            .pending_starts
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable())?;
        let _gate = slot.gate.lock().await;
        let mut connection = platform.host.vfs.db.pool.acquire().await?;
        let app = store::installation(&mut connection, id).await?;
        app.check(&app.generation)?;
        let manifest = app.manifest()?;
        let backend = manifest
            .backend
            .ok_or_else(|| PlatformError::new(error_code::CALLER_NOT_ALLOWED, "应用没有后端"))?;
        drop(connection);
        if platform.host.restore.maintenance() || platform.host.restore.shutdown.is_cancelled() {
            return Err(PlatformError::new(
                error_code::HOST_MAINTENANCE,
                "宿主正在维护或停止",
            ));
        }
        if slot.crash_blocked.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let previous = slot.run.read().unwrap().clone();
        if let Some(run) = previous {
            if !run.caller.cancelled.is_cancelled()
                && run.caller.generation == app.generation
                && self.status(id, backend.run_mode).state == BackendState::Ready
            {
                return Ok(run);
            }
            slot.run.write().unwrap().take();
            terminate(&run).await;
        }
        if !system::available() {
            return Err(PlatformError::new(
                error_code::CAPABILITY_UNAVAILABLE,
                "当前宿主无法提供后端硬隔离",
            ));
        }
        let retry_at = {
            let mut failures = slot.failures.lock().unwrap();
            let current = Instant::now();
            failures.retain(|at| {
                current.duration_since(*at) < Duration::from_secs(BACKEND_RESTART_WINDOW_SECONDS)
            });
            if failures.len() as u64 > MAX_BACKEND_RESTARTS {
                slot.crash_blocked.store(true, Ordering::Release);
                return Err(PlatformError::new(
                    error_code::BACKEND_UNAVAILABLE,
                    "后端已达到自动重启上限",
                ));
            }
            failures.last().map(|last| {
                *last + Duration::from_secs(1u64 << failures.len().saturating_sub(1).min(4))
            })
        };
        if let Some(retry_at) = retry_at {
            tokio::time::sleep_until(retry_at).await;
        }
        *slot.state.write().unwrap() = Some(BackendStatus {
            state: BackendState::Starting,
            run_mode: backend.run_mode,
            run_id: None,
            error_code: None,
        });
        let failure_count = slot.failures.lock().unwrap().len();
        let result = self.start(platform, &app, backend.run_mode).await;
        match result {
            Ok(run) => {
                *slot.run.write().unwrap() = Some(run.clone());
                *slot.state.write().unwrap() = Some(BackendStatus {
                    state: BackendState::Ready,
                    run_mode: backend.run_mode,
                    run_id: Some(run.caller.id.clone()),
                    error_code: None,
                });
                let _ = platform.events.emit(
                    id,
                    tgdrive_contract::events::event_name::BACKEND_CHANGED,
                    &tgdrive_contract::events::BackendChangedEvent {
                        state: BackendState::Ready,
                        run_id: Some(run.caller.id.clone()),
                    },
                );
                let _ = platform.events.emit(
                    id,
                    tgdrive_contract::events::event_name::EXTERNAL_SERVICE_CHANGED,
                    &tgdrive_contract::EmptyResponse {},
                );
                Ok(run)
            }
            Err(error) => {
                {
                    let mut failures = slot.failures.lock().unwrap();
                    if failures.len() == failure_count {
                        failures.push(Instant::now());
                    }
                    if failures.len() as u64 > MAX_BACKEND_RESTARTS {
                        slot.crash_blocked.store(true, Ordering::Release);
                    }
                }
                *slot.state.write().unwrap() = Some(BackendStatus {
                    state: BackendState::Failed,
                    run_mode: backend.run_mode,
                    run_id: None,
                    error_code: Some(error_code::BACKEND_UNAVAILABLE.into()),
                });
                Err(error)
            }
        }
    }
    async fn start(
        &self,
        platform: &Arc<AppPlatform>,
        app: &store::Installation,
        mode: RunMode,
    ) -> PlatformResult<Arc<Run>> {
        // 停止 future 被取消后，slot.run 可能已被取走，但旧监护任务仍在回收。
        // 必须等其 finished，不能让旧轮次的 UID 清理误杀新轮次。
        let retiring = {
            let slot = self.slot(&app.id);
            let mut runs = slot.retiring_runs.lock().unwrap();
            runs.retain(|run| {
                run.upgrade()
                    .is_some_and(|run| !run.finished.is_cancelled())
            });
            runs.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        for run in retiring {
            terminate(&run).await;
            if !run.finished.is_cancelled() {
                return Err(unavailable());
            }
        }
        let identity = system::identity(&platform.host.vfs.db, &app.id).await?;
        // 宿主异常退出后可能仍有旧 UID 子进程，首次启动也要先回收，而不只依赖内存 Run。
        system::signal_identity(identity, libc::SIGKILL);
        system::harden_host(&platform.host.rt.env.db_path)?;
        system::reap_identity(identity).await?;
        let root = super::safe_fs::Directory::open(&platform.root, true)?;
        root.permissions(
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            0o711,
        )?;
        let packages = super::safe_fs::Directory::open(&platform.package_root, false)?;
        packages.permissions(
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            0o711,
        )?;
        let package = packages.child(&app.package_sha256, false)?;
        package.permissions(unsafe { libc::geteuid() }, identity.gid, 0o550)?;
        package.child("backend", false)?.permissions(
            unsafe { libc::geteuid() },
            identity.gid,
            0o550,
        )?;
        let data = system::prepare_layout(&platform.data_root(), &app.id, identity)?;
        let shared = platform.shared_limits().await?.values();
        let limits = super::runtime::limits(app, shared)?
            .backend
            .ok_or_else(|| PlatformError::new(error_code::CALLER_NOT_ALLOWED, "应用没有后端"))?;
        super::safe_fs::remove_tree_owner(&data.join("run"), "backend", Some(identity.uid))?;
        let run_dir = super::safe_fs::Directory::open(&data.join("run"), false)?;
        run_dir.remove("host.sock")?;
        run_dir
            .child("backend", true)?
            .permissions(identity.uid, identity.gid, 0o700)?;
        let caller = CallerSession::backend(app, opaque());
        let context = BackendStartContext {
            contract: CONTRACT_VERSION,
            app: app.identity()?,
            run_id: caller.id.clone(),
            host_socket: data.join("run/host.sock").to_string_lossy().into_owned(),
            listen_socket: data
                .join("run/backend/backend.sock")
                .to_string_lossy()
                .into_owned(),
            data_dir: data.to_string_lossy().into_owned(),
            database_path: data
                .join("state/app.sqlite3")
                .to_string_lossy()
                .into_owned(),
            cache_dir: data.join("cache").to_string_lossy().into_owned(),
            temp_dir: data.join("tmp").to_string_lossy().into_owned(),
            ffmpeg_path: "/usr/local/bin/ffmpeg".into(),
            ffprobe_path: "/usr/local/bin/ffprobe".into(),
            limits,
        };
        let config = LaunchConfig {
            context: context.clone(),
            executable: platform
                .package_root
                .join(&app.package_sha256)
                .join(tgdrive_contract::manifest::BACKEND_ENTRY)
                .to_string_lossy()
                .into_owned(),
            identity,
            cgroup: system::cgroup_path(identity.uid, &caller.id),
        };
        let launch = system::write_launch(&data, &config)?;
        let listener = UnixListener::bind(&context.host_socket)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&context.host_socket, std::fs::Permissions::from_mode(0o660))?;
        let socket = std::ffi::CString::new(context.host_socket.as_bytes())
            .map_err(|_| PlatformError::new(error_code::INVALID_PATH, "套接字路径无效"))?;
        if unsafe { libc::chown(socket.as_ptr(), libc::geteuid(), identity.gid) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if !system::real_isolation() {
            run_dir.write_atomic("context.json", &serde_json::to_vec(&context)?)?;
        }
        let mut command = tokio::process::Command::new(if system::real_isolation() {
            system::helper_executable()?
        } else {
            std::path::PathBuf::from(&config.executable)
        });
        if system::real_isolation() {
            command.arg("internal-app-run").arg(&launch);
        } else {
            configure_test_command(&mut command, &context);
        }
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("HOME", data.join("state"))
            .env("TMPDIR", &context.temp_dir)
            .current_dir(&data)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        configure_test_environment(&mut command, &context);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| PlatformError::new(error_code::BACKEND_UNAVAILABLE, "后端进程未创建"))?;
        let run = Arc::new(Run {
            caller: caller.clone(),
            identity,
            pid,
            process: system::process_descriptor(pid)?,
            socket: context.listen_socket.clone(),
            data_dir: context.data_dir.clone(),
            cgroup: config.cgroup.clone(),
            negotiated: RwLock::new(CONTRACT_VERSION),
            limits: RwLock::new(context.limits.clone()),
            active: AtomicUsize::new(0),
            proxy_slots: Arc::new(tokio::sync::Semaphore::new(
                BACKEND_CONTROL_CONCURRENCY as usize,
            )),
            last_activity: AtomicI64::new(now().0),
            keep_until: AtomicI64::new(0),
            finished: CancellationToken::new(),
            ready: AtomicBool::new(false),
        });
        let mut attempt = StartAttempt(Some(run.clone()));
        self.slot(&app.id)
            .retiring_runs
            .lock()
            .unwrap()
            .push(Arc::downgrade(&run));
        *self.slot(&app.id).run.write().unwrap() = Some(run.clone());
        serve_host(platform.clone(), run.clone(), listener);
        if let Some(stdout) = child.stdout.take() {
            capture_log(platform, &app.id, "stdout", stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            capture_log(platform, &app.id, "stderr", stderr);
        }
        let weak = Arc::downgrade(platform);
        let monitored = run.clone();
        let id = app.id.clone();
        tokio::spawn(async move {
            let result = child.wait().await;
            let expected = monitored.caller.cancelled.is_cancelled();
            if let Some(platform) = weak.upgrade() {
                platform.backends.exited(&id, &monitored, mode, expected);
            }
            monitored.caller.cancelled.cancel();
            system::kill_group(monitored.pid, monitored.identity, libc::SIGKILL);
            let cleanup_ok = system::reap_identity(monitored.identity).await.is_ok();
            if !cleanup_ok {
                tracing::warn!("应用子进程未完全回收，阻止自动重启");
                if let Some(platform) = weak.upgrade() {
                    let slot = platform.backends.slot(&id);
                    slot.crash_blocked.store(true, Ordering::Release);
                    *slot.state.write().unwrap() = Some(BackendStatus {
                        state: BackendState::Failed,
                        run_mode: mode,
                        run_id: None,
                        error_code: Some(error_code::BACKEND_UNAVAILABLE.into()),
                    });
                }
            }
            if let Some(cgroup) = &monitored.cgroup {
                let _ = std::fs::write(std::path::Path::new(cgroup).join("cgroup.kill"), "1");
                let _ = std::fs::remove_dir(cgroup);
            }
            if let Some(platform) = weak.upgrade() {
                let slot = platform.backends.slot(&id);
                let mut current = slot.run.write().unwrap();
                if current
                    .as_ref()
                    .is_some_and(|run| run.caller.id == monitored.caller.id)
                {
                    current.take();
                }
            }
            monitored.finished.cancel();
            if let Some(platform) = weak.upgrade() {
                let _ = platform.events.emit(
                    &id,
                    tgdrive_contract::events::event_name::BACKEND_CHANGED,
                    &tgdrive_contract::events::BackendChangedEvent {
                        state: if expected && cleanup_ok {
                            BackendState::Stopped
                        } else {
                            BackendState::Failed
                        },
                        run_id: None,
                    },
                );
                let _ = platform.events.emit(
                    &id,
                    tgdrive_contract::events::event_name::EXTERNAL_SERVICE_CHANGED,
                    &tgdrive_contract::EmptyResponse {},
                );
                let _ = result;
            }
        });
        if let Err(error) = ready(platform, run.clone(), app).await {
            terminate(&run).await;
            return Err(error);
        }
        if run.caller.cancelled.is_cancelled() {
            return Err(unavailable());
        }
        run.ready.store(true, Ordering::Release);
        attempt.0.take();
        Ok(run)
    }
    pub async fn stop_all(&self) -> PlatformResult<()> {
        let ids = self
            .slots
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(ids.iter().map(|id| self.stop(id))).await;
        for result in results {
            result?;
        }
        Ok(())
    }
    pub async fn stop(&self, id: &str) -> PlatformResult<()> {
        let slot = self.slot(id);
        let _gate = slot.gate.lock().await;
        let run = slot.run.write().unwrap().take();
        if let Some(run) = run {
            terminate(&run).await;
            system::reap_identity(run.identity).await?;
        }
        if let Some(state) = slot.state.write().unwrap().as_mut() {
            state.state = BackendState::Stopped;
            state.run_id = None;
            state.error_code = None;
        }
        Ok(())
    }
    pub fn reset_failures(&self, id: &str) {
        let slot = self.slot(id);
        slot.failures.lock().unwrap().clear();
        slot.crash_blocked.store(false, Ordering::Release);
    }
    pub async fn restart(
        &self,
        platform: &Arc<AppPlatform>,
        id: &str,
    ) -> PlatformResult<BackendStatus> {
        self.stop(id).await?;
        let app = platform.installed(id).await?;
        if app.manifest.backend.is_none() {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "应用没有后端",
            ));
        }
        self.reset_failures(id);
        let run = self.ensure(platform, id).await?;
        let mode = platform
            .installed(id)
            .await?
            .manifest
            .backend
            .unwrap()
            .run_mode;
        Ok(self.status(&run.caller.app_id, mode))
    }
    fn exited(&self, id: &str, run: &Arc<Run>, mode: RunMode, expected: bool) {
        let slot = self.slot(id);
        let current = slot
            .run
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|current| current.caller.id == run.caller.id);
        if !current {
            return;
        }
        if !expected && run.ready.load(Ordering::Acquire) {
            let mut failures = slot.failures.lock().unwrap();
            let current = Instant::now();
            failures.retain(|at| {
                current.duration_since(*at) < Duration::from_secs(BACKEND_RESTART_WINDOW_SECONDS)
            });
            failures.push(current);
            if failures.len() as u64 > MAX_BACKEND_RESTARTS {
                slot.crash_blocked.store(true, Ordering::Release);
            }
        }
        *slot.state.write().unwrap() = Some(BackendStatus {
            state: if expected {
                BackendState::Stopped
            } else {
                BackendState::Failed
            },
            run_mode: mode,
            run_id: None,
            error_code: if expected {
                None
            } else {
                Some(error_code::BACKEND_UNAVAILABLE.into())
            },
        });
    }
}

// 就绪等待被客户端取消时也要回收整个启动轮次，不能遗留 starting 进程。
struct StartAttempt(Option<Arc<Run>>);
impl Drop for StartAttempt {
    fn drop(&mut self) {
        if let Some(run) = &self.0 {
            run.caller.cancelled.cancel();
            signal_parent(run, libc::SIGKILL);
        }
    }
}
struct StoppingGroup<'a>(&'a Run);
impl Drop for StoppingGroup<'_> {
    fn drop(&mut self) {
        signal_parent(self.0, libc::SIGKILL);
    }
}
fn signal_parent(run: &Run, signal: i32) {
    #[cfg(target_os = "linux")]
    if let Some(process) = &run.process {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                process.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        return;
    }
    // 测试替身与非 Linux 路径保留进程组回收；真实 UID 子孙由监护任务收养并 wait。
    #[cfg(not(target_os = "linux"))]
    let _ = &run.process;
    system::kill_group(run.pid, run.identity, signal);
}
async fn terminate(run: &Run) {
    // 停止 future 被宿主退出或 HTTP 取消丢弃时也必须回收整个进程组。
    let _group = StoppingGroup(run);
    run.caller.cancelled.cancel();
    system::kill_group(run.pid, run.identity, libc::SIGTERM);
    let _ = tokio::time::timeout(
        Duration::from_secs(BACKEND_STOP_SECONDS),
        run.finished.cancelled(),
    )
    .await;
}

fn configure_test_command(command: &mut tokio::process::Command, context: &BackendStartContext) {
    #[cfg(test)]
    {
        let _ = context;
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("apps::backend_tests::fixture_backend_main")
            .arg("--nocapture");
    }
    #[cfg(not(test))]
    {
        command
            .arg(CONTEXT_ARGUMENT)
            .arg(std::path::Path::new(&context.data_dir).join("run/context.json"));
    }
}
fn configure_test_environment(
    command: &mut tokio::process::Command,
    context: &BackendStartContext,
) {
    #[cfg(test)]
    if !system::real_isolation() {
        command.env(
            "TGDRIVE_TEST_BACKEND_CONTEXT",
            std::path::Path::new(&context.data_dir).join("run/context.json"),
        );
    }
    #[cfg(not(test))]
    let _ = (command, context);
}

pub(crate) async fn connect(run: &Run) -> PlatformResult<UnixStream> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if run.caller.cancelled.is_cancelled() {
        return Err(PlatformError::new(
            error_code::BACKEND_UNAVAILABLE,
            "后端启动轮次已撤销",
        ));
    }
    let meta = std::fs::symlink_metadata(&run.socket)?;
    if !meta.file_type().is_socket() || meta.uid() != run.identity.uid {
        return Err(PlatformError::new(
            error_code::BACKEND_UNAVAILABLE,
            "后端套接字身份不匹配",
        ));
    }
    let stream = UnixStream::connect(&run.socket).await?;
    if stream.peer_cred()?.uid() != run.identity.uid {
        return Err(PlatformError::new(
            error_code::BACKEND_UNAVAILABLE,
            "后端连接身份不匹配",
        ));
    }
    Ok(stream)
}

pub(crate) async fn request(
    run: &Arc<Run>,
    request: hyper::Request<axum::body::Body>,
) -> PlatformResult<hyper::Response<axum::body::Body>> {
    let stream = connect(run).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|_| unavailable())?;
    let cancelled = run.caller.cancelled.clone();
    let stopped = CancellationToken::new();
    let connection_stop = stopped.clone();
    let mut abort = ConnectionAbort(Some(stopped.clone()));
    tokio::spawn(async move {
        tokio::select! { _ = cancelled.cancelled() => {}, _ = connection_stop.cancelled() => {}, _ = connection => {} }
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| unavailable())?;
    let (parts, body) = response.into_parts();
    abort.0.take();
    Ok(hyper::Response::from_parts(
        parts,
        axum::body::Body::new(ConnectionBody { body, stopped }),
    ))
}
struct ConnectionAbort(Option<CancellationToken>);
impl Drop for ConnectionAbort {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

// 丢弃正文即关闭私有连接；Hyper 不能在客户端取消后继续替它排空无限 SSE。
struct ConnectionBody {
    body: hyper::body::Incoming,
    stopped: CancellationToken,
}
impl http_body::Body for ConnectionBody {
    type Data = bytes::Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}
impl Drop for ConnectionBody {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}
fn unavailable() -> PlatformError {
    PlatformError::new(
        error_code::BACKEND_UNAVAILABLE,
        "应用后端未能就绪或连接已中断",
    )
}
async fn ready(
    platform: &Arc<AppPlatform>,
    run: Arc<Run>,
    app: &store::Installation,
) -> PlatformResult<()> {
    tokio::time::timeout(
        Duration::from_secs(BACKEND_READY_SECONDS),
        ready_inner(platform, run, app),
    )
    .await
    .map_err(|_| unavailable())?
}
async fn ready_inner(
    platform: &Arc<AppPlatform>,
    run: Arc<Run>,
    app: &store::Installation,
) -> PlatformResult<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(BACKEND_READY_SECONDS);
    loop {
        if run.caller.cancelled.is_cancelled() {
            return Err(unavailable());
        }
        let req = hyper::Request::builder()
            .uri(BACKEND_READY_PATH)
            .header("host", "tgdrive")
            .body(axum::body::Body::empty())
            .map_err(|_| unavailable())?;
        if let Ok(response) = request(&run, req).await
            && response.status() == hyper::StatusCode::OK
        {
            let body = axum::body::Body::new(response.into_body());
            let bytes = axum::body::to_bytes(body, MAX_JSON_REQUEST_BYTES as usize)
                .await
                .map_err(|_| unavailable())?;
            let raw = tgdrive_contract::validation::parse_unique_json(&bytes)
                .map_err(|_| unavailable())?;
            let response: BackendReadyResponse =
                serde_json::from_value(raw).map_err(|_| unavailable())?;
            let manifest = app.manifest()?;
            if response.app_id != app.id
                || response.version != manifest.version
                || response.generation != app.generation
                || response.run_id != run.caller.id
            {
                return Err(unavailable());
            }
            let negotiated = tgdrive_contract::validation::negotiate_contract(
                CONTRACT_VERSION,
                response.contract,
                manifest.contract,
            )?;
            *run.negotiated.write().unwrap() = negotiated;
            if let Ok(bytes) = super::safe_fs::Directory::open(
                std::path::Path::new(&run.data_dir).join("run").as_path(),
                false,
            )?
            .read_bytes("context.json", MAX_JSON_REQUEST_BYTES)
            {
                let context: BackendStartContext = serde_json::from_value(
                    tgdrive_contract::validation::parse_unique_json(&bytes)?,
                )?;
                *run.limits.write().unwrap() = context.limits;
            }
            // 启动上下文是启动时的快照；启动途中主人改过总体限制时以当前值为准。
            run.sync_shared_limits(&platform.shared_limits().await?.report());
            let mut conn = platform.host.vfs.db.pool.acquire().await?;
            store::installation(&mut conn, &app.id)
                .await?
                .check(&app.generation)?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(unavailable());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn serve_host(platform: Arc<AppPlatform>, run: Arc<Run>, listener: UnixListener) {
    let weak = Arc::downgrade(&platform);
    let shutdown = platform.host.restore.shutdown.clone();
    drop(platform);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {_=run.caller.cancelled.cancelled()=>break,_=shutdown.cancelled()=>break,accepted=listener.accept()=>accepted};
            let Ok((stream, _)) = accepted else { break };
            if !stream
                .peer_cred()
                .is_ok_and(|cred| cred.uid() == run.identity.uid)
            {
                continue;
            }
            let weak = weak.clone();
            let current = run.clone();
            tokio::spawn(async move {
                use tower::ServiceExt;
                let caller = current.clone();
                let service = hyper::service::service_fn(
                    move |request: hyper::Request<hyper::body::Incoming>| {
                        let weak = weak.clone();
                        let current = caller.clone();
                        async move {
                            use axum::response::IntoResponse;
                            use futures_util::StreamExt;
                            let Some(platform) = weak.upgrade() else {
                                return Ok::<_, std::convert::Infallible>(
                                    hyper::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                                );
                            };
                            let router = super::http::self_router(platform, current.caller.clone());
                            let activity =
                                if request.uri().path() != super::endpoint_path("events", &[]) {
                                    Some(current.activity())
                                } else {
                                    None
                                };
                            let (parts, body) = request.into_parts();
                            let response = router
                                .oneshot(hyper::Request::from_parts(
                                    parts,
                                    axum::body::Body::new(body),
                                ))
                                .await
                                .expect("Router 错误不可达");
                            let (parts, body) = response.into_parts();
                            let stream = body.into_data_stream().map(move |chunk| {
                                let _ = &activity;
                                chunk
                            });
                            Ok::<_, std::convert::Infallible>(hyper::Response::from_parts(
                                parts,
                                axum::body::Body::from_stream(stream),
                            ))
                        }
                    },
                );
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .keep_alive(true)
                    .max_buf_size(MAX_HEADER_BYTES as usize)
                    .header_read_timeout(Duration::from_secs(CONTROL_TIMEOUT_SECONDS))
                    .timer(hyper_util::rt::TokioTimer::new());
                let connection =
                    builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                tokio::select! {_=current.caller.cancelled.cancelled()=>{},_=connection=>{}}
            });
        }
    });
}

fn capture_log(
    platform: &Arc<AppPlatform>,
    id: &str,
    source: &'static str,
    mut reader: impl tokio::io::AsyncRead + Send + Unpin + 'static,
) {
    let directory = platform.root.join("logs").join(id);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if tokio::fs::create_dir_all(&directory).await.is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
        }
        let path = directory.join(format!("{source}.log"));
        let mut buffer = [0u8; 16 * 1024];
        while let Ok(n) = reader.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            if tokio::fs::metadata(&path)
                .await
                .is_ok_and(|m| m.len() > 4 * 1024 * 1024)
            {
                let _ = tokio::fs::remove_file(directory.join(format!("{source}.2.log"))).await;
                let _ = tokio::fs::rename(
                    directory.join(format!("{source}.1.log")),
                    directory.join(format!("{source}.2.log")),
                )
                .await;
                let _ = tokio::fs::rename(&path, directory.join(format!("{source}.1.log"))).await;
            }
            let Ok(mut file) = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            else {
                break;
            };
            if file.write_all(&buffer[..n]).await.is_err() {
                break;
            }
        }
    });
}

pub(crate) fn start(platform: &Arc<AppPlatform>) {
    let weak = Arc::downgrade(platform);
    tokio::spawn(supervise(weak));
}
async fn supervise(weak: Weak<AppPlatform>) {
    loop {
        let Some(platform) = weak.upgrade() else {
            return;
        };
        if platform.host.restore.shutdown.is_cancelled() {
            let ids = platform
                .backends
                .slots
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            futures_util::future::join_all(ids.iter().map(|id| platform.backends.stop(id))).await;
            return;
        }
        let _ = platform.prune_uploads().await;
        let records: Vec<store::Installation> =
            match sqlx::query_as("SELECT * FROM app_installations")
                .fetch_all(&platform.host.vfs.db.pool)
                .await
            {
                Ok(records) => records,
                Err(_) => {
                    drop(platform);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
        for app in records {
            let Ok(manifest) = app.manifest() else {
                continue;
            };
            let Some(backend) = manifest.backend else {
                continue;
            };
            let run = platform.backends.run(&app.id);
            if !app.enabled
                || run
                    .as_ref()
                    .is_some_and(|run| run.caller.generation != app.generation)
            {
                let _ = platform.backends.stop(&app.id).await;
                if !app.enabled {
                    continue;
                }
            }
            if let Some(run) = platform.backends.run(&app.id) {
                if backend.run_mode == RunMode::OnDemand
                    && run.active.load(Ordering::Acquire) == 0
                    && now().0 > run.keep_until.load(Ordering::Acquire)
                    && now().0 - run.last_activity.load(Ordering::Acquire)
                        > BACKEND_IDLE_SECONDS as i64 * 1000
                {
                    let _ = platform.backends.stop(&app.id).await;
                    continue;
                }
            } else if backend.run_mode == RunMode::Resident && app.enabled && system::available() {
                let slot = platform.backends.slot(&app.id);
                let ready = {
                    let failures = slot.failures.lock().unwrap();
                    failures.last().is_none_or(|last| {
                        Instant::now()
                            > *last
                                + Duration::from_secs(
                                    1u64 << failures.len().saturating_sub(1).min(4),
                                )
                    })
                };
                if ready
                    && !slot.crash_blocked.load(Ordering::Acquire)
                    && platform.backends.status(&app.id, backend.run_mode).state
                        != BackendState::Starting
                {
                    let platform = platform.clone();
                    let id = app.id.clone();
                    tokio::spawn(async move {
                        let _ = platform.backends.ensure(&platform, &id).await;
                    });
                }
            }
        }
        drop(platform);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
impl Manager {
    /// 总体限制变更后同步运行中各后端对外报告的限额，上限本身已写在共同的根 cgroup 上。
    pub(crate) fn update_shared_limits(&self, shared: &AppResourceLimits) {
        for slot in self.slots.lock().unwrap().values() {
            if let Some(run) = slot.run.read().unwrap().as_ref() {
                run.sync_shared_limits(shared);
            }
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        for slot in self.slots.lock().unwrap().values() {
            if let Some(run) = slot.run.read().unwrap().as_ref() {
                run.caller.cancelled.cancel();
                system::kill_group(run.pid, run.identity, libc::SIGKILL);
            }
        }
    }
}
