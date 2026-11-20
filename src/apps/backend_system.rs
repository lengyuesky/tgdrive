//! 后端硬隔离与受限启动辅助入口。应用代码从不在宿主 root 身份执行。

use super::safe_fs::Directory;
use super::{PlatformError, PlatformResult};
use crate::db::Db;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tgdrive_contract::{error_code, http::*, runtime::*};

#[cfg(test)]
pub(crate) static REAL_ISOLATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(crate) fn real_isolation() -> bool {
    #[cfg(test)]
    {
        return cfg!(target_os = "linux")
            && REAL_ISOLATION.load(std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(not(test))]
    {
        cfg!(target_os = "linux")
    }
}
pub(crate) fn available() -> bool {
    #[cfg(test)]
    if !real_isolation() {
        return true;
    }
    cfg!(target_os = "linux") && unsafe { libc::geteuid() } == 0
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub uid: u32,
    pub gid: u32,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LaunchConfig {
    pub context: BackendStartContext,
    pub executable: String,
    pub identity: Identity,
    pub cgroup: Option<String>,
}

pub(crate) async fn identity(db: &Db, id: &str) -> PlatformResult<Identity> {
    if !available() {
        return Err(PlatformError::new(
            error_code::CAPABILITY_UNAVAILABLE,
            "当前运行身份无法提供后端硬隔离",
        ));
    }
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let old: Option<(i64, i64)> =
        sqlx::query_as("SELECT uid,gid FROM app_runtime_identities WHERE app_id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let assigned = if let Some((uid, gid)) = old {
        Identity {
            uid: uid as u32,
            gid: gid as u32,
        }
    } else {
        let maximum: Option<i64> =
            sqlx::query_scalar("SELECT max(uid) FROM app_runtime_identities")
                .fetch_one(&mut *tx)
                .await?;
        let mut uid = maximum.unwrap_or(19999).saturating_add(1).max(20000) as u32;
        // 系统已有账号/组也不能借用；这里只读取 libc 数据库，不执行 shell 或应用指令。
        while unsafe { !libc::getpwuid(uid).is_null() || !libc::getgrgid(uid).is_null() } {
            uid = uid.checked_add(1).ok_or_else(|| {
                PlatformError::new(error_code::CAPABILITY_UNAVAILABLE, "后端系统身份已耗尽")
            })?;
        }
        sqlx::query(
            "INSERT INTO app_runtime_identities(app_id,uid,gid,created_at) VALUES(?,?,?,?)",
        )
        .bind(id)
        .bind(uid)
        .bind(uid)
        .bind(super::now().0)
        .execute(&mut *tx)
        .await?;
        Identity { uid, gid: uid }
    };
    if real_isolation() {
        register_user(assigned)?;
    }
    tx.commit().await?;
    if !real_isolation() {
        return Ok(Identity {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        });
    }
    Ok(assigned)
}
fn register_user(identity: Identity) -> PlatformResult<()> {
    use std::io::Write;
    let user = format!("tgapp{}", identity.uid);
    if unsafe { libc::getpwuid(identity.uid).is_null() } {
        let mut passwd = std::fs::OpenOptions::new()
            .append(true)
            .open("/etc/passwd")?;
        writeln!(
            passwd,
            "{user}:x:{}:{}:TGDrive application:/nonexistent:/usr/sbin/nologin",
            identity.uid, identity.gid
        )?;
        passwd.sync_all()?;
    }
    if unsafe { libc::getgrgid(identity.gid).is_null() } {
        let mut group = std::fs::OpenOptions::new()
            .append(true)
            .open("/etc/group")?;
        writeln!(group, "{user}:x:{}:", identity.gid)?;
        group.sync_all()?;
    }
    Ok(())
}

pub(crate) fn prepare_layout(
    data_root: &Path,
    id: &str,
    identity: Identity,
) -> PlatformResult<PathBuf> {
    let root_uid = unsafe { libc::geteuid() };
    let root_gid = unsafe { libc::getegid() };
    let root = Directory::open(data_root, true)?;
    root.permissions(root_uid, root_gid, 0o711)?;
    let app = root.child(id, true)?;
    app.permissions(
        root_uid,
        identity.gid,
        if real_isolation() { 0o550 } else { 0o700 },
    )?;
    let state = app.child_owner("state", true, Some(identity.uid))?;
    state.permissions(identity.uid, identity.gid, 0o700)?;
    let cache = app.child("cache", true)?;
    cache.permissions(
        root_uid,
        identity.gid,
        if real_isolation() { 0o710 } else { 0o700 },
    )?;
    cache
        .child("objects", true)?
        .permissions(root_uid, root_gid, 0o700)?;
    cache
        .child_owner("work", true, Some(identity.uid))?
        .permissions(identity.uid, identity.gid, 0o700)?;
    app.child_owner("tmp", true, Some(identity.uid))?
        .permissions(identity.uid, identity.gid, 0o700)?;
    let run = app.child("run", true)?;
    run.permissions(
        root_uid,
        identity.gid,
        if real_isolation() { 0o550 } else { 0o700 },
    )?;
    run.child_owner("backend", true, Some(identity.uid))?
        .permissions(identity.uid, identity.gid, 0o700)?;
    Ok(data_root.join(id))
}

pub(crate) fn disk_usage(data_dir: &Path, identity: Identity) -> PlatformResult<(u64, u64)> {
    if !data_dir.exists() {
        return Ok((0, 0));
    }
    let root = Directory::open(data_dir, false)?;
    let usage = |name: &str| -> PlatformResult<u64> {
        match root.child_owner(name, false, Some(identity.uid)) {
            Ok(directory) => directory.usage(Some(identity.uid)),
            Err(error) if error.code == error_code::NOT_FOUND => Ok(0),
            Err(error) => Err(error),
        }
    };
    Ok((
        usage("state")?,
        usage("cache")?.saturating_add(usage("tmp")?),
    ))
}

pub(crate) fn harden_host(db_path: &str) -> PlatformResult<()> {
    if !real_isolation() {
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(PlatformError::new(
            error_code::CAPABILITY_UNAVAILABLE,
            "应用后端要求宿主以 root 运行",
        ));
    }
    // 禁用 root 敏感进程转储；应用无 ptrace 能力，也没有宿主进程的附加组。
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
    let path = Path::new(db_path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // 只让应用穿过管理根，不让它列目录；数据库与辅助文件仍只允许 root 读写。
    Directory::open(parent, true)?.permissions(0, 0, 0o711)?;
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{db_path}{suffix}"));
        if file.exists() {
            let meta = std::fs::symlink_metadata(&file)?;
            if !meta.is_file() || meta.nlink() != 1 {
                return Err(PlatformError::new(
                    error_code::INVALID_PATH,
                    "宿主数据库文件类型无效",
                ));
            }
            let fd = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&file)?;
            use std::os::fd::AsRawFd;
            if unsafe { libc::fchown(fd.as_raw_fd(), 0, 0) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            fd.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    for temporary in ["/tmp", "/var/tmp", "/dev/shm"] {
        if Path::new(temporary).exists() {
            if Path::new("/.dockerenv").exists() {
                std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o555))?;
            } else if std::fs::metadata(temporary)?.mode() & 0o022 != 0 {
                return Err(PlatformError::new(
                    error_code::CAPABILITY_UNAVAILABLE,
                    "独立部署须先隔离共享临时目录；宿主不会更改系统共享目录权限",
                ));
            }
        }
    }
    Ok(())
}
use std::os::unix::fs::OpenOptionsExt;

pub(crate) fn write_launch(data_dir: &Path, config: &LaunchConfig) -> PlatformResult<PathBuf> {
    for path in [&config.context.host_socket, &config.context.listen_socket] {
        if path.len() as u64 > MAX_UNIX_SOCKET_PATH_BYTES {
            return Err(PlatformError::new(
                error_code::CAPABILITY_UNAVAILABLE,
                "应用套接字路径超过系统上限",
            ));
        }
    }
    let run = Directory::open(&data_dir.join("run"), false)?;
    run.write_atomic("launch.json", &serde_json::to_vec(config)?)?;
    Ok(data_dir.join("run/launch.json"))
}

/// 生产辅助进程在创建 Tokio/线程/数据库连接前调用，最终 exec 的 argv 严格遵守契约。
pub(crate) fn launch_helper(path: &Path) -> anyhow::Result<std::convert::Infallible> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.uid() == 0 && metadata.is_file() && metadata.nlink() == 1,
        "启动辅助配置不可信"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JSON_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_JSON_REQUEST_BYTES,
        "启动辅助配置过长"
    );
    let mut config: LaunchConfig =
        serde_json::from_value(tgdrive_contract::validation::parse_unique_json(&bytes)?)?;
    anyhow::ensure!(
        config.identity.uid > 0 && config.identity.gid > 0 && available(),
        "不能启动非隔离身份"
    );
    let mut enforced = BackendLimitEnforcement {
        memory: false,
        cpu: false,
        pids: false,
        open_files: false,
    };
    unsafe {
        let core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        anyhow::ensure!(
            libc::setrlimit(libc::RLIMIT_CORE, &core) == 0,
            "不能禁用进程转储"
        );
        let fds = libc::rlimit {
            rlim_cur: config.context.limits.open_files as libc::rlim_t,
            rlim_max: config.context.limits.open_files as libc::rlim_t,
        };
        enforced.open_files = libc::setrlimit(libc::RLIMIT_NOFILE, &fds) == 0;
        let pids = libc::rlimit {
            rlim_cur: config.context.limits.pids as libc::rlim_t,
            rlim_max: config.context.limits.pids as libc::rlim_t,
        };
        enforced.pids = libc::setrlimit(libc::RLIMIT_NPROC, &pids) == 0;
    }
    if let Some(cgroup) = &config.cgroup {
        // 在管理父组启用控制器；失败只是尽力而为限制不可用，不阻止硬隔离启动。
        if let Some(uid_group) = Path::new(cgroup).parent()
            && let Some(root) = uid_group.parent()
        {
            let _ = std::fs::create_dir_all(uid_group);
            let _ = std::fs::write(root.join("cgroup.subtree_control"), "+memory +cpu");
            let _ = std::fs::write(uid_group.join("cgroup.subtree_control"), "+memory +cpu");
        }
        if std::fs::create_dir_all(cgroup).is_ok()
            && std::fs::write(
                Path::new(cgroup).join("cgroup.procs"),
                std::process::id().to_string(),
            )
            .is_ok()
        {
            enforced.memory = std::fs::write(
                Path::new(cgroup).join("memory.max"),
                config.context.limits.memory_bytes.0.to_string(),
            )
            .is_ok();
            let quota = u64::from(config.context.limits.cpu_millis) * 100;
            enforced.cpu =
                std::fs::write(Path::new(cgroup).join("cpu.max"), format!("{quota} 100000"))
                    .is_ok();
        }
    }
    if !enforced.memory || !enforced.cpu || !enforced.pids || !enforced.open_files {
        eprintln!(
            "后端部分资源限制未强制：memory={} cpu={} pids={} open_files={}",
            enforced.memory, enforced.cpu, enforced.pids, enforced.open_files
        );
    }
    config.context.limits.enforced = enforced;
    let run = Directory::open(&Path::new(&config.context.data_dir).join("run"), false)?;
    run.write_atomic("context.json", &serde_json::to_vec(&config.context)?)?;
    run.file_permissions("context.json", 0, config.identity.gid, 0o440)?;
    #[cfg(target_os = "linux")]
    unsafe {
        anyhow::ensure!(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0,
            "不能禁止权限提升"
        );
        // 清空 ambient capabilities；setuid 随后清空 permitted/effective，再显式核对空集。
        anyhow::ensure!(
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0
            ) == 0,
            "不能清空环境 capabilities"
        );
    }
    unsafe {
        anyhow::ensure!(
            libc::setgroups(0, std::ptr::null()) == 0
                && libc::setgid(config.identity.gid) == 0
                && libc::setuid(config.identity.uid) == 0,
            "不能降权或清空附加组"
        );
        #[cfg(target_os = "linux")]
        {
            clear_capabilities()?;
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        }
    }
    let home = Path::new(&config.context.data_dir).join("state");
    let temporary = Path::new(&config.context.temp_dir);
    let error = std::process::Command::new(&config.executable)
        .arg(CONTEXT_ARGUMENT)
        .arg(Path::new(&config.context.data_dir).join("run/context.json"))
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", home)
        .env("TMPDIR", temporary)
        .current_dir(&config.context.data_dir)
        .exec();
    Err(error.into())
}

