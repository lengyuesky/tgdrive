//! Telegram 客户端：Bot API 负责上传/删除，MTProto 或本地缓存负责分块字节流下载。
//!
//! 端点可切换:官方 api.telegram.org 或自部署 telegram-bot-api。
//! 自部署 --local 模式下，已知且仍存在的共享卷文件直接读盘；否则文档型文件
//! 通过 MTProto 按 512 KiB 读取，避免 `getFile` 先拉取完整的大分块。

use crate::db::StorageTarget;
use crate::error::{AppError, AppResult};
use crate::mtproto::{MtprotoClient, MtprotoConfig};
use bytes::Bytes;
use serde::Deserialize;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};

/// getFile 结果缓存时长(官方 file_path 约 1 小时失效,留余量)
const FILE_PATH_TTL: Duration = Duration::from_secs(30 * 60);
const FILE_PATH_CACHE_CAPACITY: usize = 4096;
const MAX_RETRIES: u32 = 5;
/// Telegram 建议同一聊天不要持续超过每秒一条消息。这里只错开发送请求的
/// 起始时间，大文件仍可并行传输，不会把上传并发重新退化成串行。
const MEDIA_SEND_INTERVAL: Duration = Duration::from_millis(1100);
/// Telegram 自定义媒体缩略图必须小于该值。
pub const MAX_THUMBNAIL_SIZE: usize = 200 * 1024;

/// reqwest 错误通常携带完整请求 URL，而 Telegram URL 中含 Bot Token。
/// 对外只保留故障类别，绝不格式化原始错误对象。
pub fn reqwest_error_summary(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else if error.is_body() {
        "传输请求体失败"
    } else if error.is_decode() {
        "响应解析失败"
    } else if error.is_redirect() {
        "重定向失败"
    } else {
        "网络请求失败"
    }
}

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
        let retry =
            (status.is_server_error() || status.as_u16() == 429).then(|| Duration::from_secs(2));
        (retry, AppError::Telegram(detail))
    }
}

pub type ByteReader = Pin<Box<dyn AsyncRead + Send>>;

#[derive(Debug, Clone)]
pub struct TgMessage {
    pub message_id: i64,
    pub file_id: String,
    /// Telegram 实际绑定到该媒体消息的缩略图，可能是自定义图，也可能由服务端生成。
    pub thumbnail_file_id: Option<String>,
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
    TempFile {
        file: tempfile::NamedTempFile,
        len: u64,
    },
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
    /// 多 Bot 共用一个 Bot API 进程时，运行时关闭破坏性的全局重启。
    fn set_restart_enabled(&self, _enabled: bool) {}
    /// 在发起一次新的媒体请求前取得共享发送时隙。真实客户端按聊天错峰，
    /// 测试/占位客户端默认不等待。
    async fn wait_upload_turn(&self) {}
    /// 当前 Telegram 429 共享冷却剩余秒数，供上传界面显示。
    fn upload_retry_after_secs(&self) -> u64 {
        0
    }
    async fn send_document(
        &self,
        filename: &str,
        caption: Option<String>,
        thumbnail: Option<&Bytes>,
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
        thumbnail: Option<&Bytes>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let _ = (mime, meta);
        self.send_document(filename, caption, thumbnail, body).await
    }
    /// 把来源频道中的一条媒体消息复制到当前存储频道，并返回当前 Bot
    /// 可用的新 file_id。默认实现用于不支持该能力的测试客户端。
    async fn copy_message_from(
        &self,
        _from_chat_id: &str,
        _message_id: i64,
    ) -> AppResult<TgMessage> {
        Err(AppError::BadRequest(
            "当前 Telegram 客户端不支持消息迁移".into(),
        ))
    }
    async fn delete_message(&self, message_id: i64) -> AppResult<()>;
    /// 终止已经交给自建 Bot API、但尚未完成的上传。
    /// 官方 Bot API 和测试实现没有进程控制能力，默认无需处理。
    async fn cancel_pending_uploads(&self) -> AppResult<()> {
        Ok(())
    }
    async fn get_me(&self) -> AppResult<String>;
    /// 打开某个分块的字节流，从块内 offset 开始读。
    /// message_id 用于定位日志/后续刷新引用，chunk_size 用于严格限制读取边界。
    async fn open_chunk_stream(
        &self,
        file_id: &str,
        message_id: i64,
        chunk_size: u64,
        offset: u64,
    ) -> AppResult<ByteReader>;

    /// 打开 Telegram 小缩略图。默认实现便于测试客户端复用普通读取逻辑；
    /// 真实客户端会绕过 MTProto 文档解析，直接使用 Bot API 的 getFile。
    async fn open_thumbnail_stream(&self, file_id: &str, message_id: i64) -> AppResult<ByteReader> {
        self.open_chunk_stream(file_id, message_id, MAX_THUMBNAIL_SIZE as u64, 0)
            .await
    }
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
    /// 符合贴纸条件的 WebP 即使用 sendDocument，Telegram 也可能按 sticker 返回。
    sticker: Option<RawFileRef>,
}

