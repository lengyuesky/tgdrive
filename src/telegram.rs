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

/// 普通查询/删除可放心重试；发送媒体必须避免在结果不确定时再次发送。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallMode {
    Idempotent,
    SendMedia,
}

fn classify_api_failure(
    mode: CallMode,
    retry: Option<Duration>,
    detail: String,
) -> (Option<Duration>, AppError) {
    let err = if mode == CallMode::SendMedia && retry.is_none() {
        AppError::TelegramRejected(detail)
    } else {
        AppError::Telegram(detail)
    };
    (retry, err)
}

fn classify_malformed_response(
    mode: CallMode,
    status: http::StatusCode,
    detail: String,
) -> (Option<Duration>, AppError) {
    if mode == CallMode::SendMedia {
        if status.as_u16() == 429 {
            (Some(Duration::from_secs(2)), AppError::Telegram(detail))
        } else if status.is_client_error() {
            (None, AppError::TelegramRejected(detail))
        } else {
            // 典型场景是反向代理在上游已处理请求后返回 502/504。
            (
                None,
                AppError::TelegramUncertain(format!(
                    "{detail}；请求可能已经送达，请检查存储频道中的对应分块"
                )),
            )
        }
    } else {
        let retry = (status.is_server_error() || status.as_u16() == 429)
            .then(|| Duration::from_secs(2));
        (retry, AppError::Telegram(detail))
    }
}

pub type ByteReader = Pin<Box<dyn AsyncRead + Send>>;

#[derive(Debug, Clone)]
pub struct TgMessage {
    pub message_id: i64,
    pub file_id: String,
}

/// 视频显示元数据(从文件头解析);sendVideo 不带宽高时
/// Telegram 不探测内容,消息气泡的长宽比是猜的
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoMeta {
    pub width: u32,
    pub height: u32,
    pub duration_secs: Option<u32>,
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
    /// 以「视频消息」发送,客户端可直接流式播放;默认实现退回文档发送,
    /// 只有真实客户端(以及需要转发的代理)才覆写
    async fn send_video(
        &self,
        filename: &str,
        mime: &str,
        meta: Option<VideoMeta>,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let _ = (mime, meta);
        self.send_document(filename, caption, body).await
    }
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
    document: Option<RawFileRef>,
    /// sendVideo 成功时在此;Telegram 也可能改以 document/animation 存储
    video: Option<RawFileRef>,
    animation: Option<RawFileRef>,
}

impl RawMessage {
    fn any_file_id(self) -> Option<String> {
        self.video.or(self.document).or(self.animation).map(|f| f.file_id)
    }
}

#[derive(Deserialize)]
struct RawFileRef {
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

