//! 自建 Bot API 通过受认证的内部 SOCKS5 入口复用 tgdrive 的代理。
//! proxychains 只负责把 Bot API 的 TCP connect 送到这里，不持有上游代理凭据。

use super::{CONNECT_TIMEOUT, ProxyClient, ProxyConfig, connect_tunnel};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

const USERNAME: &[u8] = b"tgdrive";
const MAX_CONNECTIONS: usize = 256;

pub struct Bridge {
    task: JoinHandle<()>,
}

impl Bridge {
    /// 仅 Compose 开启；原生部署或使用其他 Bot API 时不开放监听端口。
    pub async fn from_env(network: Arc<ProxyClient>) -> anyhow::Result<Option<Self>> {
        let Some(listen) = crate::config::env_opt("TGDRIVE_BOT_API_PROXY_LISTEN") else {
            return Ok(None);
        };
        let config = crate::config::env_opt("TGDRIVE_BOT_API_PROXY_CONFIG")
            .ok_or_else(|| anyhow::anyhow!("缺少 TGDRIVE_BOT_API_PROXY_CONFIG"))?;
        let listener = TcpListener::bind(listen).await?;
        let port = listener.local_addr()?.port();
        let token = crate::auth::random_secret();
        let bridge = Self::start(listener, network, token.clone());
        // 监听建立后再原子发布。每次启动更换入口密钥，supervisor 自动重启 Bot API。
        write_config(Path::new(&config), port, &token)?;
        tracing::info!("自建 Bot API 代理入口已就绪，共用后台网络代理设置");
        Ok(Some(bridge))
    }

    fn start(listener: TcpListener, network: Arc<ProxyClient>, token: String) -> Self {
        let mut changes = network.subscribe();
        let token: Arc<str> = token.into();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    change = changes.changed() => {
                        if change.is_err() { break; }
                        // TDLib 会长期复用 DC 连接。断开后由它重连，确保开关/地址真正生效。
                        connections.abort_all();
                        tracing::info!("代理设置已更新，自建 Bot API 正在重新连接 Telegram");
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                    accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                        let (stream, _) = match accepted {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                tracing::error!("自建 Bot API 代理入口停止: {error}");
                                break;
                            }
                        };
                        let config = network.current().config.clone();
                        let token = token.clone();
                        connections.spawn(async move {
                            if let Err(error) = serve_connection(stream, &token, &config).await {
                                tracing::debug!("自建 Bot API 代理连接结束: {error}");
                            }
                        });
                    }
                }
            }
        });
        Self { task }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn write_config(path: &Path, port: u16, token: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    // 该文件只含随机的容器间凭据，不复制上游代理的地址、用户名或密码。
    // proxychains 默认在 Bot API 容器解析域名；Telegram DC 连接本身使用 IP。
    write!(
        file,
        "strict_chain\nquiet_mode\ntcp_read_time_out 30000\ntcp_connect_time_out 30000\n[ProxyList]\nsocks5 tgdrive {port} tgdrive {token}\n"
    )?;
    file.as_file().sync_all()?;
    // Bot API 启动器是 root，读取 0600 文件后再由 Bot API 降权。
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

async fn serve_connection(
    mut local: TcpStream,
    token: &str,
    config: &ProxyConfig,
) -> io::Result<()> {
    let destination =
        tokio::time::timeout(CONNECT_TIMEOUT, authenticate_and_request(&mut local, token))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "内部代理认证超时"))??;
    let result = tokio::time::timeout(CONNECT_TIMEOUT, async {
        if config.enabled {
            let url = reqwest::Url::parse(config.effective_url())
                .map_err(|_| io::Error::other("代理地址无效"))?;
            connect_tunnel(&url, destination).await
        } else {
            TcpStream::connect(destination).await
        }
    })
    .await;
    let mut remote = match result {
        Ok(Ok(remote)) => remote,
        _ => {
            // 代理失败绝不改为直连。不要把上游错误/认证信息回传到 Bot API 日志。
            local.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(io::Error::other("Telegram 出站连接失败，请检查网络代理"));
        }
    };
    local.set_nodelay(true)?;
    remote.set_nodelay(true)?;
    local.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tracing::debug!(%destination, proxy_enabled = config.enabled, "自建 Bot API 出站连接已建立");
    tokio::io::copy_bidirectional(&mut local, &mut remote).await?;
    Ok(())
}

async fn read_field(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let len = stream.read_u8().await? as usize;
    let mut value = vec![0; len];
    stream.read_exact(&mut value).await?;
    Ok(value)
}

async fn authenticate_and_request(stream: &mut TcpStream, token: &str) -> io::Result<SocketAddr> {
    if stream.read_u8().await? != 5 {
        return Err(io::Error::other("内部代理协议无效"));
    }
    if !read_field(stream).await?.contains(&2) {
        stream.write_all(&[5, 0xff]).await?;
        return Err(io::Error::other("内部代理必须认证"));
    }
    stream.write_all(&[5, 2]).await?;
    if stream.read_u8().await? != 1 {
        return Err(io::Error::other("内部代理认证协议无效"));
    }
    let username = read_field(stream).await?;
    let password = read_field(stream).await?;
    if username != USERNAME || !bool::from(password.as_slice().ct_eq(token.as_bytes())) {
        stream.write_all(&[1, 1]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "内部代理认证失败",
        ));
    }
    stream.write_all(&[1, 0]).await?;
    let mut request = [0; 4];
    stream.read_exact(&mut request).await?;
    if request[..3] != [5, 1, 0] {
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(io::Error::other("内部代理仅支持 TCP CONNECT"));
    }
    let ip = match request[3] {
        1 => {
            let mut ip = [0; 4];
            stream.read_exact(&mut ip).await?;
            IpAddr::V4(Ipv4Addr::from(ip))
        }
        4 => {
            let mut ip = [0; 16];
            stream.read_exact(&mut ip).await?;
            IpAddr::V6(Ipv6Addr::from(ip))
        }
        _ => {
            stream.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(io::Error::other("内部代理要求已解析的目标 IP"));
        }
    };
    let port = stream.read_u16().await?;
    if port == 0 || ip.is_unspecified() || ip.is_multicast() {
        return Err(io::Error::other("内部代理目标无效"));
    }
    Ok(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests;