impl RawMessage {
    fn any_file(self) -> Option<RawFileRef> {
        self.video
            .or(self.document)
            .or(self.animation)
            .or(self.sticker)
    }
}

#[derive(Deserialize)]
struct RawFileRef {
    file_id: String,
    thumbnail: Option<RawThumbnailRef>,
}

#[derive(Deserialize)]
struct RawThumbnailRef {
    file_id: String,
}

#[derive(Deserialize)]
struct RawFile {
    file_path: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    id: i64,
    username: Option<String>,
    first_name: Option<String>,
}

#[derive(Deserialize)]
struct RawChat {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RawChatMember {
    status: String,
    can_post_messages: Option<bool>,
    can_delete_messages: Option<bool>,
}

fn validate_storage_member(kind: &str, member: &RawChatMember) -> AppResult<()> {
    if !matches!(kind, "channel" | "group" | "supergroup") {
        return Ok(());
    }
    if member.status == "creator" {
        return Ok(());
    }
    if member.status != "administrator" {
        return Err(AppError::BadRequest(
            "Bot 必须是存储频道/群组的管理员".into(),
        ));
    }
    if kind == "channel" && member.can_post_messages != Some(true) {
        return Err(AppError::BadRequest(
            "Bot 缺少在存储频道发布消息的权限".into(),
        ));
    }
    if member.can_delete_messages != Some(true) {
        return Err(AppError::BadRequest("Bot 缺少删除存储消息的权限".into()));
    }
    Ok(())
}

// ---------- 存储目标注册表 ----------

/// 上传开始时取得的不可变目标快照；配置热更新不会把同一文件的分块发往不同频道。
#[derive(Clone)]
pub struct TgBinding {
    pub storage_target_id: i64,
    pub api: Arc<dyn TgApi>,
}

/// 同时保留历史存储目标、主目标和上传池。读取、删除按分块记录的 target_id 路由；
/// 主目标供收件与备份使用，新上传则从池中选择。
pub struct TgRegistry {
    current: std::sync::RwLock<Option<i64>>,
    upload_pool: std::sync::RwLock<Vec<i64>>,
    upload_cursor: AtomicUsize,
    multi_bot_mode: AtomicBool,
    clients: std::sync::RwLock<HashMap<i64, Arc<dyn TgApi>>>,
    mtproto: Option<MtprotoConfig>,
}

fn choose_upload_target(
    pool: &[i64],
    start: usize,
    mut retry_after: impl FnMut(i64) -> Option<u64>,
) -> Option<i64> {
    let mut best: Option<(i64, u64)> = None;
    for offset in 0..pool.len() {
        let id = pool[(start + offset) % pool.len()];
        let Some(wait) = retry_after(id) else {
            continue;
        };
        if best.is_none_or(|(_, best_wait)| wait < best_wait) {
            best = Some((id, wait));
            if wait == 0 {
                break;
            }
        }
    }
    best.map(|(id, _)| id)
}

impl TgRegistry {
    pub fn new(
        targets: &[StorageTarget],
        current: Option<i64>,
        upload_pool: Vec<i64>,
        mtproto: Option<MtprotoConfig>,
    ) -> AppResult<Self> {
        let multi_bot_mode = upload_pool.len() > 1;
        let registry = TgRegistry {
            current: std::sync::RwLock::new(current),
            upload_pool: std::sync::RwLock::new(upload_pool),
            upload_cursor: AtomicUsize::new(0),
            multi_bot_mode: AtomicBool::new(multi_bot_mode),
            clients: std::sync::RwLock::new(HashMap::new()),
            mtproto,
        };
        for target in targets {
            registry.register(target)?;
        }
        Ok(registry)
    }

    #[cfg(test)]
    pub fn with_client(storage_target_id: i64, api: Arc<dyn TgApi>) -> Self {
        TgRegistry {
            current: std::sync::RwLock::new(Some(storage_target_id)),
            upload_pool: std::sync::RwLock::new(vec![storage_target_id]),
            upload_cursor: AtomicUsize::new(0),
            multi_bot_mode: AtomicBool::new(false),
            clients: std::sync::RwLock::new(HashMap::from([(storage_target_id, api)])),
            mtproto: None,
        }
    }