    // 必须显式转发:走 trait 默认实现会在代理层退回 send_document,内层真客户端就收不到视频请求
    async fn send_video(
        &self,
        filename: &str,
        mime: &str,
        meta: Option<VideoMeta>,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        self.current().send_video(filename, mime, meta, caption, body).await
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

    /// 解析响应体；发送媒体时，只有 Telegram 明确返回失败才允许重试。
    /// 响应在回程中损坏时消息可能已经写入，此时必须停止，避免产生重复消息。
    async fn parse<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
        mode: CallMode,
    ) -> Result<T, (Option<Duration>, AppError)> {
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            if mode == CallMode::SendMedia {
                (
                    None,
                    AppError::TelegramUncertain(format!(
                        "请求已发出，但读取响应失败: {e}；请检查存储频道中的对应分块"
                    )),
                )
            } else {
                (Some(Duration::from_secs(2)), AppError::Telegram(format!("读取响应失败: {e}")))
            }
        })?;
        let parsed: Result<ApiResponse<T>, _> = serde_json::from_slice(&body);
        match parsed {
            Ok(r) if r.ok => r.result.ok_or_else(|| {
                let err = if mode == CallMode::SendMedia {
                    AppError::TelegramUncertain(
                        "Telegram 返回成功但响应缺少 result；请检查存储频道中的对应分块".into(),
                    )
                } else {
                    AppError::Telegram("响应缺少 result".into())
                };
                (None, err)
            }),
            Ok(r) => {
                let desc = r.description.unwrap_or_else(|| "未知错误".into());
                let code = r.error_code.unwrap_or(status.as_u16() as i64);
                let retry = r
                    .parameters
                    .and_then(|p| p.retry_after)
                    .map(Duration::from_secs)
                    .or_else(|| {
                        // 5xx 无 retry_after 时按 2s 起步退避
                        (code >= 500).then(|| Duration::from_secs(2))
                    });
                let detail = format!("[{code}] {desc}");
                Err(classify_api_failure(mode, retry, detail))
            }
            Err(_) => {
                let detail = format!(
                    "HTTP {status}: {}",
                    String::from_utf8_lossy(&body[..body.len().min(200)])
                );
                Err(classify_malformed_response(mode, status, detail))
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
                Ok(resp) => Self::parse::<T>(resp, CallMode::Idempotent).await,
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

    fn body_to_part(
        &self,
        filename: &str,
        mime: &str,
        body: &UploadBody,
    ) -> AppResult<reqwest::multipart::Part> {
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
        part.file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| AppError::Telegram(e.to_string()))
    }

    /// sendDocument / sendVideo 共用的 multipart 发送
    #[allow(clippy::too_many_arguments)]
    async fn send_media(
        &self,
        method: &str,
        field: &'static str,
        filename: &str,
        mime: &str,
        meta: Option<VideoMeta>,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let url = self.method_url(method);
        let mut attempt = 0u32;
        let msg: RawMessage = loop {
            attempt += 1;
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", self.chat_id.clone())
                .text("disable_notification", "true");
            if field == "video" {
                // 让客户端不必下完整个文件就能拖动播放
                form = form.text("supports_streaming", "true");
            }
            if let Some(m) = meta {
                // 不带宽高时 Telegram 不探测内容,气泡长宽比是猜的
                form = form
                    .text("width", m.width.to_string())
                    .text("height", m.height.to_string());
                if let Some(d) = m.duration_secs {
                    form = form.text("duration", d.to_string());
                }
            }
            if let Some(c) = &caption {
                form = form.text("caption", c.clone());
            }
            // 每次安全重试都重新打开临时文件；构造失败发生在请求发出前，直接返回即可。
            let part = self.body_to_part(filename, mime, body)?;
            form = form.part(field, part);

            let outcome = match self.http.post(&url).multipart(form).send().await {
                Ok(resp) => Self::parse::<RawMessage>(resp, CallMode::SendMedia).await,
                // 连接尚未建立或请求体没有发完整时，Telegram 不可能成功解析媒体，可安全重试。
                Err(e) if e.is_connect() || e.is_body() => Err((
                    Some(Duration::from_secs(2u64.pow(attempt.min(4)))),
                    AppError::Telegram(format!("{method} 网络错误: {e}")),
                )),
                // 请求体可能已经完整送达，仅响应头丢失；此时重试会制造重复消息。
                Err(e) => Err((
                    None,
                    AppError::TelegramUncertain(format!(
                        "{method} 请求已发出但未收到可靠响应: {e}；请检查存储频道中的对应分块"
                    )),
                )),
            };
            match outcome {
                Ok(msg) => break msg,
                Err((Some(wait), err)) if attempt < MAX_RETRIES => {
                    let jitter = Duration::from_millis((attempt as u64 * 137) % 500);
                    tracing::warn!(
                        "{method} 第 {attempt} 次失败({err}),{:?} 后安全重试",
                        wait + jitter
                    );
                    tokio::time::sleep(wait + jitter).await;
                }
                Err((_, err)) => return Err(err),
            }
        };
        let message_id = msg.message_id;
        let file_id = match msg.any_file_id() {
            Some(file_id) => file_id,
            None => {
                // 已知消息号但结构异常时主动撤销，不能留下数据库无法引用的孤儿消息。
                if let Err(e) = self.delete_message(message_id).await {
                    tracing::warn!("{method} 响应缺少文件字段，且清理消息 {message_id} 失败: {e}");
                }
                return Err(AppError::Telegram(format!("{method} 响应缺少文件字段")));
            }
        };
        Ok(TgMessage { message_id, file_id })
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
        self.send_media(
            "sendDocument",
            "document",
            filename,
            "application/octet-stream",
            None,
            caption,
            body,
        )
        .await
    }

    async fn send_video(
        &self,
        filename: &str,
        mime: &str,
        meta: Option<VideoMeta>,
        caption: Option<String>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        self.send_media("sendVideo", "video", filename, mime, meta, caption, body).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_retry_classification_is_safe() {
        let (retry, err) = classify_malformed_response(
            CallMode::SendMedia,
            http::StatusCode::BAD_GATEWAY,
            "HTTP 502".into(),
        );
        assert!(retry.is_none());
        assert!(matches!(err, AppError::TelegramUncertain(_)));

        let (retry, err) = classify_api_failure(
            CallMode::SendMedia,
            Some(Duration::ZERO),
            "[429] Too Many Requests".into(),
        );
        assert_eq!(retry, Some(Duration::ZERO));
        assert!(matches!(err, AppError::Telegram(_)));

        let (retry, err) = classify_api_failure(
            CallMode::SendMedia,
            None,
            "[400] wrong file identifier".into(),
        );
        assert!(retry.is_none());
        assert!(matches!(err, AppError::TelegramRejected(_)));
    }
}
