//! 出站代理配置与热更新。HTTP 请求和 MTProto 隧道共用同一份配置。

use crate::error::{AppError, AppResult};
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

const REDACTED: &str = "***";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 容器内的 Bot API 和本机服务不能交给远端代理解析、访问。
const LOCAL_HOSTS: &str = "localhost,127.0.0.1,::1,telegram-bot-api";

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub url: String,
}

impl ProxyConfig {
    pub fn new(enabled: bool, url: &str) -> AppResult<Self> {
        let url = normalize_url(url)?;
        if enabled && url.is_empty() {
            return Err(AppError::BadRequest("启用代理时必须填写代理地址".into()));
        }
        Ok(Self { enabled, url })
    }

    /// 脱敏地址原样回传时保留凭据；编辑了地址则必须重新输入完整认证信息。
    pub fn patched(&self, enabled: Option<bool>, url: Option<&str>) -> AppResult<Self> {
        let url = match url {
            Some(value) if value.trim() != mask_url(&self.url) => value,
            _ => &self.url,
        };
        Self::new(enabled.unwrap_or(self.enabled), url)
    }

    pub fn http_builder(&self) -> AppResult<reqwest::ClientBuilder> {
        // 显式关闭系统代理，避免关闭网页开关后仍被 HTTP_PROXY / ALL_PROXY 接管。
        let mut builder = reqwest::Client::builder().no_proxy();
        if self.enabled {
            let proxy = reqwest::Proxy::all(&self.url)
                .map_err(|_| AppError::BadRequest("代理地址无效".into()))?
                .no_proxy(reqwest::NoProxy::from_string(LOCAL_HOSTS));
            builder = builder.proxy(proxy);
        }
        Ok(builder)
    }
}

fn normalize_url(raw: &str) -> AppResult<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let bad = |message: &str| AppError::BadRequest(message.into());
    if raw.len() > 4096 || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad(
            "代理地址过长或含空白字符；认证信息中的特殊字符请使用 URL 编码",
        ));
    }
    if !raw.contains("://") {
        return Err(bad("代理地址须以 http://、socks5:// 或 socks5h:// 开头"));
    }
    let mut url = reqwest::Url::parse(raw).map_err(|_| bad("代理地址格式无效"))?;
    if url.scheme() == "socks" {
        url.set_scheme("socks5h").map_err(|_| bad("代理协议无效"))?;
    }
    if !matches!(url.scheme(), "http" | "socks5" | "socks5h") {
        return Err(bad(
            "代理仅支持 HTTP 和 SOCKS5（http://、socks5://、socks5h://）",
        ));
    }
    if url.host_str().is_none_or(str::is_empty) || url.port() == Some(0) {
        return Err(bad("代理必须包含有效主机，端口范围为 1～65535"));
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(bad("代理地址不能包含路径、查询参数或片段"));
    }
    if url.scheme() != "http" && url.port().is_none() {
        url.set_port(Some(1080)).map_err(|_| bad("代理端口无效"))?;
    }
    let (username, password) = credentials(&url).map_err(|_| bad("代理认证信息的 URL 编码无效"))?;
    if username == REDACTED || password == REDACTED {
        return Err(bad(
            "代理认证信息已脱敏；修改地址时请重新输入完整的用户名和密码",
        ));
    }
    if username.is_empty() && url.password().is_some() {
        return Err(bad("填写代理密码时必须同时填写用户名"));
    }
    if url.scheme() != "http"
        && !username.is_empty()
        && (username.len() > 255 || password.is_empty() || password.len() > 255)
    {
        return Err(bad("SOCKS5 用户名和密码各需为 1～255 字节"));
    }
    Ok(url.to_string())
}

fn credentials(url: &reqwest::Url) -> io::Result<(String, String)> {
    let decode = |value: &str| {
        percent_decode_str(value)
            .decode_utf8()
            .map(|value| value.into_owned())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "代理认证信息编码无效"))
    };
    Ok((
        decode(url.username())?,
        decode(url.password().unwrap_or(""))?,
    ))
}

/// 不返回认证信息的任何前后缀，也不泄露密码长度。
pub fn mask_url(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "[已脱敏]".into();
    };
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username(REDACTED);
        let _ = url.set_password(Some(REDACTED));
    }
    url.to_string()
}

/// 先构造新客户端，再提交配置；构造失败不会留下半生效的数据库配置。
pub struct ProxySnapshot {
    pub config: ProxyConfig,
    pub http: reqwest::Client,
}

impl ProxySnapshot {
    pub fn new(config: ProxyConfig) -> AppResult<Arc<Self>> {
        let http = config
            .http_builder()?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(60))
            // 大分块上传、下载不设置整体超时。
            .build()
            .map_err(|_| AppError::BadRequest("构造代理 HTTP 客户端失败".into()))?;
        Ok(Arc::new(Self { config, http }))
    }
}

/// 历史目标和正在使用的 Bot 共享句柄，切换代理不重置限流状态和文件路径缓存。
pub struct ProxyClient(RwLock<Arc<ProxySnapshot>>);

impl ProxyClient {
    pub fn new(config: ProxyConfig) -> AppResult<Self> {
        Ok(Self(RwLock::new(ProxySnapshot::new(config)?)))
    }