    pub fn register(&self, target: &StorageTarget) -> AppResult<TgBinding> {
        if let Some(api) = self.clients.read().unwrap().get(&target.id).cloned() {
            return Ok(TgBinding {
                storage_target_id: target.id,
                api,
            });
        }
        let api: Arc<dyn TgApi> = Arc::new(
            TgClient::new_with_mtproto_and_restart(
                &target.api_base,
                &target.bot_token,
                &target.chat_id,
                self.mtproto.clone(),
                target.id,
                !self.multi_bot_mode.load(Ordering::Acquire),
            )
            .map_err(|e| AppError::BadRequest(format!("Telegram 客户端构造失败: {e}")))?,
        );
        self.clients.write().unwrap().insert(target.id, api.clone());
        Ok(TgBinding {
            storage_target_id: target.id,
            api,
        })
    }

    /// 热更新主 Bot 与上传池；所有目标必须已注册，避免保存后才在上传途中报错。
    pub fn set_targets(&self, current: Option<i64>, upload_pool: Vec<i64>) -> AppResult<()> {
        let multi_bot_mode = upload_pool.len() > 1;
        self.multi_bot_mode.store(multi_bot_mode, Ordering::Release);
        if let Some(id) = current {
            self.binding(id)?;
        }
        for id in &upload_pool {
            self.binding(*id)?;
        }
        *self.current.write().unwrap() = current;
        *self.upload_pool.write().unwrap() = upload_pool;
        for api in self.clients.read().unwrap().values() {
            api.set_restart_enabled(!multi_bot_mode);
        }
        self.upload_cursor.store(0, Ordering::Release);
        Ok(())
    }

    pub fn current(&self) -> AppResult<TgBinding> {
        let id = self.current.read().unwrap().ok_or_else(|| {
            AppError::BadRequest(
                "还没配置 Telegram 存储:请到「系统设置」填写 Bot Token 与存储频道".into(),
            )
        })?;
        self.binding(id)
    }

    /// 为一个新文件选择上传 Bot：优先没有冷却的目标，同等待时间下轮询分摊。
    /// 调用方会把返回的 target_id 固定进上传会话，文件续传不会换 Bot。
    pub fn next_upload(&self) -> AppResult<TgBinding> {
        let pool = self.upload_pool.read().unwrap().clone();
        if pool.is_empty() {
            return self.current();
        }
        let start = self.upload_cursor.fetch_add(1, Ordering::AcqRel) % pool.len();
        let clients = self.clients.read().unwrap();
        let best = choose_upload_target(&pool, start, |id| {
            clients.get(&id).map(|api| api.upload_retry_after_secs())
        });
        drop(clients);
        let id = best.ok_or_else(|| AppError::BadRequest("上传 Bot 池中没有可用目标".into()))?;
        self.binding(id)
    }

    pub fn upload_bot_count(&self) -> usize {
        self.upload_pool.read().unwrap().len()
    }

    /// 只要池中有一个 Bot 可发送，前端就不应显示全局冷却。
    pub fn upload_retry_after_secs(&self) -> u64 {
        let pool = self.upload_pool.read().unwrap();
        let clients = self.clients.read().unwrap();
        pool.iter()
            .filter_map(|id| clients.get(id))
            .map(|api| api.upload_retry_after_secs())
            .min()
            .unwrap_or(0)
    }

    pub fn binding(&self, storage_target_id: i64) -> AppResult<TgBinding> {
        let api = self
            .clients
            .read()
            .unwrap()
            .get(&storage_target_id)
            .cloned()
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "历史存储目标 {storage_target_id} 不可用，请检查数据库与配置"
                ))
            })?;
        Ok(TgBinding {
            storage_target_id,
            api,
        })
    }
}

// ---------- 未配置占位 ----------

/// 还没填 Bot Token / 存储频道时装在 `SwitchableTg` 里的占位实现。
/// 这样上传、下载、WebDAV 各条路径都能拿到同一句可读的提示,而不是 Telegram 的 401。
#[cfg(test)]
pub struct UnconfiguredTg;

