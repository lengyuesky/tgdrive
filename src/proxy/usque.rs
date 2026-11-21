//! 内置 usque 的注册、进程托管与状态。仅监听回环地址，不需要 TUN 或特权。

use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

static SERVICE: Mutex<Weak<Service>> = Mutex::new(Weak::new());
static REGISTER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const PORT: u16 = 1083;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Transport {
    // 一些网络允许 QUIC 握手，但不能可靠传输其 DATAGRAM；TCP 更兼容。
    #[default]
    Http2,
    Quic,
}

impl Transport {
    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "http2" => Ok(Self::Http2),
            "quic" => Ok(Self::Quic),
            _ => Err(AppError::BadRequest(
                "TGDRIVE_USQUE_TRANSPORT 须为 http2 或 quic".into(),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Http2 => "HTTP/2 / TCP/443",
            Self::Quic => "QUIC / UDP/443",
        }
    }

    fn configure(self, command: &mut Command) {
        if self == Self::Http2 {
            command.arg("--http2");
        }
    }
}

fn transport() -> AppResult<Transport> {
    crate::config::env_opt("TGDRIVE_USQUE_TRANSPORT")
        .map(|value| Transport::parse(&value))
        .unwrap_or(Ok(Transport::default()))
}

pub fn transport_label() -> &'static str {
    transport()
        .map(Transport::label)
        .unwrap_or("传输方式配置无效")
}

pub fn proxy_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| format!("socks5h://tgdrive:{}@127.0.0.1:{PORT}", password()))
}

fn password() -> &'static str {
    static PASSWORD: OnceLock<String> = OnceLock::new();
    PASSWORD.get_or_init(crate::auth::random_secret)
}

fn binary() -> PathBuf {
    std::env::var_os("TGDRIVE_USQUE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/usque"))
}

fn config_path() -> PathBuf {
    let db = std::env::var_os("TGDRIVE_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/tgdrive.db"));
    db.parent()
        .unwrap_or(Path::new("."))
        .join("usque/config.json")
}

fn command(bin: &Path, config: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // 注册和隧道必须直连，避免继承系统代理造成循环。
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .kill_on_drop(true);
    cmd
}

fn valid_config(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    [
        "private_key",
        "endpoint_pub_key",
        "endpoint_v4",
        "ipv4",
        "ipv6",
    ]
    .iter()
    .all(|key| value[key].as_str().is_some_and(|v| !v.is_empty()))
}

#[derive(Clone, serde::Serialize)]
pub struct Status {
    pub phase: &'static str,
    pub message: String,
    pub registered: bool,
}

fn status(phase: &'static str, message: &str, registered: bool) -> Status {
    Status {
        phase,
        message: message.into(),
        registered,
    }
}

pub fn current_status() -> Status {
    if let Some(service) = SERVICE.lock().unwrap().upgrade() {
        return service.state.borrow().clone();
    }
    if valid_config(&config_path()) {
        status("stopped", "已初始化，启用 usque 后自动启动", true)
    } else {
        status("unregistered", "尚未初始化，请先注册 WARP 设备", false)
    }
}

