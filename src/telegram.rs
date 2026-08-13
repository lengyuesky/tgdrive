//! Telegram Bot API 轻量客户端:仅封装本项目需要的四个方法 + 分块字节流下载。
//!
//! 端点可切换:官方 api.telegram.org 或自部署 telegram-bot-api。
//! 自部署 --local 模式下 getFile 返回容器内绝对路径,此时若本地(共享卷)存在
//! 该文件则直接读盘,否则回退 HTTP。

use crate::error::{AppError, AppResult};
use bytes::Bytes;
use serde::Deserialize;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};

/// getFile 结果缓存时长(官方 file_path 约 1 小时失效,留余量)
const FILE_PATH_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_RETRIES: u32 = 5;

pub type ByteReader = Pin<Box<dyn AsyncRead + Send>>;

#[derive(Debug, Clone)]
pub struct TgMessage {
    pub message_id: i64,
    pub file_id: String,
}

/// 上传体:小分块驻内存,大分块经临时文件(重试时可重复读取)
pub enum UploadBody {
    Memory(Bytes),
    TempFile { file: tempfile::NamedTempFile, len: u64 },
}

impl UploadBody {
    pub fn len(&self) -> u64 {
        match self {
            UploadBody::Memory(b) => b.len() as u64,
            UploadBody::TempFile { len, .. } => *len,
        }
    }
}

#[async_trait::async_trait]
pub trait TgApi: Send + Sync + 'static {
    async fn send_document(
        &self,
        filename: &str,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage>;
    async fn delete_message(&self, message_id: i64) -> AppResult<()>;
    async fn get_me(&self) -> AppResult<String>;
    /// 打开某个分块的字节流,从块内 offset 开始读
    async fn open_chunk_stream(&self, file_id: &str, offset: u64) -> AppResult<ByteReader>;
}

// ---------- Bot API 响应结构 ----------

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
    parameters: Option<ApiParams>,
}

#[derive(Deserialize)]
struct ApiParams {
    retry_after: Option<u64>,
}

#[derive(Deserialize)]
struct RawMessage {
    message_id: i64,
    document: Option<RawDocument>,
}

#[derive(Deserialize)]
struct RawDocument {
    file_id: String,
}

#[derive(Deserialize)]
struct RawFile {
    file_path: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    username: Option<String>,
    first_name: Option<String>,
}

// ---------- 可热切换代理 ----------

/// 包一层可替换的实现:后台改了 token/chat_id/端点后,
/// 已被 Vfs / Uploader / DavHandler 克隆持有的 `Arc<dyn TgApi>` 无需重建即可指向新客户端。
pub struct SwitchableTg {
    inner: std::sync::RwLock<Arc<dyn TgApi>>,
}

impl SwitchableTg {
    pub fn new(inner: Arc<dyn TgApi>) -> Self {
        SwitchableTg { inner: std::sync::RwLock::new(inner) }
    }

    pub fn replace(&self, new: Arc<dyn TgApi>) {
        *self.inner.write().unwrap() = new;
    }