#[cfg(test)]
impl UnconfiguredTg {
    fn err<T>() -> AppResult<T> {
        Err(AppError::BadRequest(
            "还没配置 Telegram 存储:请到「系统设置」填写 Bot Token 与存储频道".into(),
        ))
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl TgApi for UnconfiguredTg {
    async fn send_document(
        &self,
        _filename: &str,
        _caption: Option<String>,
        _thumbnail: Option<&Bytes>,
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

    async fn open_chunk_stream(
        &self,
        _file_id: &str,
        _message_id: i64,
        _chunk_size: u64,
        _offset: u64,
    ) -> AppResult<ByteReader> {
        Self::err()
    }
}

// ---------- 真实客户端 ----------

pub struct TgClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
    /// 有界 LRU：file_id → file_path。
    path_cache: Mutex<FilePathCache>,
    local_data_root: std::path::PathBuf,
    /// 自建 Bot API 监督器的重启信号文件；官方端点不启用。
    bot_api_restart_file: Option<std::path::PathBuf>,
    bot_api_restart_enabled: AtomicBool,
    media_throttle: MediaSendThrottle,
    mtproto: Option<Arc<MtprotoClient>>,
}

struct MediaSendThrottle {
    state: tokio::sync::Mutex<MediaSendThrottleState>,
    cooldown_until_unix_ms: AtomicU64,
    interval: Duration,
}

struct MediaSendThrottleState {
    next_send_at: Instant,
    cooldown_until: Instant,
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl MediaSendThrottle {
    fn new(interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            state: tokio::sync::Mutex::new(MediaSendThrottleState {
                next_send_at: now,
                cooldown_until: now,
            }),
            cooldown_until_unix_ms: AtomicU64::new(0),
            interval,
        }
    }

    /// 所有等待者醒来后重新竞争并检查截止时间，因此延长冷却时不需要逐个通知；
    /// 同一时刻只会有一个任务取得时隙，其余任务自动按 interval 排开。
    async fn wait_turn(&self) {
        loop {
            let deadline = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                let deadline = state.next_send_at.max(state.cooldown_until);
                if deadline <= now {
                    state.next_send_at = now + self.interval;
                    return;
                }
                deadline
            };
            tokio::time::sleep_until(deadline.into()).await;
        }
    }

    async fn defer_all(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        {
            let mut state = self.state.lock().await;
            state.cooldown_until = state.cooldown_until.max(deadline);
            state.next_send_at = state.next_send_at.max(deadline);
        }
        let wait_ms = wait.as_millis().min(u128::from(u64::MAX)) as u64;
        self.cooldown_until_unix_ms
            .fetch_max(unix_millis().saturating_add(wait_ms), Ordering::AcqRel);
    }

    fn retry_after_secs(&self) -> u64 {
        self.cooldown_until_unix_ms
            .load(Ordering::Acquire)
            .saturating_sub(unix_millis())
            .div_ceil(1000)
    }
}

fn is_flood_limit(error: &AppError) -> bool {
    matches!(error, AppError::Telegram(detail) if detail.contains("[429]") || detail.contains("HTTP 429"))
}

struct CachedFilePath {
    path: String,
    cached_at: Instant,
    touched: u64,
}

#[derive(Default)]
struct FilePathCache {
    entries: HashMap<String, CachedFilePath>,
    clock: u64,
}

impl FilePathCache {
    fn get(&mut self, file_id: &str) -> Option<String> {
        let expired = self
            .entries
            .get(file_id)
            .is_some_and(|entry| entry.cached_at.elapsed() >= FILE_PATH_TTL);
        if expired {
            self.entries.remove(file_id);
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(file_id)?;
        entry.touched = self.clock;
        Some(entry.path.clone())
    }

    fn insert(&mut self, file_id: String, path: String) {
        if self.entries.len() >= FILE_PATH_CACHE_CAPACITY
            && !self.entries.contains_key(&file_id)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries.insert(
            file_id,
            CachedFilePath {
                path,
                cached_at: Instant::now(),
                touched: self.clock,
            },
        );
    }
}

impl TgClient {
    pub fn new(api_base: &str, token: &str, chat_id: &str) -> anyhow::Result<Self> {
        Self::build(api_base, token, chat_id, None, true)
    }

    fn new_with_mtproto_and_restart(
        api_base: &str,
        token: &str,
        chat_id: &str,
        config: Option<MtprotoConfig>,
        storage_target_id: i64,
        restart_enabled: bool,
    ) -> anyhow::Result<Self> {
        let mtproto =
            config.map(|config| Arc::new(MtprotoClient::new(config, storage_target_id, token)));
        Self::build(api_base, token, chat_id, mtproto, restart_enabled)
    }