#[cfg(target_os = "linux")]
fn clear_capabilities() -> anyhow::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [0u32; 6];
    anyhow::ensure!(
        unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } == 0,
        "不能清空进程 capabilities"
    );
    Ok(())
}

pub(crate) fn cgroup_path(uid: u32, run: &str) -> Option<String> {
    let root = crate::config::env_opt("TGDRIVE_APP_CGROUP_ROOT")
        .unwrap_or_else(|| "/sys/fs/cgroup/tgdrive-apps".into());
    if !real_isolation() {
        return None;
    }
    Some(format!("{root}/{uid}/{run}"))
}
pub(crate) fn kill_group(pid: u32, identity: Identity, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
    #[cfg(target_os = "linux")]
    if real_isolation() {
        // UID 不与其他保留数据应用共享；进程组之外的恶意脱离子进程也一并撤销。
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(process) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                else {
                    continue;
                };
                if process == std::process::id() as i32 {
                    continue;
                }
                let uid = std::fs::read_to_string(entry.path().join("status"))
                    .ok()
                    .and_then(|status| {
                        status.lines().find_map(|line| {
                            line.strip_prefix("Uid:")
                                .and_then(|line| line.split_whitespace().next())
                                .and_then(|uid| uid.parse::<u32>().ok())
                        })
                    });
                if uid == Some(identity.uid) {
                    unsafe {
                        libc::kill(process, signal);
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = identity;
}