    /// 取出当前实现;guard 立即释放,不跨 await
    fn current(&self) -> Arc<dyn TgApi> {
        self.inner.read().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl TgApi for SwitchableTg {
    async fn send_document(
        &self,
        filename: &str,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        self.current().send_document(filename, caption, body).await
    }

    async fn delete_message(&self, message_id: i64) -> AppResult<()> {
        self.current().delete_message(message_id).await
    }

    async fn get_me(&self) -> AppResult<String> {
        self.current().get_me().await
    }

    async fn open_chunk_stream(&self, file_id: &str, offset: u64) -> AppResult<ByteReader> {
        self.current().open_chunk_stream(file_id, offset).await
    }
}

// ---------- 未配置占位 ----------

/// 还没填 Bot Token / 存储频道时装在 `SwitchableTg` 里的占位实现。
/// 这样上传、下载、WebDAV 各条路径都能拿到同一句可读的提示,而不是 Telegram 的 401。
pub struct UnconfiguredTg;

impl UnconfiguredTg {
    fn err<T>() -> AppResult<T> {
        Err(AppError::BadRequest(
            "还没配置 Telegram 存储:请到「系统设置」填写 Bot Token 与存储频道".into(),
        ))
    }
}

#[async_trait::async_trait]
impl TgApi for UnconfiguredTg {
    async fn send_document(
        &self,
        _filename: &str,
        _caption: Option<String>,
        _body: &UploadBody,
    ) -> AppResult<TgMessage> {
        Self::err()
    }

    async fn delete_message(&self, _message_id: i64) -> AppResult<()> {
        Self::err()
    }

    async fn get_me(&self) -> AppResult<String> {
        Self::err()
    }

    async fn open_chunk_stream(&self, _file_id: &str, _offset: u64) -> AppResult<ByteReader> {
        Self::err()
    }
}

// ---------- 真实客户端 ----------

pub struct TgClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
    /// file_id → (file_path, 缓存时间)
    path_cache: Mutex<HashMap<String, (String, Instant)>>,
}

impl TgClient {
    pub fn new(api_base: &str, token: &str, chat_id: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            // 上传/下载大分块耗时不可控,不设总超时
            .build()?;
        Ok(TgClient {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            chat_id: chat_id.to_string(),
            path_cache: Mutex::new(HashMap::new()),
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.token, method)
    }

    fn file_url(&self, file_path: &str) -> String {
        format!("{}/file/bot{}/{}", self.api_base, self.token, file_path)
    }

    /// 解析响应体;429/5xx 时返回需等待的秒数以便重试
    async fn parse<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, (Option<Duration>, AppError)> {
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            (Some(Duration::from_secs(2)), AppError::Telegram(format!("读取响应失败: {e}")))
        })?;
        let parsed: Result<ApiResponse<T>, _> = serde_json::from_slice(&body);
        match parsed {
            Ok(r) if r.ok => r.result.ok_or_else(|| {
                (None, AppError::Telegram("响应缺少 result".into()))
            }),
            Ok(r) => {
                let desc = r.description.unwrap_or_else(|| "未知错误".into());
                let retry = r
                    .parameters
                    .and_then(|p| p.retry_after)
                    .map(Duration::from_secs)
                    .or_else(|| {
                        // 5xx 无 retry_after 时按 2s 起步退避
                        (r.error_code.unwrap_or(0) >= 500).then(|| Duration::from_secs(2))
                    });
                Err((retry, AppError::Telegram(format!(
                    "[{}] {desc}",
                    r.error_code.unwrap_or(status.as_u16() as i64)
                ))))
            }
            Err(_) => {
                let retry = (status.is_server_error() || status.as_u16() == 429)
                    .then(|| Duration::from_secs(2));
                Err((retry, AppError::Telegram(format!(
                    "HTTP {status}: {}",
                    String::from_utf8_lossy(&body[..body.len().min(200)])
                ))))
            }
        }
    }

    /// 带重试执行一次 API 调用;`make_req` 每次重试重新构造请求
    async fn call_with_retry<T, F, Fut>(&self, what: &str, mut make_req: F) -> AppResult<T>
    where
        T: serde::de::DeserializeOwned,
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let outcome = match make_req().await {
                Ok(resp) => Self::parse::<T>(resp).await,
                Err(e) => Err((
                    Some(Duration::from_secs(2u64.pow(attempt.min(4)))),
                    AppError::Telegram(format!("{what} 网络错误: {e}")),
                )),
            };
            match outcome {
                Ok(v) => return Ok(v),
                Err((Some(wait), err)) if attempt < MAX_RETRIES => {
                    // flood 控制:叠加少量抖动,避免并发任务同时重试
                    let jitter = Duration::from_millis((attempt as u64 * 137) % 500);
                    tracing::warn!("{what} 第 {attempt} 次失败({err}),{:?} 后重试", wait + jitter);
                    tokio::time::sleep(wait + jitter).await;
                }
                Err((_, err)) => return Err(err),
            }
        }
    }

    async fn get_file_path(&self, file_id: &str) -> AppResult<String> {
        if let Some((path, at)) = self.path_cache.lock().unwrap().get(file_id) {
            if at.elapsed() < FILE_PATH_TTL {
                return Ok(path.clone());
            }
        }
        let url = self.method_url("getFile");
        let file: RawFile = self
            .call_with_retry("getFile", || {
                self.http.get(&url).query(&[("file_id", file_id)]).send()
            })
            .await?;
        let path = file.file_path.ok_or_else(|| {
            AppError::Telegram("getFile 未返回 file_path(官方 API 单文件下载上限 20MB)".into())
        })?;
        self.path_cache
            .lock()
            .unwrap()
            .insert(file_id.to_string(), (path.clone(), Instant::now()));
        Ok(path)
    }

    fn body_to_part(&self, filename: &str, body: &UploadBody) -> AppResult<reqwest::multipart::Part> {
        let part = match body {
            UploadBody::Memory(b) => reqwest::multipart::Part::stream_with_length(
                reqwest::Body::from(b.clone()),
                b.len() as u64,
            ),
            UploadBody::TempFile { file, len } => {
                // 每次(重试)重新打开临时文件,流式读取不占内存
                let std_file = file.reopen()?;
                let tokio_file = tokio::fs::File::from_std(std_file);
                let stream = tokio_util::io::ReaderStream::new(tokio_file);
                reqwest::multipart::Part::stream_with_length(
                    reqwest::Body::wrap_stream(stream),
                    *len,
                )
            }
        };
        Ok(part
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| AppError::Telegram(e.to_string()))?)
    }

    /// HTTP 下载分块;offset>0 时先试 Range,服务器不支持则丢弃前缀
    async fn open_http_stream(&self, file_path: &str, offset: u64) -> AppResult<ByteReader> {
        let url = self.file_url(file_path);
        let mut req = self.http.get(&url);
        if offset > 0 {
            req = req.header(http::header::RANGE, format!("bytes={offset}-"));
        }
        let resp = req.send().await.map_err(|e| AppError::Telegram(format!("下载失败: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Telegram(format!("下载分块 HTTP {status}")));
        }
        let got_range = status == http::StatusCode::PARTIAL_CONTENT;
        let stream = resp.bytes_stream();
        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            stream,
            std::io::Error::other,
        ));
        let mut reader: ByteReader = Box::pin(reader);
        if offset > 0 && !got_range {
            // 服务器忽略了 Range,读掉前缀字节
            let mut to_skip = offset;
            let mut buf = vec![0u8; 64 * 1024];
            while to_skip > 0 {
                let take = buf.len().min(to_skip as usize);
                let n = reader.read(&mut buf[..take]).await?;
                if n == 0 {
                    return Err(AppError::Telegram("分块比预期短,无法定位偏移".into()));
                }
                to_skip -= n as u64;
            }
        }
        Ok(reader)
    }
}