    fn build(
        api_base: &str,
        token: &str,
        chat_id: &str,
        mtproto: Option<Arc<MtprotoClient>>,
        restart_enabled: bool,
    ) -> anyhow::Result<Self> {
        let api_base = api_base.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            // 上传/下载大分块耗时不可控,不设总超时
            .build()?;
        Ok(TgClient {
            http,
            bot_api_restart_file: (restart_enabled && !crate::config::is_official(&api_base))
                .then(|| std::env::var("TGDRIVE_BOT_API_RESTART_FILE").ok())
                .flatten()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(Into::into),
            bot_api_restart_enabled: AtomicBool::new(restart_enabled),
            api_base,
            token: token.to_string(),
            chat_id: chat_id.to_string(),
            path_cache: Mutex::new(FilePathCache::default()),
            media_throttle: MediaSendThrottle::new(MEDIA_SEND_INTERVAL),
            local_data_root: std::env::var("TGDRIVE_LOCAL_DATA_ROOT")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(Into::into)
                .unwrap_or_else(|| crate::config::DEFAULT_LOCAL_DATA_ROOT.into()),
            mtproto,
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.token, method)
    }

    fn file_url(&self, file_path: &str) -> String {
        format!("{}/file/bot{}/{}", self.api_base, self.token, file_path)
    }

    async fn raw_me(&self) -> AppResult<RawUser> {
        let url = self.method_url("getMe");
        self.call_with_retry("getMe", || self.http.get(&url).send())
            .await
    }

    /// 验证 Bot 身份、目标会话以及频道/群组内的发消息和删除权限。
    pub async fn probe_storage(&self) -> AppResult<String> {
        let user = self.raw_me().await?;
        let name = user
            .username
            .or(user.first_name)
            .unwrap_or_else(|| "bot".into());
        // 会话探测页只需要验证 Token，使用内部哨兵值跳过目标检查。
        if self.chat_id == "0" {
            return Ok(name);
        }

        let get_chat_url = self.method_url("getChat");
        let chat: RawChat = self
            .call_with_retry("getChat", || {
                self.http
                    .post(&get_chat_url)
                    .json(&serde_json::json!({ "chat_id": self.chat_id }))
                    .send()
            })
            .await?;

        if matches!(chat.kind.as_str(), "channel" | "group" | "supergroup") {
            let member_url = self.method_url("getChatMember");
            let member: RawChatMember = self
                .call_with_retry("getChatMember", || {
                    self.http
                        .post(&member_url)
                        .json(&serde_json::json!({
                            "chat_id": self.chat_id,
                            "user_id": user.id,
                        }))
                        .send()
                })
                .await?;
            validate_storage_member(&chat.kind, &member)?;
        }
        Ok(name)
    }

    /// 解析响应体；发送媒体时，只有 Telegram 明确返回失败才允许重试。
    /// 响应在回程中损坏时消息可能已经写入，此时必须停止，避免产生重复消息。
    async fn parse<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
        mode: CallMode,
    ) -> Result<T, (Option<Duration>, AppError)> {
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            if mode == CallMode::SendMedia {
                (
                    None,
                    AppError::TelegramUncertain(format!(
                        "请求已发出，但{}；请检查存储频道中的对应分块",
                        reqwest_error_summary(&e)
                    )),
                )
            } else {
                (
                    Some(Duration::from_secs(2)),
                    AppError::Telegram(reqwest_error_summary(&e).into()),
                )
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
                let desc = r
                    .description
                    .unwrap_or_else(|| "未知错误".into())
                    .replace(&self.token, "[已脱敏]");
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
                let detail = format!("HTTP {status}: 响应不是合法的 Telegram JSON");
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
                Ok(resp) => self.parse::<T>(resp, CallMode::Idempotent).await,
                Err(e) => Err((
                    Some(Duration::from_secs(2u64.pow(attempt.min(4)))),
                    AppError::Telegram(format!("{what}: {}", reqwest_error_summary(&e))),
                )),
            };
            match outcome {
                Ok(v) => return Ok(v),
                Err((Some(wait), err)) if attempt < MAX_RETRIES => {
                    // flood 控制:叠加少量抖动,避免并发任务同时重试
                    let jitter = Duration::from_millis((attempt as u64 * 137) % 500);
                    tracing::warn!(
                        "{what} 第 {attempt} 次失败({err}),{:?} 后重试",
                        wait + jitter
                    );
                    tokio::time::sleep(wait + jitter).await;
                }
                Err((_, err)) => return Err(err),
            }
        }
    }