/// 只有用户点击明确接受条款的初始化按钮才调用；不改变当前代理选择。
pub async fn register(accept_tos: bool) -> AppResult<Status> {
    if !accept_tos {
        return Err(AppError::BadRequest(
            "初始化 usque 需要先同意 Cloudflare 服务条款".into(),
        ));
    }
    let _lock = REGISTER.lock().await;
    let config = config_path();
    if valid_config(&config) {
        return Ok(current_status());
    }
    if config.exists() {
        return Err(AppError::BadRequest(
            "usque 配置损坏，请检查数据目录中的 usque/config.json；原文件未覆盖".into(),
        ));
    }
    let directory = config.parent().unwrap();
    tokio::fs::create_dir_all(directory).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    // 注册失败或请求取消时不留下半份配置，也不覆盖用户导入的配置。
    let temporary = tempfile::tempdir_in(directory)?;
    let pending = temporary.path().join("config.json");
    let mut child = command(&binary(), &pending)
        .args(["register", "--accept-tos", "--name", "tgdrive"])
        .spawn()
        .map_err(|_| AppError::BadRequest("无法启动 usque，请使用包含 usque 的最新镜像".into()))?;
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .map_err(|_| {
            AppError::BadRequest("usque 注册超时，请检查到 Cloudflare 的网络后重试".into())
        })??;
    if !result.success() || !valid_config(&pending) {
        return Err(AppError::BadRequest(
            "usque 注册失败，可能无法连接 Cloudflare 或受到限流，请稍后重试".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).await?;
    }
    // hard_link 不会替换并发导入的文件，且源与目标位于同一文件系统。
    tokio::fs::hard_link(&pending, &config).await?;
    tracing::info!("usque WARP 设备初始化完成");
    Ok(status(
        "stopped",
        "初始化完成，选择 usque 并保存即可使用",
        true,
    ))
}

pub struct Service {
    state: watch::Receiver<Status>,
    task: JoinHandle<()>,
}

impl Service {
    pub fn acquire() -> Arc<Self> {
        let mut weak = SERVICE.lock().unwrap();
        if let Some(service) = weak.upgrade() {
            return service;
        }
        let service = Self::start(binary(), config_path(), PORT, password().into());
        *weak = Arc::downgrade(&service);
        service
    }

    fn start(bin: PathBuf, config: PathBuf, port: u16, password: String) -> Arc<Self> {
        let (tx, state) =
            watch::channel(status("starting", "usque 正在启动", valid_config(&config)));
        let task = tokio::spawn(async move {
            loop {
                let result = run(&bin, &config, port, &password, &tx).await;
                let message = match result {
                    Ok(()) => "usque 进程已退出，稍后自动重启".to_string(),
                    Err(error) => error.to_string(),
                };
                tracing::warn!("{message}");
                tx.send_replace(status("error", &message, valid_config(&config)));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        Arc::new(Self { state, task })
    }

    pub async fn ready(&self) -> AppResult<()> {
        let mut state = self.state.clone();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let current = state.borrow().clone();
                match current.phase {
                    "running" => return Ok(()),
                    "error" => return Err(AppError::BadRequest(current.message)),
                    _ => {}
                }
                state
                    .changed()
                    .await
                    .map_err(|_| AppError::BadRequest("usque 已停止".into()))?;
            }
        })
        .await
        .map_err(|_| AppError::BadRequest("usque 启动超时，请查看代理状态".into()))?
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    bin: &Path,
    config: &Path,
    port: u16,
    password: &str,
    tx: &watch::Sender<Status>,
) -> AppResult<()> {
    if !valid_config(config) {
        return Err(AppError::BadRequest(
            "usque 尚未初始化或配置无效，请先在网络代理设置中初始化".into(),
        ));
    }
    let transport = transport()?;
    tx.send_replace(status(
        "starting",
        &format!("usque 正在启动（{}）", transport.label()),
        true,
    ));
    let mut cmd = command(bin, config);
    cmd.args([
        "socks",
        "--bind",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--username",
        "tgdrive",
        "--password",
        password,
        "--always-reconnect",
    ]);
    transport.configure(&mut cmd);
    // 只识别固定的隧道状态标记，不输出原始日志，以免暴露配置或代理凭据。
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|_| AppError::BadRequest("无法启动 usque，请检查镜像版本及程序执行权限".into()))?;
    wait_listener(&mut child, port, password).await?;
    tx.send_replace(status(
        "starting",
        &format!(
            "usque 本地代理已启动，正在建立 WARP 隧道（{}）",
            transport.label()
        ),
        true,
    ));
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::BadRequest("无法读取 usque 隧道状态".into()))?;
    let mut lines = BufReader::new(stderr).lines();
    loop {
        tokio::select! {
            result = child.wait() => { result?; break; }
            line = lines.next_line() => {
                let Some(line) = line? else { child.wait().await?; break; };
                if let Some((phase, message)) = tunnel_event(&line) {
                    tracing::info!("{message}（{}）", transport.label());
                    tx.send_replace(status(phase, &format!("{message}（{}）", transport.label()), true));
                }
            }
        }
    }
    Ok(())
}

fn tunnel_event(line: &str) -> Option<(&'static str, &'static str)> {
    if line.contains("Connected to MASQUE server") {
        Some((
            "running",
            "usque WARP 隧道已建立，可点击「测试连接」验证网络",
        ))
    } else if line.contains("Tunnel connection lost:")
        || line.contains("Failed to connect tunnel:")
        || line.contains("Tunnel connection failed:")
    {
        Some(("starting", "usque WARP 隧道连接失败，正在自动重连"))
    } else {
        None
    }
}

async fn wait_listener(child: &mut Child, port: u16, password: &str) -> AppResult<()> {
    for _ in 0..40 {
        if child.try_wait()?.is_some() {
            return Err(AppError::BadRequest(
                "usque 启动失败，请检查注册配置或端口 1083 是否被占用".into(),
            ));
        }
        if tokio::time::timeout(Duration::from_millis(200), async {
            let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
            stream.write_all(&[5, 1, 2]).await?;
            let mut reply = [0; 2];
            stream.read_exact(&mut reply).await?;
            if reply != [5, 2] {
                return Err(std::io::Error::other("认证方式无效"));
            }
            let mut auth = vec![1, 7];
            auth.extend_from_slice(b"tgdrive");
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await?;
            stream.read_exact(&mut reply).await?;
            if reply != [1, 0] {
                return Err(std::io::Error::other("认证失败"));
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(AppError::BadRequest("usque 本地代理启动超时".into()))
}

#[cfg(test)]
mod tests;
