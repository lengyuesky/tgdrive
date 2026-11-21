//! 以应用 UID 执行 SQLite 在线备份；root 只接收有界管道，不打开应用可控数据库路径。
use super::backend_system::{self as system, Identity};
use super::{PlatformError, PlatformResult};
use crate::db::Db;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tgdrive_contract::{error_code, http::MAX_JSON_REQUEST_BYTES};

const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Config {
    identity: Identity,
    data_dir: PathBuf,
    max_bytes: u64,
}
struct HelperGroup {
    pid: u32,
    finished: bool,
}
impl Drop for HelperGroup {
    fn drop(&mut self) {
        if !self.finished {
            // 快照失败只终止这一辅助进程组，绝不能按 UID 回收仍在运行的应用后端。
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
    }
}
pub(crate) async fn snapshot(
    db: &Db,
    app: &str,
    data_dir: &Path,
    destination: &Path,
) -> PlatformResult<bool> {
    let identity = system::identity(db, app).await?;
    system::prepare_layout(
        data_dir
            .parent()
            .ok_or_else(|| PlatformError::new(error_code::INVALID_PATH, "应用数据根无效"))?,
        app,
        identity,
    )?;
    let temp = tempfile::tempdir()?;
    let config = temp.path().join("snapshot.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&Config {
            identity,
            data_dir: data_dir.into(),
            max_bytes: MAX_BYTES,
        })?,
    )?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))?;
    let parent = destination
        .parent()
        .ok_or_else(|| PlatformError::new(error_code::INVALID_PATH, "快照输出目录无效"))?;
    let output = tempfile::NamedTempFile::new_in(parent)?;
    let mut command = tokio::process::Command::new(system::helper_executable()?);
    command.env_clear();
    if system::real_isolation() || !cfg!(test) {
        command.arg("internal-app-backup").arg(&config);
    }
    #[cfg(test)]
    if !system::real_isolation() {
        command
            .args([
                "--ignored",
                "--exact",
                "apps::snapshot::fixture_snapshot_helper",
                "--nocapture",
            ])
            .env("TGDRIVE_TEST_SNAPSHOT_CONFIG", &config);
    }
    command
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", data_dir.join("state"))
        .env("TMPDIR", data_dir.join("tmp"))
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut group = HelperGroup {
        pid: child
            .id()
            .ok_or_else(|| PlatformError::new(error_code::INTERNAL_ERROR, "备份辅助进程未创建"))?,
        finished: false,
    };
    let stdout = child.stdout.take().expect("已配置快照管道");
    let stderr = child.stderr.take().expect("已配置诊断管道");
    let mut diagnostics = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut bytes).await;
        bytes
    });
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut file = tokio::fs::File::from_std(output.reopen()?);
        #[cfg(test)]
        let mut stdout = stdout;
        #[cfg(test)]
        if !system::real_isolation() {
            let mut prefix = Vec::new();
            while !prefix.ends_with(b"TGDRIVE_SNAPSHOT_START\n") {
                prefix.push(stdout.read_u8().await?);
                if prefix.len() > 16 * 1024 {
                    return Err(PlatformError::new(
                        error_code::INTERNAL_ERROR,
                        "测试辅助帧无效",
                    ));
                }
            }
        }
        let bytes = tokio::io::copy(&mut stdout.take(MAX_BYTES + 1), &mut file).await?;
        if bytes > MAX_BYTES {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "应用快照超过上限",
            ));
        }
        file.flush().await?;
        file.sync_all().await?;
        let status = child.wait().await?;
        group.finished = true;
        if status.code() == Some(3) {
            return Ok(false);
        }
        if !status.success() {
            return Err(PlatformError::new(
                error_code::STORAGE_UNAVAILABLE,
                "应用在线快照失败，旧备份保持不变",
            ));
        }
        super::archive::validate_application_database(output.path()).await?;
        Ok(true)
    })
    .await
    .unwrap_or_else(|_| {
        Err(PlatformError::new(
            error_code::REQUEST_TIMEOUT,
            "应用在线快照超时",
        ))
    });
    if !group.finished {
        unsafe {
            libc::kill(-(group.pid as i32), libc::SIGKILL);
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        group.finished = true;
    }
    let diagnostic = match tokio::time::timeout(Duration::from_secs(5), &mut diagnostics).await {
        Ok(result) => result.unwrap_or_default(),
        Err(_) => {
            diagnostics.abort();
            Vec::new()
        }
    };
    if result.is_err() {
        tracing::warn!(app_id = app, "应用快照辅助进程失败，保留上次成功备份");
    }
    #[cfg(test)]
    if !diagnostic.is_empty() {
        eprintln!("{}", String::from_utf8_lossy(&diagnostic));
    }
    #[cfg(not(test))]
    let _ = diagnostic;
    if matches!(result, Ok(true)) {
        if let Ok(metadata) = std::fs::symlink_metadata(destination)
            && (!metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() })
        {
            return Err(PlatformError::new(
                error_code::INVALID_PATH,
                "已有快照输出类型或身份无效",
            ));
        }
        output
            .persist(destination)
            .map_err(|error| PlatformError::from(error.error))?;
        std::fs::File::open(parent)?.sync_all()?;
    }
    result
}