    async fn get_file_path(&self, file_id: &str) -> AppResult<String> {
        if let Some(path) = self.path_cache.lock().unwrap().get(file_id) {
            return Ok(path);
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
            .insert(file_id.to_string(), path.clone());
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
        thumbnail: Option<&Bytes>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let url = self.method_url(method);
        let mut attempt = 0u32;
        let msg: RawMessage = loop {
            // 第一次发送已由调用者在“标记上游在途”之前取得时隙；只有安全重试
            // 需要在这里再次排队，防止取消等待任务时误重启 Bot API。
            if attempt > 0 {
                self.media_throttle.wait_turn().await;
            }
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
            if let Some(image) = thumbnail {
                let part = reqwest::multipart::Part::stream_with_length(
                    reqwest::Body::from(image.clone()),
                    image.len() as u64,
                )
                .file_name("thumbnail.jpg")
                .mime_str("image/jpeg")
                .map_err(|e| AppError::Telegram(e.to_string()))?;
                form = form
                    .text("thumbnail", "attach://thumbnail")
                    .part("thumbnail", part);
            }
            // 每次安全重试都重新打开临时文件；构造失败发生在请求发出前，直接返回即可。
            let part = self.body_to_part(filename, mime, body)?;
            form = form.part(field, part);

            let outcome = match self.http.post(&url).multipart(form).send().await {
                Ok(resp) => self.parse::<RawMessage>(resp, CallMode::SendMedia).await,
                // 连接尚未建立或请求体没有发完整时，Telegram 不可能成功解析媒体，可安全重试。
                Err(e) if e.is_connect() || e.is_body() => Err((
                    Some(Duration::from_secs(2u64.pow(attempt.min(4)))),
                    AppError::Telegram(format!("{method}: {}", reqwest_error_summary(&e))),
                )),
                // 请求体可能已经完整送达，仅响应头丢失；此时重试会制造重复消息。
                Err(e) => Err((
                    None,
                    AppError::TelegramUncertain(format!(
                        "{method} 请求已发出但未收到可靠响应（{}）；请检查存储频道中的对应分块",
                        reqwest_error_summary(&e)
                    )),
                )),
            };
            match outcome {
                Ok(msg) => break msg,
                Err((Some(wait), err)) if attempt < MAX_RETRIES => {
                    let jitter = Duration::from_millis((attempt as u64 * 137) % 500);
                    let wait = wait + jitter;
                    if is_flood_limit(&err) {
                        self.media_throttle.defer_all(wait).await;
                        tracing::warn!(
                            "{method} 第 {attempt} 次触发 Telegram 限流({err})，该 Bot 的上传共享等待 {:?}",
                            wait
                        );
                    } else {
                        tracing::warn!("{method} 第 {attempt} 次失败({err}),{:?} 后安全重试", wait);
                        tokio::time::sleep(wait).await;
                    }
                }
                Err((_, err)) => return Err(err),
            }
        };
        let message_id = msg.message_id;
        let media = match msg.any_file() {
            Some(media) => media,
            None => {
                // 已知消息号但结构异常时主动撤销，不能留下数据库无法引用的孤儿消息。
                if let Err(e) = self.delete_message(message_id).await {
                    tracing::warn!("{method} 响应缺少文件字段，且清理消息 {message_id} 失败: {e}");
                }
                return Err(AppError::Telegram(format!("{method} 响应缺少文件字段")));
            }
        };
        let thumbnail_file_id = media.thumbnail.map(|thumbnail| thumbnail.file_id);
        Ok(TgMessage {
            message_id,
            file_id: media.file_id,
            thumbnail_file_id,
        })
    }

    /// HTTP 下载分块;offset>0 时先试 Range,服务器不支持则丢弃前缀
    async fn open_http_stream(&self, file_path: &str, offset: u64) -> AppResult<ByteReader> {
        let url = self.file_url(file_path);
        let mut req = self.http.get(&url);
        if offset > 0 {
            req = req.header(http::header::RANGE, format!("bytes={offset}-"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Telegram(format!("下载失败: {}", reqwest_error_summary(&e))))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Telegram(format!("下载分块 HTTP {status}")));
        }
        let got_range = status == http::StatusCode::PARTIAL_CONTENT;
        let stream = resp.bytes_stream();
        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            stream,
            |error| std::io::Error::other(reqwest_error_summary(&error)),
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

    /// 尝试使用已经知道的 file_path。这个函数本身不会调用 getFile，因此不会触发
    /// 本地 Bot API 为了生成路径而先下载完整 Telegram 文件。
    async fn open_known_path(&self, file_path: &str, offset: u64) -> AppResult<Option<ByteReader>> {
        if !file_path.starts_with('/') {
            return self.open_http_stream(file_path, offset).await.map(Some);
        }

        // local 模式的绝对路径必须位于显式数据根目录中，避免上游返回任意主机路径。
        let root = tokio::fs::canonicalize(&self.local_data_root).await;
        let candidate = tokio::fs::canonicalize(file_path).await;
        match (root, candidate) {
            (Ok(root), Ok(candidate)) if !candidate.starts_with(&root) => Err(AppError::Telegram(
                "Bot API 返回的本地文件路径超出允许的数据目录，已拒绝读取".into(),
            )),
            (Ok(_), Ok(candidate)) => match tokio::fs::File::open(candidate).await {
                Ok(mut file) => {
                    if offset > 0 {
                        file.seek(std::io::SeekFrom::Start(offset)).await?;
                    }
                    Ok(Some(Box::pin(file)))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    tracing::warn!(error = %error, "本地 Bot API 缓存文件无法读取");
                    Ok(None)
                }
            },
            _ => Ok(None),
        }
    }
}

#[async_trait::async_trait]
impl TgApi for TgClient {
    fn set_restart_enabled(&self, enabled: bool) {
        self.bot_api_restart_enabled
            .store(enabled, Ordering::Release);
    }

    async fn wait_upload_turn(&self) {
        self.media_throttle.wait_turn().await;
    }

    fn upload_retry_after_secs(&self) -> u64 {
        self.media_throttle.retry_after_secs()
    }

    async fn send_document(
        &self,
        filename: &str,
        caption: Option<String>,
        thumbnail: Option<&Bytes>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        self.send_media(
            "sendDocument",
            "document",
            filename,
            "application/octet-stream",
            None,
            caption,
            thumbnail,
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
        thumbnail: Option<&Bytes>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        self.send_media(
            "sendVideo",
            "video",
            filename,
            mime,
            meta,
            caption,
            thumbnail,
            body,
        )
        .await
    }

    async fn copy_message_from(&self, from_chat_id: &str, message_id: i64) -> AppResult<TgMessage> {
        // Bot API 的 copyMessage 只返回 MessageId，拿不到主 Bot 专属的新 file_id；
        // forwardMessage 同样在 Telegram 服务端复制媒体，但会返回完整 Message，
        // 因而能安全更新 tgdrive 的分块映射。
        let url = self.method_url("forwardMessage");
        let mut attempt = 0u32;
        let raw: RawMessage = loop {
            if attempt > 0 {
                self.media_throttle.wait_turn().await;
            }
            attempt += 1;
            let outcome = match self
                .http
                .post(&url)
                .json(&serde_json::json!({
                    "chat_id": self.chat_id,
                    "from_chat_id": from_chat_id,
                    "message_id": message_id,
                    "disable_notification": true,
                }))
                .send()
                .await
            {
                Ok(response) => {
                    self.parse::<RawMessage>(response, CallMode::SendMedia)
                        .await
                }
                Err(error) if error.is_connect() => Err((
                    None,
                    AppError::Telegram(format!(
                        "forwardMessage: {}",
                        reqwest_error_summary(&error)
                    )),
                )),
                Err(error) => Err((
                    None,
                    AppError::TelegramUncertain(format!(
                        "forwardMessage 请求结果不确定（{}）；可能已产生一条未登记的复制消息",
                        reqwest_error_summary(&error)
                    )),
                )),
            };
            match outcome {
                Ok(message) => break message,
                Err((Some(wait), error)) if is_flood_limit(&error) && attempt < MAX_RETRIES => {
                    let wait = wait + Duration::from_millis((attempt as u64 * 137) % 500);
                    self.media_throttle.defer_all(wait).await;
                    tracing::warn!(
                        "forwardMessage 第 {attempt} 次触发限流({error})，共享等待 {:?}",
                        wait
                    );
                }
                // 发送类 5xx 即使带有重试提示也可能已经创建消息，不能自动重放。
                Err((Some(_), error)) => {
                    return Err(AppError::TelegramUncertain(format!(
                        "{error}；可能已产生一条未登记的复制消息"
                    )));
                }
                Err((_, error)) => return Err(error),
            }
        };
        let copied_message_id = raw.message_id;
        let Some(media) = raw.any_file() else {
            if let Err(error) = self.delete_message(copied_message_id).await {
                tracing::warn!("迁移消息 {copied_message_id} 缺少媒体且清理失败: {error}");
            }
            return Err(AppError::Telegram(format!(
                "复制后的消息 {copied_message_id} 没有可用媒体字段"
            )));
        };
        Ok(TgMessage {
            message_id: copied_message_id,
            file_id: media.file_id,
            thumbnail_file_id: media.thumbnail.map(|thumbnail| thumbnail.file_id),
        })
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

    async fn cancel_pending_uploads(&self) -> AppResult<()> {
        if !self.bot_api_restart_enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(path) = &self.bot_api_restart_file else {
            return Ok(());
        };

        tokio::fs::write(path, crate::db::now_ts().to_string()).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::fs::metadata(path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(_) if tokio::time::Instant::now() >= deadline => {
                    return Err(AppError::Telegram(
                        "Bot API 未在 10 秒内确认终止上传".into(),
                    ));
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    async fn get_me(&self) -> AppResult<String> {
        let user = self.raw_me().await?;
        Ok(user
            .username
            .or(user.first_name)
            .unwrap_or_else(|| "bot".into()))
    }

    async fn open_chunk_stream(
        &self,
        file_id: &str,
        message_id: i64,
        chunk_size: u64,
        offset: u64,
    ) -> AppResult<ByteReader> {
        // 已经明确知道并且仍可用的本地缓存最快，也完全没有外网流量。
        let cached_path = self.path_cache.lock().unwrap().get(file_id);
        if let Some(path) = cached_path
            && let Some(reader) = self.open_known_path(&path, offset).await?
        {
            return Ok(reader);
        }

        // 文档/视频直接走 MTProto 小块读取。失败时不回退 getFile，避免一次 Range
        // 请求悄悄变成最多 1.9GB 的整块下载。
        if let Some(mtproto) = &self.mtproto
            && let Some(reader) = mtproto
                .open_stream(file_id, message_id, chunk_size, offset)
                .await?
        {
            return Ok(reader);
        }

        // 未配置 MTProto，或照片等不支持的 file_id，保留原 Bot API 路径。
        let file_path = self.get_file_path(file_id).await?;
        if let Some(reader) = self.open_known_path(&file_path, offset).await? {
            return Ok(reader);
        }
        tracing::warn!(
            message_id,
            "本地 Bot API 文件无法从共享卷直接读取，回退其 HTTP 文件端点"
        );
        self.open_http_stream(&file_path, offset).await
    }

    async fn open_thumbnail_stream(
        &self,
        file_id: &str,
        _message_id: i64,
    ) -> AppResult<ByteReader> {
        // 缩略图很小且属于 PhotoSize，不交给只解析文档 file_id 的 MTProto 链路。
        let file_path = self.get_file_path(file_id).await?;
        if let Some(reader) = self.open_known_path(&file_path, 0).await? {
            return Ok(reader);
        }
        self.open_http_stream(&file_path, 0).await
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
        assert!(is_flood_limit(&AppError::Telegram(
            "[429] Too Many Requests".into()
        )));
        assert!(!is_flood_limit(&AppError::Telegram(
            "[500] Internal Server Error".into()
        )));
    }

    #[tokio::test]
    async fn media_throttle_shares_cooldown_and_staggers_waiters() {
        let throttle = Arc::new(MediaSendThrottle::new(Duration::from_millis(20)));
        throttle.defer_all(Duration::from_millis(40)).await;
        assert_eq!(throttle.retry_after_secs(), 1);

        let started = Instant::now();
        let first = {
            let throttle = throttle.clone();
            tokio::spawn(async move { throttle.wait_turn().await })
        };
        let second = {
            let throttle = throttle.clone();
            tokio::spawn(async move { throttle.wait_turn().await })
        };
        first.await.unwrap();
        second.await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "共享冷却结束后，两个等待者仍应按发送间隔错峰"
        );
    }

    #[test]
    fn file_path_cache_is_bounded_and_refreshes_hits() {
        let mut cache = FilePathCache::default();
        for i in 0..=FILE_PATH_CACHE_CAPACITY {
            cache.insert(format!("f{i}"), format!("p{i}"));
        }
        assert_eq!(cache.entries.len(), FILE_PATH_CACHE_CAPACITY);
        assert_eq!(
            cache.get(&format!("f{FILE_PATH_CACHE_CAPACITY}")),
            Some(format!("p{FILE_PATH_CACHE_CAPACITY}"))
        );
    }

    #[test]
    fn storage_member_requires_channel_permissions() {
        let member = RawChatMember {
            status: "administrator".into(),
            can_post_messages: Some(true),
            can_delete_messages: Some(true),
        };
        assert!(validate_storage_member("channel", &member).is_ok());

        let missing_delete = RawChatMember {
            can_delete_messages: Some(false),
            ..member
        };
        assert!(validate_storage_member("channel", &missing_delete).is_err());
    }

    #[test]
    fn upload_pool_round_robins_and_skips_cooled_bots() {
        let pool = [10, 20, 30];
        assert_eq!(choose_upload_target(&pool, 0, |_| Some(0)), Some(10));
        assert_eq!(choose_upload_target(&pool, 1, |_| Some(0)), Some(20));
        assert_eq!(
            choose_upload_target(&pool, 0, |id| Some(if id == 10 { 30 } else { 0 })),
            Some(20)
        );
        assert_eq!(
            choose_upload_target(&pool, 0, |id| Some(match id {
                10 => 30,
                20 => 12,
                _ => 18,
            })),
            Some(20)
        );
    }

    #[test]
    fn media_response_keeps_telegram_thumbnail_file_id() {
        let message: RawMessage = serde_json::from_value(serde_json::json!({
            "message_id": 42,
            "video": {
                "file_id": "video-file",
                "thumbnail": { "file_id": "thumbnail-file" }
            }
        }))
        .unwrap();
        let media = message.any_file().unwrap();
        assert_eq!(media.file_id, "video-file");
        assert_eq!(
            media
                .thumbnail
                .map(|thumbnail| thumbnail.file_id)
                .as_deref(),
            Some("thumbnail-file")
        );
    }

    #[test]
    fn webp_sticker_response_is_accepted_as_uploaded_file() {
        let message: RawMessage = serde_json::from_value(serde_json::json!({
            "message_id": 43,
            "sticker": {
                "file_id": "webp-sticker-file",
                "thumbnail": { "file_id": "webp-thumbnail-file" }
            }
        }))
        .unwrap();
        let media = message.any_file().unwrap();
        assert_eq!(media.file_id, "webp-sticker-file");
        assert_eq!(
            media
                .thumbnail
                .map(|thumbnail| thumbnail.file_id)
                .as_deref(),
            Some("webp-thumbnail-file")
        );
    }
}
