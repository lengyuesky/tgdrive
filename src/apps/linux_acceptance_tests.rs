//! 仅在独立 Linux root 容器显式执行，使用真实生产启动/备份辅助入口。
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tgdrive_contract::{http::*, manifest::RunMode, runtime::*};

pub(super) struct RealIsolation;
impl RealIsolation {
    pub(super) fn new() -> Self {
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "必须在独立 Linux root 容器内运行"
        );
        assert!(std::env::var_os("TGDRIVE_TEST_HOST_EXECUTABLE").is_some());
        backend_system::REAL_ISOLATION.store(true, Ordering::Release);
        Self
    }
}
impl Drop for RealIsolation {
    fn drop(&mut self) {
        backend_system::REAL_ISOLATION.store(false, Ordering::Release);
    }
}
async fn probe(run: &Arc<backend::Run>, route: &str, path: Option<&Path>) -> serde_json::Value {
    let mut request = Request::builder()
        .uri(route)
        .header("host", "tgdrive")
        .header(
            FORWARD_CONTEXT_HEADER,
            super::backend_tests::forward(run, ForwardCaller::Owner, ForwardEntry::Page),
        );
    if let Some(path) = path {
        request = request.header("x-probe-path", path.to_str().unwrap());
    }
    let response = backend::request(run, request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
pub(super) async fn gone(pid: u32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("进程 {pid} 未彻底回收（包括僵尸进程）"));
}
async fn descendants(run: &Arc<backend::Run>, detached: bool) -> Vec<u32> {
    let route = if detached {
        "/descendants-detached"
    } else {
        "/descendants"
    };
    serde_json::from_value(probe(run, route, None).await["pids"].clone()).unwrap()
}
async fn all_gone(run: &Arc<backend::Run>, children: &[u32]) {
    gone(run.pid).await;
    for pid in children {
        gone(*pid).await;
    }
}
async fn install_probe(platform: &Arc<AppPlatform>, id: &str, digest: &str, executable: &Path) {
    let package = platform.package_root.join(digest).join("backend");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::copy(executable, package.join("app")).unwrap();
    let manifest = serde_json::json!({
        "id":id,"name":"隔离探针","version":"1.0.0","contract":{"major":1,"minor":0},
        "description":"仅 Linux 验收","author":"测试","files":{"read":true,"write":true},
        "backend":{"entry":"backend/app","run_mode":"on_demand","external_service":true}
    });
    let target = match std::env::consts::ARCH {
        "aarch64" => "linux-arm64",
        "x86_64" => "linux-amd64",
        _ => panic!("验收架构不支持"),
    };
    sqlx::query("INSERT INTO app_installations(id,manifest,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at) VALUES(?,?,?,?,1,0,?,?,?,?,1,?,?)")
        .bind(id).bind(manifest.to_string()).bind(format!("generation-{id}")).bind(format!("state-{id}"))
        .bind(r#"{"mode":"all","access":"read_write"}"#).bind(format!("scope-{id}"))
        .bind(target).bind(digest).bind(now().0).bind(now().0)
        .execute(&platform.host.vfs.db.pool).await.unwrap();
}
struct RelaxedPermissions(Vec<(PathBuf, std::fs::Permissions)>);
impl RelaxedPermissions {
    fn for_socket(run: &backend::Run) -> Self {
        let paths = [
            (PathBuf::from(&run.data_dir), 0o555),
            (Path::new(&run.data_dir).join("run"), 0o555),
            (Path::new(&run.data_dir).join("run/host.sock"), 0o666),
        ];
        let mut saved = Vec::new();
        for (path, mode) in paths {
            saved.push((
                path.clone(),
                std::fs::metadata(&path).unwrap().permissions(),
            ));
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        Self(saved)
    }
}
impl Drop for RelaxedPermissions {
    fn drop(&mut self) {
        for (path, permissions) in &self.0 {
            std::fs::set_permissions(path, permissions.clone()).unwrap();
        }
    }
}
async fn backup_checks(platform: &Arc<AppPlatform>, run: &Arc<backend::Run>) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let output = temp.path().join("snapshot.sqlite3");
    assert!(
        snapshot::snapshot(
            &platform.host.vfs.db,
            &run.caller.app_id,
            Path::new(&run.data_dir),
            &output
        )
        .await?
    );
    assert!(std::fs::read(&output)?.starts_with(b"SQLite format 3\0"));
    let database = Path::new(&run.data_dir).join("state/app.sqlite3");
    assert_eq!(std::fs::metadata(&database)?.uid(), run.identity.uid);
    // 额外直接调用同一生产辅助入口，核验真实辅助身份，不在测试中降权。
    let configuration = temp.path().join("helper.json");
    std::fs::write(
        &configuration,
        serde_json::to_vec(&serde_json::json!({
            "identity":run.identity,"data_dir":run.data_dir,"max_bytes":1024 * 1024,
        }))?,
    )?;
    std::fs::set_permissions(&configuration, std::fs::Permissions::from_mode(0o600))?;
    let helper = || {
        let mut command =
            tokio::process::Command::new(backend_system::helper_executable().unwrap());
        command
            .arg("internal-app-backup")
            .arg(&configuration)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("HOME", Path::new(&run.data_dir).join("state"))
            .env("TMPDIR", Path::new(&run.data_dir).join("tmp"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    };
    let result = helper().output().await?;
    assert!(result.status.success());
    assert!(result.stdout.starts_with(b"SQLite format 3\0"));
    assert!(String::from_utf8_lossy(&result.stderr).contains(&format!(
        "UID={} GID={}",
        run.identity.uid, run.identity.gid
    )));
    let previous = std::fs::read(&output)?;
    let saved = database.with_file_name("saved.sqlite3");
    std::fs::rename(&database, &saved)?;
    std::os::unix::fs::symlink(&platform.host.rt.env.db_path, &database)?;
    assert!(
        snapshot::snapshot(
            &platform.host.vfs.db,
            &run.caller.app_id,
            Path::new(&run.data_dir),
            &output
        )
        .await
        .is_err()
    );
    let result = helper().output().await?;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains(&format!(
        "UID={} GID={}",
        run.identity.uid, run.identity.gid
    )));
    assert_eq!(std::fs::read(&output)?, previous);
    std::fs::remove_file(&database)?;
    std::fs::rename(saved, database)?;
    assert!(!run.caller.cancelled.is_cancelled());
    println!(
        "隔离验收：真实 Online Backup 辅助进程 UID/GID={}；无法读取链接到宿主的主库；失败保留成功快照及后端",
        run.identity.uid
    );
    Ok(())
}
fn counter(path: &Path, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.trim().parse().unwrap())
        })
        .unwrap()
}
async fn resources(platform: &AppPlatform, run: &Arc<backend::Run>) {
    let delegated = std::env::var_os("TGDRIVE_TEST_REQUIRE_CGROUP").is_some();
    if delegated {
        // 后端运行中收紧宿主的总体限制：不重启即写入全部应用后端共同所在的根组。
        let report = platform
            .resource_limits_update(AppResourceLimitsUpdateRequest {
                memory_bytes: tgdrive_contract::ByteCount(64 * 1024 * 1024),
                cpu_millis: 100,
            })
            .await
            .unwrap();
        assert!(
            report.memory_enforced && report.cpu_enforced,
            "委派 cgroup 后总体限制仍未强制"
        );
    }
    let info: RuntimeInfo = serde_json::from_value(probe(run, "/host-info", None).await).unwrap();
    let limits = info.limits.backend.unwrap();
    assert!(limits.enforced.pids && limits.enforced.open_files);
    let environment = probe(run, "/environment", None).await;
    for name in ["soft", "hard"] {
        assert_eq!(environment["nproc"][name], limits.pids);
        assert_eq!(environment["nofile"][name], limits.open_files);
        assert_eq!(environment["core"][name], 0);
    }
    let exercised = probe(run, "/limits", None).await;
    assert_eq!(exercised["file_error"], libc::EMFILE);
    assert!(exercised["opened"].as_u64().unwrap() < u64::from(limits.open_files));
    assert_eq!(exercised["process_error"], libc::EAGAIN);
    assert!(exercised["spawned"].as_u64().unwrap() < u64::from(limits.pids));
    assert_eq!(exercised["core_raise"], -1);
    assert_eq!(exercised["core_error"], libc::EPERM);
    assert_eq!(exercised["core_signal"], libc::SIGABRT);
    assert_eq!(exercised["core_dumped"], false);
    println!(
        "隔离验收：RLIMIT_NPROC={} 实际 EAGAIN；RLIMIT_NOFILE={} 实际 EMFILE；core 上下限 0、不可提高且 SIGABRT 无转储",
        limits.pids, limits.open_files
    );
    if delegated {
        assert!(
            limits.enforced.memory && limits.enforced.cpu,
            "委派 cgroup 后仍未强制限额"
        );
        assert_eq!(limits.memory_bytes.0, 64 * 1024 * 1024);
        assert_eq!(limits.cpu_millis, 100);
        // 上限设在根组，各后端合计共用；本轮子组只收纳进程，用于整组回收。
        let leaf = backend_system::cgroup_path(run.identity.uid, &run.caller.id).unwrap();
        let leaf = Path::new(&leaf);
        let group = backend_system::cgroup_root().unwrap();
        let group = Path::new(&group);
        assert_eq!(
            std::fs::read_to_string(group.join("memory.max"))
                .unwrap()
                .trim(),
            limits.memory_bytes.0.to_string()
        );
        assert_eq!(
            std::fs::read_to_string(group.join("cpu.max"))
                .unwrap()
                .trim(),
            format!("{} 100000", u64::from(limits.cpu_millis) * 100)
        );
        assert_eq!(
            std::fs::read_to_string(group.join("memory.swap.max"))
                .unwrap()
                .trim(),
            "0",
            "有内存上限时不得换出到交换空间"
        );
        assert!(
            std::fs::read_to_string(leaf.join("cgroup.procs"))
                .unwrap()
                .lines()
                .any(|pid| pid == run.pid.to_string())
        );
        let kills = counter(&group.join("memory.events"), "oom_kill");
        assert_eq!(probe(run, "/memory", None).await["signal"], libc::SIGKILL);
        assert!(counter(&group.join("memory.events"), "oom_kill") > kills);
        let throttled = counter(&group.join("cpu.stat"), "nr_throttled");
        let cpu = probe(run, "/cpu", None).await;
        assert!(counter(&group.join("cpu.stat"), "nr_throttled") > throttled);
        let allowance = cpu["wall_ns"].as_u64().unwrap() * u64::from(limits.cpu_millis) / 1000 * 2
            + 100_000_000;
        assert!(cpu["cpu_ns"].as_u64().unwrap() < allowance);
        println!(
            "隔离验收：根组总体限制运行中生效，内存实际 OOM kill、CPU 实际节流且 enforced=true"
        );
        // 放开为不限制，后续步骤不受本项验收的小额度影响。
        let report = platform
            .resource_limits_update(AppResourceLimitsUpdateRequest {
                memory_bytes: tgdrive_contract::ByteCount(0),
                cpu_millis: 0,
            })
            .await
            .unwrap();
        assert!(!report.memory_enforced && !report.cpu_enforced);
        for file in ["memory.max", "memory.swap.max"] {
            assert_eq!(
                std::fs::read_to_string(group.join(file)).unwrap().trim(),
                "max"
            );
        }
    } else {
        assert!(!limits.enforced.memory && !limits.enforced.cpu);
        println!(
            "隔离验收：无可写 cgroup 仍正常 ready，memory=false,cpu=false,pids=true,open_files=true；可写 cgroup 实际限额本轮未测"
        );
    }
}
#[tokio::test]
#[ignore = "须在独立 Linux root 容器显式执行；使用真实生产启动/备份辅助二进制"]
async fn linux_root_isolation() {
    let _isolation = RealIsolation::new();
    acceptance().await.unwrap();
}
async fn acceptance() -> anyhow::Result<()> {
    let (_data, platform, _fake) = super::tests::fixture().await;
    let executable = std::env::var_os("TGDRIVE_TEST_PROBE_EXECUTABLE").expect("验收探针未构建");
    install_probe(
        &platform,
        "probe-one",
        &"b".repeat(64),
        Path::new(&executable),
    )
    .await;
    install_probe(
        &platform,
        "probe-two",
        &"c".repeat(64),
        Path::new(&executable),
    )
    .await;
    let first = platform.backends.ensure(&platform, "probe-one").await?;
    let second = platform.backends.ensure(&platform, "probe-two").await?;
    assert_ne!(first.identity.uid, second.identity.uid);
    assert_ne!(first.identity.gid, second.identity.gid);
    assert_eq!(first.identity.uid, first.identity.gid);
    assert_eq!(second.identity.uid, second.identity.gid);
    assert!(first.identity.uid >= 20000);
    let environment = probe(&first, "/environment", None).await;
    assert_eq!(environment["uid"], first.identity.uid);
    assert_eq!(environment["gid"], first.identity.gid);
    assert_eq!(environment["groups"], serde_json::json!([]));
    let keys = environment["environment"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        ["PATH", "LANG", "LC_ALL", "HOME", "TMPDIR"]
            .into_iter()
            .collect()
    );
    assert_eq!(
        environment["environment"]["HOME"],
        format!("{}/state", first.data_dir)
    );
    assert_eq!(
        environment["environment"]["TMPDIR"],
        format!("{}/tmp", first.data_dir)
    );
    println!(
        "隔离验收：独立 UID/GID {}、{}，零附加组、环境白名单及私有 HOME/TMPDIR 通过",
        first.identity.uid, second.identity.uid
    );
    for path in [
        PathBuf::from(&platform.host.rt.env.db_path),
        Path::new(&second.data_dir).join("state/app.sqlite3"),
        PathBuf::from(format!("/proc/{}/environ", std::process::id())),
    ] {
        let checked = probe(&first, "/inspect", Some(&path)).await;
        assert_eq!(checked["readable"], false);
        assert_eq!(checked["writable"], false);
    }
    assert_eq!(
        probe(&first, "/inspect", Some(Path::new(&second.data_dir))).await["directory_readable"],
        false
    );
    assert_eq!(
        probe(
            &first,
            "/inspect",
            Some(&Path::new(&first.data_dir).join("tmp/own-file"))
        )
        .await["writable"],
        true
    );
    assert_eq!(
        probe(
            &first,
            "/inspect",
            Some(Path::new("/tmp/shared-write-must-fail"))
        )
        .await["writable"],
        false
    );
    assert_eq!(
        probe(
            &first,
            "/inspect",
            Some(Path::new("/var/tmp/shared-write-must-fail"))
        )
        .await["writable"],
        false
    );
    println!(
        "隔离验收：宿主主库、其他应用文件/目录、宿主 /proc、共享临时目录拒绝；本应用 tmp 可写"
    );
    let target = Path::new(&second.data_dir).join("run/host.sock");
    assert_eq!(
        probe(&first, "/socket", Some(&target)).await["connected"],
        false
    );
    {
        // 仅临时放宽测试路径，以单独证明 SO_PEERCRED 检查而非只测文件权限。
        let _permissions = RelaxedPermissions::for_socket(&second);
        let checked = probe(&first, "/socket", Some(&target)).await;
        assert_eq!(checked["connected"], true);
        assert_eq!(checked["authorized"], false);
    }
    let stream = backend::connect(&first).await?;
    assert_eq!(stream.peer_cred()?.uid(), first.identity.uid);
    println!(
        "隔离验收：其他 UID 即使能连接 host.sock 仍被 SO_PEERCRED 拒绝；宿主出向 UID 核验通过"
    );
    resources(&platform, &first).await;
    backup_checks(&platform, &first).await?;
    let mut children = descendants(&first, false).await;
    children.extend(descendants(&first, true).await);
    platform.backends.stop("probe-one").await?;
    all_gone(&first, &children).await;
    assert!(first.caller.cancelled.is_cancelled());
    println!(
        "隔离验收：停止回收父进程、拒绝 SIGTERM 的子/孙进程、setsid 脱离的子/孙进程（无僵尸残留）"
    );
    let cancelled_run = platform.backends.ensure(&platform, "probe-one").await?;
    let children = descendants(&cancelled_run, true).await;
    assert_eq!(
        probe(&cancelled_run, "/ignore-stop", None).await["ignored"],
        true
    );
    let stopping_platform = platform.clone();
    let stopping = tokio::spawn(async move { stopping_platform.backends.stop("probe-one").await });
    tokio::time::timeout(
        Duration::from_secs(2),
        cancelled_run.caller.cancelled.cancelled(),
    )
    .await?;
    assert!(!cancelled_run.finished.is_cancelled());
    stopping.abort();
    let _ = stopping.await;
    let replacement = platform.backends.ensure(&platform, "probe-one").await?;
    all_gone(&cancelled_run, &children).await;
    assert!(!replacement.finished.is_cancelled());
    assert!(probe(&replacement, "/host-info", None).await.is_object());
    platform.backends.stop("probe-one").await?;
    println!("隔离验收：取消停止请求仍回收旧轮次；立即启动新轮次不被旧 UID 监护清理误杀");
    assert_eq!(BACKEND_RESTART_WINDOW_SECONDS, 600);
    assert_eq!(MAX_BACKEND_RESTARTS, 5);
    let mut run = platform.backends.ensure(&platform, "probe-one").await?;
    for attempt in 0..6 {
        let children = descendants(&run, false).await;
        let crashed = Instant::now();
        let _ = backend::request(
            &run,
            Request::builder()
                .uri("/exit")
                .header("host", "tgdrive")
                .header(
                    FORWARD_CONTEXT_HEADER,
                    super::backend_tests::forward(&run, ForwardCaller::Owner, ForwardEntry::Page),
                )
                .body(Body::empty())?,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), run.finished.cancelled()).await?;
        assert!(run.caller.cancelled.is_cancelled());
        all_gone(&run, &children).await;
        if attempt < 5 {
            run = platform.backends.ensure(&platform, "probe-one").await?;
            let minimum = 1u64 << attempt;
            // 回收子孙的时间也属于崩溃后的退避，不能只测回收结束后的剩余等待。
            let elapsed = crashed.elapsed();
            assert!(
                elapsed >= Duration::from_millis(minimum * 1000 - 250),
                "第 {} 次退避过短：{elapsed:?}",
                attempt + 1
            );
            assert!(elapsed < Duration::from_secs(minimum + 5));
            println!(
                "隔离验收：第 {} 次真实崩溃退避 {:.3?}",
                attempt + 1,
                elapsed
            );
        }
    }
    assert!(
        platform
            .backends
            .ensure(&platform, "probe-one")
            .await
            .is_err()
    );
    assert_eq!(
        platform
            .backends
            .status("probe-one", RunMode::OnDemand)
            .state,
        BackendState::Failed
    );
    platform.backends.restart(&platform, "probe-one").await?;
    platform.backends.stop("probe-one").await?;
    println!(
        "隔离验收：10 分钟窗口内最多自动重启 5 次，之后 failed；主人重启解除阻断，崩溃均回收子孙进程"
    );
    let mut children = descendants(&second, false).await;
    children.extend(descendants(&second, true).await);
    let app = platform.installed("probe-two").await?;
    platform
        .uninstall("probe-two", &app.state_revision, true)
        .await?;
    all_gone(&second, &children).await;
    assert!(second.caller.cancelled.is_cancelled());
    println!("隔离验收：卸载回收后端、子/孙进程和轮次能力");
    platform.host.restore.shutdown.cancel();
    platform.shutdown().await?;
    Ok(())
}
