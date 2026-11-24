//! 只用于 Linux 隔离验收；不进入宿主发布构建或应用目录。
use serde_json::{Value, json};
use std::ffi::CString;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tgdrive_backend_sdk::{
    BackendServer, Error, ForwardRequest, HostClient, HttpResponse, Result, StartContext,
    UnixHostClient, json_response,
};
use tgdrive_contract::CONTRACT_VERSION;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("--leaf") => {
            ignore_term();
            std::fs::write(&args[2], std::process::id().to_string())?;
            wait_forever();
        }
        Some("--tree") => {
            ignore_term();
            let leaf_path = format!("{}.leaf", args[2]);
            let child = Command::new(std::env::current_exe()?)
                .args(["--leaf", &leaf_path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            wait_for_file(Path::new(&leaf_path)).await?;
            // 报告出现时必须已写完，不能让 is_file() 的读者读到空文件或半截 JSON。
            let pending = format!("{}.{}.pending", args[2], std::process::id());
            std::fs::write(
                &pending,
                json!([std::process::id(), child.id()]).to_string(),
            )?;
            std::fs::rename(pending, &args[2])?;
            wait_forever();
        }
        Some("--memory") => {
            // 固定、短时、最多 96 MiB 的负载；只有已委派 cgroup 的验收才调用。
            let mut bytes = vec![0u8; 96 * 1024 * 1024];
            for index in (0..bytes.len()).step_by(4096) {
                bytes[index] = 1;
            }
            std::hint::black_box(&bytes);
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
        _ => {}
    }
    let start = Arc::new(StartContext::from_args(
        std::env::args_os(),
        CONTRACT_VERSION,
    )?);
    create_database(&start.context.database_path)?;
    let host = Arc::new(UnixHostClient::new(&start));
    BackendServer::bind((*start).clone())?
        .run(
            move |request| route(start.clone(), host.clone(), request),
            |_| async {},
        )
        .await
}

fn ignore_term() {
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        // 进程名含伪造 Uid 行，验收宿主读取内核真实身份而非信任应用名称。
        libc::prctl(libc::PR_SET_NAME, c"\nUid:0 0 0 0".as_ptr(), 0, 0, 0);
    }
}
fn wait_forever() -> ! {
    loop {
        unsafe {
            libc::pause();
        }
    }
}
async fn wait_for_file(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(Error::Timeout)
}
fn header<'a>(request: &'a ForwardRequest, name: &str) -> Result<&'a str> {
    request
        .request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(Error::Protocol("缺少探针参数"))
}
async fn route(
    start: Arc<StartContext>,
    host: Arc<UnixHostClient>,
    request: ForwardRequest,
) -> Result<HttpResponse> {
    match request.request.uri().path() {
        "/environment" => {
            let environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
            let mut groups = [0u32; 32];
            let count = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            json_response(
                200,
                &json!({
                    "uid": unsafe { libc::geteuid() }, "gid": unsafe { libc::getegid() },
                    "groups": &groups[..count as usize], "environment": environment,
                    "nproc": limit(libc::RLIMIT_NPROC)?, "nofile": limit(libc::RLIMIT_NOFILE)?,
                    "core": limit(libc::RLIMIT_CORE)?,
                }),
            )
        }
        "/inspect" => {
            let path = Path::new(header(&request, "x-probe-path")?);
            let readable = std::fs::read(path).is_ok();
            let directory_readable = std::fs::read_dir(path).is_ok();
            let writable = if path.exists() {
                std::fs::OpenOptions::new().write(true).open(path).is_ok()
            } else {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(file) => {
                        drop(file);
                        std::fs::remove_file(path)?;
                        true
                    }
                    Err(_) => false,
                }
            };
            json_response(
                200,
                &json!({"readable":readable,"directory_readable":directory_readable,"writable":writable}),
            )
        }
        "/socket" => {
            let path = header(&request, "x-probe-path")?;
            let connected = tokio::net::UnixStream::connect(path).await.is_ok();
            let mut other = (*start).clone();
            other.context.host_socket = path.into();
            let client = UnixHostClient::new(&other);
            let authorized = matches!(
                tokio::time::timeout(Duration::from_secs(3), client.runtime_info()).await,
                Ok(Ok(_))
            );
            json_response(200, &json!({"connected":connected,"authorized":authorized}))
        }
        "/host-info" => json_response(200, &host.runtime_info().await?),
        "/ignore-stop" => {
            ignore_term();
            json_response(200, &json!({"ignored":true}))
        }
        "/limits" => json_response(200, &exercise_limits(&start.context.temp_dir)?),
        "/descendants" | "/descendants-detached" => {
            use std::os::unix::process::CommandExt;
            let report =
                Path::new(&start.context.temp_dir).join(format!("tree-{}", std::process::id()));
            let _ = std::fs::remove_file(&report);
            let _ = std::fs::remove_file(format!("{}.leaf", report.display()));
            let mut command = Command::new(std::env::current_exe()?);
            command
                .arg("--tree")
                .arg(&report)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if request.request.uri().path() == "/descendants-detached" {
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            command.spawn()?;
            wait_for_file(&report).await?;
            let pids: Vec<u32> = serde_json::from_slice(&std::fs::read(report)?)?;
            json_response(200, &json!({"pids":pids}))
        }
        "/memory" => {
            let status = tokio::process::Command::new(std::env::current_exe()?)
                .arg("--memory")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            use std::os::unix::process::ExitStatusExt;
            json_response(
                200,
                &json!({"signal":status.signal(),"success":status.success()}),
            )
        }
        "/cpu" => {
            let before = cpu_nanos()?;
            let started = Instant::now();
            let mut value = 1u64;
            while started.elapsed() < Duration::from_millis(1500) {
                value =
                    std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
            json_response(
                200,
                &json!({"cpu_ns":cpu_nanos()? - before,"wall_ns":started.elapsed().as_nanos() as u64}),
            )
        }
        "/exit" => std::process::exit(42),
        _ => json_response(404, &json!({"error":"unknown_probe_route"})),
    }
}
fn limit(resource: libc::c_uint) -> Result<Value> {
    let mut value = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(resource, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(json!({"soft":value.rlim_cur,"hard":value.rlim_max}))
}
fn exercise_limits(temp: &str) -> Result<Value> {
    // 固定负载超过旧上限，但不再根据继承的系统上限循环到资源耗尽。
    let mut files = Vec::new();
    let mut file_error = None;
    for _ in 0..2048 {
        match std::fs::File::open("/dev/null") {
            Ok(file) => files.push(file),
            Err(error) => {
                file_error = error.raw_os_error();
                break;
            }
        }
    }
    let opened = files.len();
    drop(files);
    let mut children = Vec::new();
    let mut process_error = None;
    for _ in 0..128 {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            wait_forever();
        }
        if pid < 0 {
            process_error = std::io::Error::last_os_error().raw_os_error();
            break;
        }
        children.push(pid);
    }
    let spawned = children.len();
    for pid in children {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    let raised = libc::rlimit {
        rlim_cur: 1,
        rlim_max: 1,
    };
    let core_raise = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raised) };
    let core_error = std::io::Error::last_os_error().raw_os_error();
    let directory = CString::new(temp).map_err(|_| Error::Protocol("临时路径无效"))?;
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if child == 0 {
        unsafe {
            libc::chdir(directory.as_ptr());
            libc::abort();
        }
    }
    let mut status = 0;
    if unsafe { libc::waitpid(child, &mut status, 0) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(json!({
        "opened":opened,"file_error":file_error,"spawned":spawned,"process_error":process_error,
        "core_raise":core_raise,"core_error":core_error,"core_signal":libc::WTERMSIG(status),"core_dumped":libc::WCOREDUMP(status),
    }))
}
fn cpu_nanos() -> Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64)
}
fn create_database(path: &str) -> Result<()> {
    let path = CString::new(path).map_err(|_| Error::Protocol("数据库路径无效"))?;
    let mut database = std::ptr::null_mut();
    let code = unsafe { libsqlite3_sys::sqlite3_open(path.as_ptr(), &mut database) };
    if code != libsqlite3_sys::SQLITE_OK {
        return Err(Error::Protocol("探针数据库无法打开"));
    }
    let sql = CString::new(format!("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS isolation(uid INTEGER); INSERT INTO isolation VALUES({});", unsafe { libc::geteuid() })).unwrap();
    let code = unsafe {
        libsqlite3_sys::sqlite3_exec(
            database,
            sql.as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    unsafe {
        libsqlite3_sys::sqlite3_close(database);
    }
    if code != libsqlite3_sys::SQLITE_OK {
        return Err(Error::Protocol("探针数据库初始化失败"));
    }
    Ok(())
}