    pub fn current(&self) -> Arc<ProxySnapshot> {
        self.0.read().unwrap().clone()
    }

    pub fn install(&self, snapshot: Arc<ProxySnapshot>) {
        *self.0.write().unwrap() = snapshot;
    }
}

/// grammers 没有通用连接器接口。每个 DC 使用仅监听回环地址、固定目标的转发器，
/// 让 HTTP CONNECT 和 SOCKS5 都能承载 MTProto；本地端口绝不写入持久会话。
pub struct DcTunnel {
    pub address: SocketAddrV4,
    task: JoinHandle<()>,
}

impl DcTunnel {
    pub fn new(config: &ProxyConfig, destination: SocketAddrV4) -> io::Result<Self> {
        let url = reqwest::Url::parse(&config.url)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "代理地址无效"))?;
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let SocketAddr::V4(address) = listener.local_addr()? else {
            return Err(io::Error::other("代理隧道回环地址无效"));
        };
        let listener = TcpListener::from_std(listener)?;
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept(), if connections.len() < 16 => {
                        let Ok((mut local, _)) = result else { break };
                        let url = url.clone();
                        connections.spawn(async move {
                            let result = tokio::time::timeout(
                                CONNECT_TIMEOUT,
                                connect_tunnel(&url, destination.into()),
                            ).await;
                            match result {
                                Ok(Ok(mut remote)) => {
                                    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                                }
                                Ok(Err(error)) => tracing::warn!("MTProto 代理连接失败: {error}"),
                                Err(_) => tracing::warn!("MTProto 代理连接超时"),
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { address, task })
    }
}

impl Drop for DcTunnel {
    fn drop(&mut self) {
        // 转发任务中的 JoinSet 也随之释放，不留下后台连接或监听端口。
        self.task.abort();
    }
}

async fn connect_tunnel(url: &reqwest::Url, destination: SocketAddr) -> io::Result<TcpStream> {
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::other("代理缺少主机"))?
        .trim_matches(['[', ']']);
    let port = url
        .port()
        .unwrap_or(if url.scheme() == "http" { 80 } else { 1080 });
    let mut stream = TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true)?;
    let (username, password) = credentials(url)?;
    match url.scheme() {
        "http" => {
            let mut request = format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n");
            if !username.is_empty() {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
            }
            request.push_str("\r\n");
            stream.write_all(request.as_bytes()).await?;
            // 不使用可能预读 MTProto 数据的缓冲器；响应头有严格的大小和时间上限。
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                if header.len() >= 8192 {
                    return Err(io::Error::other("HTTP 代理响应头过大"));
                }
                header.push(stream.read_u8().await?);
            }
            let first = header
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let line = std::str::from_utf8(first).unwrap_or_default();
            let mut fields = line.split_ascii_whitespace();
            let version = fields.next();
            let status = fields.next().and_then(|value| value.parse::<u16>().ok());
            if !matches!(version, Some("HTTP/1.0" | "HTTP/1.1"))
                || !matches!(status, Some(200..=299))
            {
                return Err(io::Error::other(if status == Some(407) {
                    "HTTP 代理认证失败，请检查用户名和密码"
                } else {
                    "HTTP 代理拒绝 CONNECT 隧道"
                }));
            }
        }
        "socks5" | "socks5h" => {
            let method = if username.is_empty() { 0 } else { 2 };
            stream.write_all(&[5, 1, method]).await?;
            let mut reply = [0u8; 2];
            stream.read_exact(&mut reply).await?;
            if reply != [5, method] {
                return Err(io::Error::other("SOCKS5 代理不支持所需的认证方式"));
            }
            if method == 2 {
                let mut auth = vec![1, username.len() as u8];
                auth.extend_from_slice(username.as_bytes());
                auth.push(password.len() as u8);
                auth.extend_from_slice(password.as_bytes());
                stream.write_all(&auth).await?;
                stream.read_exact(&mut reply).await?;
                if reply != [1, 0] {
                    return Err(io::Error::other("SOCKS5 代理认证失败，请检查用户名和密码"));
                }
            }
            let mut request = vec![5, 1, 0];
            match destination.ip() {
                std::net::IpAddr::V4(ip) => {
                    request.push(1);
                    request.extend_from_slice(&ip.octets());
                }
                std::net::IpAddr::V6(ip) => {
                    request.push(4);
                    request.extend_from_slice(&ip.octets());
                }
            }
            request.extend_from_slice(&destination.port().to_be_bytes());
            stream.write_all(&request).await?;
            let mut reply = [0u8; 4];
            stream.read_exact(&mut reply).await?;
            if reply[..3] != [5, 0, 0] {
                return Err(io::Error::other("SOCKS5 代理拒绝连接目标"));
            }
            let address_len = match reply[3] {
                1 => 4,
                4 => 16,
                3 => stream.read_u8().await? as usize,
                _ => return Err(io::Error::other("SOCKS5 代理响应地址类型无效")),
            };
            let mut address = vec![0u8; address_len + 2];
            stream.read_exact(&mut address).await?;
        }
        _ => return Err(io::Error::other("代理协议不受支持")),
    }
    Ok(stream)
}

#[cfg(test)]
pub(crate) mod tests;