pub(crate) fn helper(path: &Path) -> anyhow::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "辅助配置身份无效"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JSON_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_JSON_REQUEST_BYTES,
        "辅助配置超过上限"
    );
    let config: Config =
        serde_json::from_value(tgdrive_contract::validation::parse_unique_json(&bytes)?)?;
    if system::real_isolation() {
        system::drop_identity(config.identity)?;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        anyhow::ensure!(
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0,
            "不能禁用辅助进程转储"
        );
    }
    #[cfg(test)]
    {
        use std::io::Write;
        std::io::stdout().write_all(b"TGDRIVE_SNAPSHOT_START\n")?;
        std::io::stdout().flush()?;
    }
    eprintln!(
        "应用快照辅助进程 UID={} GID={}",
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() }
    );
    let data_dir = std::fs::canonicalize(&config.data_dir)?;
    let source = data_dir.join(tgdrive_contract::manifest::DATABASE_RELATIVE_PATH);
    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "应用数据库类型、链接数或所有者无效"
    );
    let input = Database::open(
        &source,
        libsqlite3_sys::SQLITE_OPEN_READONLY | libsqlite3_sys::SQLITE_OPEN_NOFOLLOW,
    )?;
    input.exec("PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-2048; PRAGMA query_only=ON;")?;
    let mut target = tempfile::Builder::new()
        .prefix("snapshot-")
        .tempfile_in(data_dir.join("tmp"))?;
    let output = Database::open(
        target.path(),
        libsqlite3_sys::SQLITE_OPEN_READWRITE | libsqlite3_sys::SQLITE_OPEN_NOFOLLOW,
    )?;
    output.exec("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-2048;")?;
    let main = CString::new("main")?;
    let backup = unsafe {
        libsqlite3_sys::sqlite3_backup_init(output.0, main.as_ptr(), input.0, main.as_ptr())
    };
    anyhow::ensure!(!backup.is_null(), "不能开始在线快照");
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut result = Ok(());
    loop {
        let code = unsafe { libsqlite3_sys::sqlite3_backup_step(backup, 64) };
        if code == libsqlite3_sys::SQLITE_DONE {
            break;
        }
        if !matches!(
            code,
            libsqlite3_sys::SQLITE_OK | libsqlite3_sys::SQLITE_BUSY | libsqlite3_sys::SQLITE_LOCKED
        ) || Instant::now() >= deadline
        {
            result = Err(anyhow::anyhow!("在线快照未能完成"));
            break;
        }
        if code != libsqlite3_sys::SQLITE_OK {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let finished = unsafe { libsqlite3_sys::sqlite3_backup_finish(backup) };
    result?;
    anyhow::ensure!(finished == libsqlite3_sys::SQLITE_OK, "在线快照提交失败");
    drop(output);
    drop(input);
    // SQLite 写入完成后取消临时名字，随后只通过持有的 FD 流回宿主。
    std::fs::remove_file(target.path())?;
    target.as_file_mut().seek(SeekFrom::Start(0))?;
    anyhow::ensure!(
        target.as_file().metadata()?.len() <= config.max_bytes,
        "应用快照超过上限"
    );
    std::io::copy(target.as_file_mut(), &mut std::io::stdout().lock())?;
    Ok(true)
}
struct Database(*mut libsqlite3_sys::sqlite3);
impl Database {
    fn open(path: &Path, flags: i32) -> anyhow::Result<Self> {
        let path = CString::new(path.as_os_str().as_encoded_bytes())?;
        let mut database = std::ptr::null_mut();
        let code = unsafe {
            libsqlite3_sys::sqlite3_open_v2(path.as_ptr(), &mut database, flags, std::ptr::null())
        };
        let database = Self(database);
        anyhow::ensure!(code == libsqlite3_sys::SQLITE_OK, "SQLite 快照文件打不开");
        Ok(database)
    }
    fn exec(&self, sql: &str) -> anyhow::Result<()> {
        let sql = CString::new(sql)?;
        let code = unsafe {
            libsqlite3_sys::sqlite3_exec(
                self.0,
                sql.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        anyhow::ensure!(code == libsqlite3_sys::SQLITE_OK, "SQLite 快照配置失败");
        Ok(())
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        unsafe {
            libsqlite3_sys::sqlite3_close(self.0);
        }
    }
}

#[cfg(test)]
#[test]
#[ignore = "仅供在线快照集成测试派生辅助进程"]
fn fixture_snapshot_helper() {
    let Some(path) = std::env::var_os("TGDRIVE_TEST_SNAPSHOT_CONFIG") else {
        return;
    };
    match helper(Path::new(&path)) {
        Ok(true) => std::process::exit(0),
        Ok(false) => std::process::exit(3),
        Err(error) => {
            eprintln!("测试在线快照失败：{error}");
            std::process::exit(1)
        }
    }
}