#[async_trait::async_trait]
impl TgApi for TgClient {
    async fn send_document(
        &self,
        filename: &str,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let url = self.method_url("sendDocument");
        let msg: RawMessage = self
            .call_with_retry("sendDocument", || {
                let mut form = reqwest::multipart::Form::new()
                    .text("chat_id", self.chat_id.clone())
                    .text("disable_notification", "true");
                if let Some(c) = &caption {
                    form = form.text("caption", c.clone());
                }
                // Part 构造失败会在 send 时报网络错误重试后放弃,此处直接 expect 简化
                let part = self.body_to_part(filename, body).expect("构造上传体失败");
                form = form.part("document", part);
                self.http.post(&url).multipart(form).send()
            })
            .await?;
        let doc = msg.document.ok_or_else(|| {
            AppError::Telegram("sendDocument 响应缺少 document 字段".into())
        })?;
        Ok(TgMessage { message_id: msg.message_id, file_id: doc.file_id })
    }

    async fn delete_message(&self, message_id: i64) -> AppResult<()> {
        let url = self.method_url("deleteMessage");
        let res: Result<bool, AppError> = self
            .call_with_retry("deleteMessage", || {
                self.http
                    .post(&url)
                    .json(&serde_json::json!({
                        "chat_id": self.chat_id,
                        "message_id": message_id,
                    }))
                    .send()
            })
            .await;
        match res {
            Ok(_) => Ok(()),
            // 消息已不存在视为删除成功(幂等)
            Err(AppError::Telegram(d)) if d.contains("message to delete not found") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn get_me(&self) -> AppResult<String> {
        let url = self.method_url("getMe");
        let user: RawUser = self.call_with_retry("getMe", || self.http.get(&url).send()).await?;
        Ok(user.username.or(user.first_name).unwrap_or_else(|| "bot".into()))
    }

    async fn open_chunk_stream(&self, file_id: &str, offset: u64) -> AppResult<ByteReader> {
        let file_path = self.get_file_path(file_id).await?;
        // local 模式:file_path 是绝对路径且共享卷里能找到 → 直接读盘(支持 seek)
        if file_path.starts_with('/') {
            match tokio::fs::File::open(&file_path).await {
                Ok(mut f) => {
                    if offset > 0 {
                        f.seek(std::io::SeekFrom::Start(offset)).await?;
                    }
                    return Ok(Box::pin(f));
                }
                Err(e) => {
                    tracing::warn!(
                        "local 模式路径 {file_path} 无法直接读取({e}),回退 HTTP;\
                         如使用 --local 请把 telegram-bot-api 数据卷以相同路径挂载到本容器"
                    );
                }
            }
        }
        self.open_http_stream(&file_path, offset).await
    }
}
