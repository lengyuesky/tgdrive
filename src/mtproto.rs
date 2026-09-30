//! 通过 MTProto 按需读取 Telegram 文件。
//!
//! 本地 Bot API 的 `getFile` 会在返回路径前先下载完整文件，不适合浏览器 Range
//! 播放。这里直接解析 Bot API `file_id`，并行请求 1~8 个 512 KiB 分段，
//! 按顺序交给现有 `ChunkedReader`；有界预读随读取流一起释放。

mod history;

use crate::config::{
    DEFAULT_DOWNLOAD_CONCURRENCY, MAX_DOWNLOAD_CONCURRENCY, MIN_DOWNLOAD_CONCURRENCY,
};
use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::proxy::{DcTunnel, ProxyClient, ProxyConfig};
use crate::telegram::ByteReader;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use grammers_client::Client;
use grammers_client::media::Downloadable;
use grammers_mtsender::{SenderPool, SenderPoolFatHandle};
use grammers_session::types::{
    ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::{BoxFuture, Session, SessionData};
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Semaphore;

/// 当前 grammers 下载接口支持的最大分段；Telegram 协议本身可支持更大的请求。
const REQUEST_SIZE: i32 = 512 * 1024;
const SESSION_VERSION: u8 = 1;

const WEB_LOCATION_FLAG: i32 = 1 << 24;
const FILE_REFERENCE_FLAG: i32 = 1 << 25;

/// 启动时固定的 MTProto 应用配置。api_id/api_hash 改动后需重启 tgdrive。
#[derive(Clone)]
pub struct MtprotoConfig {
    db: Db,
    api_id: i32,
    api_hash: Arc<str>,
    download: Arc<DownloadControl>,
}

/// 并发设置热更新；同一 Bot 的历史存储目标共享总量限制。
struct DownloadControl {
    concurrency: AtomicUsize,
    bot_requests: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl MtprotoConfig {
    /// 两项未配齐时关闭 MTProto；api_id 已填写但非法时给出明确错误。
    pub fn from_values(db: Db, api_id: &str, api_hash: &str) -> AppResult<Option<Self>> {
        let api_id = api_id.trim();
        let api_hash = api_hash.trim();
        if api_id.is_empty() || api_hash.is_empty() {
            if !api_id.is_empty() || !api_hash.is_empty() {
                tracing::warn!("api_id/api_hash 尚未配齐，MTProto 流式下载暂未启用");
            }
            return Ok(None);
        }
        let api_id = api_id
            .parse::<i32>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| AppError::BadRequest("api_id 必须是有效的正整数".into()))?;
        Ok(Some(Self {
            db,
            api_id,
            api_hash: Arc::from(api_hash),
            download: Arc::new(DownloadControl {
                concurrency: AtomicUsize::new(DEFAULT_DOWNLOAD_CONCURRENCY),
                bot_requests: Mutex::new(HashMap::new()),
            }),
        }))
    }

    pub fn download_concurrency(&self) -> usize {
        self.download.concurrency.load(Ordering::Acquire)
    }

    pub fn set_download_concurrency(&self, concurrency: usize) {
        self.download.concurrency.store(
            concurrency.clamp(MIN_DOWNLOAD_CONCURRENCY, MAX_DOWNLOAD_CONCURRENCY),
            Ordering::Release,
        );
    }

    fn request_slots(&self, bot_token: &str) -> Arc<Semaphore> {
        self.download
            .bot_requests
            .lock()
            .unwrap()
            .entry(bot_token.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY)))
            .clone()
    }
}

/// 每个不可变存储目标各自持有一个 Bot MTProto 会话。
pub struct MtprotoClient {
    config: MtprotoConfig,
    storage_target_id: i64,
    bot_token: Arc<str>,
    state: tokio::sync::Mutex<Option<Arc<ClientState>>>,
    network: Arc<ProxyClient>,
    request_slots: Arc<Semaphore>,
}

struct ClientState {
    proxy: ProxyConfig,
    _snapshot: Arc<crate::proxy::ProxySnapshot>,
    client: Client,
    handle: SenderPoolFatHandle,
    runner_task: tokio::task::JoinHandle<()>,
}

impl Drop for ClientState {
    fn drop(&mut self) {
        self.handle.quit();
        // 登录途中取消、代理切换或最后一个读取流释放时都不能留下连接任务。
        self.runner_task.abort();
    }
}

impl MtprotoClient {
    pub fn new(
        config: MtprotoConfig,
        storage_target_id: i64,
        bot_token: &str,
        network: Arc<ProxyClient>,
    ) -> Self {
        let request_slots = config.request_slots(bot_token);
        Self {
            config,
            storage_target_id,
            bot_token: Arc::from(bot_token),
            state: tokio::sync::Mutex::new(None),
            network,
            request_slots,
        }
    }

    async fn state(&self) -> AppResult<Arc<ClientState>> {
        let mut current = self.state.lock().await;
        let snapshot = self.network.current();
        if let Some(state) = current.as_ref()
            && state.proxy == snapshot.config
        {
            return Ok(state.clone());
        }
        let key = format!("mtproto.session.{}", self.storage_target_id);
        let mut session = PersistentSession::load(self.config.db.clone(), key).await?;
        session.proxy = snapshot.config.clone();
        let SenderPool { runner, handle, .. } =
            SenderPool::new(Arc::new(session), self.config.api_id);
        let state = Arc::new(ClientState {
            proxy: snapshot.config.clone(),
            _snapshot: snapshot,
            client: Client::new(handle.clone()),
            handle,
            runner_task: tokio::spawn(runner.run()),
        });
        let authorized = state
            .client
            .is_authorized()
            .await
            .map_err(|error| mtproto_error("检查登录状态失败", error))?;
        if !authorized {
            state
                .client
                .bot_sign_in(&self.bot_token, &self.config.api_hash)
                .await
                .map_err(|error| mtproto_error("Bot 登录失败", error))?;
        }
        tracing::info!(
            storage_target_id = self.storage_target_id,
            "MTProto 流式下载会话已就绪"
        );
        // 已打开的读取流持有旧会话，新代理只用于之后打开的流。
        *current = Some(state.clone());
        Ok(state)
    }

    /// 尝试按 Range 打开文档型文件。
    ///
    /// `Ok(None)` 只表示这是照片/Web location 等不支持的 file_id，调用方可走
    /// 原 Bot API；文档型 file_id 一旦进入 MTProto，失败不会静默回退整块下载。
    pub async fn open_stream(
        &self,
        file_id: &str,
        message_id: i64,
        size: u64,
        offset: u64,
        length: u64,
    ) -> AppResult<Option<ByteReader>> {
        if offset > size {
            return Err(AppError::Telegram(format!(
                "分块偏移 {offset} 超过分块大小 {size}"
            )));
        }
        if offset == size || length == 0 {
            return Ok(Some(Box::pin(std::io::Cursor::new(Vec::<u8>::new()))));
        }

        let Some(location) = decode_document(file_id, size)? else {
            return Ok(None);
        };
        let state = self.state().await?;
        let location = Arc::new(location);
        let storage_target_id = self.storage_target_id;
        let concurrency = self.config.download_concurrency();
        let reader = open_parallel_stream(
            offset.saturating_add(length).min(size),
            offset,
            concurrency,
            self.request_slots.clone(),
            move |part_offset| {
                let state = state.clone();
                let location = location.clone();
                async move {
                    let skip_chunks = i32::try_from(part_offset / REQUEST_SIZE as u64)
                        .map_err(|_| AppError::Telegram("分块偏移过大".into()))?;
                    let mut download = state
                        .client
                        .iter_download(location.as_ref())
                        .chunk_size(REQUEST_SIZE)
                        .skip_chunks(skip_chunks);
                    download
                        .next()
                        .await
                        .map_err(|error| {
                            tracing::warn!(
                                storage_target_id,
                                message_id,
                                part_offset,
                                error = %error,
                                "MTProto 分段读取中断"
                            );
                            mtproto_error("读取数据块失败", error)
                        })
                        .map(|bytes| bytes.unwrap_or_default())
                }
            },
        )
        .await?;
        Ok(Some(reader))
    }
}

/// 按需填充固定大小的请求窗口，乱序完成的数据仍按文件偏移输出。
/// 没有独立预取任务：暂停读取不再补充请求，丢弃 reader 同时丢弃全部待取分段。
async fn open_parallel_stream<F, Fut>(
    end: u64,
    offset: u64,
    concurrency: usize,
    request_slots: Arc<Semaphore>,
    fetch: F,
) -> AppResult<ByteReader>
where
    F: Fn(u64) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AppResult<Vec<u8>>> + Send + 'static,
{
    if offset > end {
        return Err(AppError::Telegram("下载偏移超过范围终点".into()));
    }
    if offset == end {
        return Ok(Box::pin(std::io::Cursor::new(Vec::<u8>::new())));
    }
    let part_size = REQUEST_SIZE as u64;
    let first_part = offset / part_size;
    let part_count = end.div_ceil(part_size);
    let fetch = Arc::new(fetch);
    let mut parts = stream::iter(first_part..part_count)
        .map(move |index| {
            let fetch = fetch.clone();
            let request_slots = request_slots.clone();
            async move {
                let _permit = request_slots.acquire_owned().await.map_err(|_| {
                    AppError::Telegram("下载并发控制已关闭".into())
                })?;
                let part_offset = index * part_size;
                let expected = (end - part_offset).min(part_size) as usize;
                let bytes = fetch(part_offset).await?;
                if bytes.len() < expected {
                    return Err(AppError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "MTProto 分段提前结束：偏移 {part_offset}，预期 {expected} 字节，实际 {} 字节",
                            bytes.len()
                        ),
                    )));
                }
                let skip = offset.saturating_sub(part_offset) as usize;
                Ok(Bytes::from(bytes).slice(skip..expected))
            }
        })
        .buffered(concurrency.clamp(MIN_DOWNLOAD_CONCURRENCY, MAX_DOWNLOAD_CONCURRENCY))
        .boxed();

    // 在交给 ChunkedReader 前验证首段；后续错误通过读取流原样返回。
    let first = parts
        .try_next()
        .await?
        .ok_or_else(|| AppError::Telegram("Telegram 未返回分块数据".into()))?;
    let stream = stream::once(async move { Ok(first) })
        .chain(parts)
        .map_err(|error| match error {
            AppError::Io(error) => error,
            other => std::io::Error::other(other.to_string()),
        })
        .boxed();
    Ok(Box::pin(tokio_util::io::StreamReader::new(stream)))
}

fn mtproto_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Telegram(format!("MTProto {context}: {error}"))
}

struct FileLocation {
    raw: tl::enums::InputFileLocation,
    size: usize,
}

impl Downloadable for FileLocation {
    fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
        Some(self.raw.clone())
    }

    fn size(&self) -> Option<usize> {
        Some(self.size)
    }
}

fn decode_document(file_id: &str, size: u64) -> AppResult<Option<FileLocation>> {
    let packed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(file_id)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(file_id))
        .map_err(|_| AppError::Telegram("Telegram file_id 的 Base64 数据无效".into()))?;
    let decoded = zero_rle_decode(&packed)?;
    let mut pos = 0usize;
    let raw_type = read_i32(&decoded, &mut pos)?;
    let _dc_id = read_i32(&decoded, &mut pos)?;

    if raw_type & WEB_LOCATION_FLAG != 0 {
        return Ok(None);
    }
    let file_type = raw_type & !(WEB_LOCATION_FLAG | FILE_REFERENCE_FLAG);
    // Bot API 的语音、视频、文档、贴纸、音频、动画及视频便笺都使用 document location。
    if !matches!(file_type, 3 | 4 | 5 | 8 | 9 | 10 | 14) {
        return Ok(None);
    }
    let file_reference = if raw_type & FILE_REFERENCE_FLAG != 0 {
        read_tl_bytes(&decoded, &mut pos)?
    } else {
        Vec::new()
    };
    let id = read_i64(&decoded, &mut pos)?;
    let access_hash = read_i64(&decoded, &mut pos)?;
    let size = usize::try_from(size)
        .map_err(|_| AppError::Telegram("当前平台无法表示该分块大小".into()))?;

    Ok(Some(FileLocation {
        raw: tl::types::InputDocumentFileLocation {
            id,
            access_hash,
            file_reference,
            thumb_size: String::new(),
        }
        .into(),
        size,
    }))
}

fn zero_rle_decode(packed: &[u8]) -> AppResult<Vec<u8>> {
    let mut decoded = Vec::with_capacity(packed.len() * 2);
    let mut pos = 0usize;
    while pos < packed.len() {
        if packed[pos] == 0 {
            pos += 1;
            let count = packed
                .get(pos)
                .copied()
                .ok_or_else(|| AppError::Telegram("Telegram file_id 的 RLE 数据不完整".into()))?;
            decoded.resize(decoded.len() + count as usize, 0);
        } else {
            decoded.push(packed[pos]);
        }
        pos += 1;
    }
    Ok(decoded)
}

fn read_i32(data: &[u8], pos: &mut usize) -> AppResult<i32> {
    let bytes: [u8; 4] = data
        .get(*pos..*pos + 4)
        .ok_or_else(|| AppError::Telegram("Telegram file_id 过短".into()))?
        .try_into()
        .map_err(|_| AppError::Telegram("Telegram file_id 的 i32 字段无效".into()))?;
    *pos += 4;
    Ok(i32::from_le_bytes(bytes))
}

fn read_i64(data: &[u8], pos: &mut usize) -> AppResult<i64> {
    let bytes: [u8; 8] = data
        .get(*pos..*pos + 8)
        .ok_or_else(|| AppError::Telegram("Telegram file_id 过短".into()))?
        .try_into()
        .map_err(|_| AppError::Telegram("Telegram file_id 的 i64 字段无效".into()))?;
    *pos += 8;
    Ok(i64::from_le_bytes(bytes))
}

fn read_tl_bytes(data: &[u8], pos: &mut usize) -> AppResult<Vec<u8>> {
    let first = *data
        .get(*pos)
        .ok_or_else(|| AppError::Telegram("Telegram file_id 缺少 bytes 长度".into()))?;
    *pos += 1;
    let (len, header_len) = if first < 254 {
        (first as usize, 1usize)
    } else if first == 254 {
        let len_bytes = data
            .get(*pos..*pos + 3)
            .ok_or_else(|| AppError::Telegram("Telegram file_id 的 bytes 长度不完整".into()))?;
        *pos += 3;
        (
            len_bytes[0] as usize | (len_bytes[1] as usize) << 8 | (len_bytes[2] as usize) << 16,
            4usize,
        )
    } else {
        return Err(AppError::Telegram(
            "Telegram file_id 的 bytes 长度标记无效".into(),
        ));
    };
    let value = data
        .get(*pos..*pos + len)
        .ok_or_else(|| AppError::Telegram("Telegram file_id 的 bytes 内容不完整".into()))?
        .to_vec();
    *pos += len;
    let padding = (4 - ((header_len + len) % 4)) % 4;
    data.get(*pos..*pos + padding)
        .ok_or_else(|| AppError::Telegram("Telegram file_id 的 bytes 对齐不完整".into()))?;
    *pos += padding;
    Ok(value)
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    version: u8,
    home_dc: i32,
    dc_options: Vec<(i32, DcOption)>,
    peer_infos: Vec<(PeerId, PeerInfo)>,
    updates_state: UpdatesState,
}

struct SessionState {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
    peer_infos: HashMap<PeerId, PeerInfo>,
    updates_state: UpdatesState,
}

impl Default for SessionState {
    fn default() -> Self {
        let data = SessionData::default();
        Self {
            home_dc: data.home_dc,
            dc_options: data.dc_options,
            peer_infos: data.peer_infos,
            updates_state: data.updates_state,
        }
    }
}

impl SessionState {
    fn from_stored(stored: StoredSession) -> Result<Self, SessionError> {
        if stored.version != SESSION_VERSION {
            return Err(SessionError::Invalid(format!(
                "不支持的会话版本 {}",
                stored.version
            )));
        }
        // 合并当前库内置 DC 地址，持久化值（含 auth_key）优先。
        let mut dc_options = Self::default().dc_options;
        dc_options.extend(stored.dc_options);
        Ok(Self {
            home_dc: stored.home_dc,
            dc_options,
            peer_infos: stored.peer_infos.into_iter().collect(),
            updates_state: stored.updates_state,
        })
    }

    fn stored(&self) -> StoredSession {
        StoredSession {
            version: SESSION_VERSION,
            home_dc: self.home_dc,
            dc_options: self
                .dc_options
                .iter()
                .map(|(id, option)| (*id, option.clone()))
                .collect(),
            peer_infos: self
                .peer_infos
                .iter()
                .map(|(id, info)| (*id, info.clone()))
                .collect(),
            updates_state: self.updates_state.clone(),
        }
    }
}

/// 将 grammers 会话状态存入项目已有 settings 表，避免每次重启重新登录。
struct PersistentSession {
    db: Db,
    key: String,
    state: Mutex<SessionState>,
    persist_lock: tokio::sync::Mutex<()>,
    proxy: ProxyConfig,
    tunnels: Mutex<HashMap<SocketAddrV4, DcTunnel>>,
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("会话锁已损坏")]
    Poisoned,
    #[error("会话序列化失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("会话数据库写入失败: {0}")]
    Database(String),
    #[error("会话数据无效: {0}")]
    Invalid(String),
    #[error("MTProto 代理隧道不可用: {0}")]
    Tunnel(#[from] std::io::Error),
}

impl PersistentSession {
    async fn load(db: Db, key: String) -> AppResult<Self> {
        let state = match db.setting_get(&key).await? {
            Some(json) => {
                let stored: StoredSession = serde_json::from_str(&json).map_err(|error| {
                    AppError::Internal(anyhow::anyhow!(
                        "MTProto 持久会话无法解析（{key}）: {error}"
                    ))
                })?;
                SessionState::from_stored(stored).map_err(|error| {
                    AppError::Internal(anyhow::anyhow!("MTProto 持久会话无效（{key}）: {error}"))
                })?
            }
            None => SessionState::default(),
        };
        Ok(Self {
            db,
            key,
            state: Mutex::new(state),
            persist_lock: tokio::sync::Mutex::new(()),
            proxy: ProxyConfig::default(),
            tunnels: Mutex::new(HashMap::new()),
        })
    }

    fn state(&self) -> Result<MutexGuard<'_, SessionState>, SessionError> {
        self.state.lock().map_err(|_| SessionError::Poisoned)
    }

    async fn persist(&self) -> Result<(), SessionError> {
        // 串行写入，并在拿到写锁后重新制作最新快照，避免并发更新倒序落库。
        let _guard = self.persist_lock.lock().await;
        let json = {
            let state = self.state()?;
            serde_json::to_string(&state.stored())?
        };
        self.db
            .setting_set(&self.key, &json)
            .await
            .map_err(|error| SessionError::Database(error.to_string()))
    }
}

impl Session for PersistentSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self.state()?.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            self.state()?.home_dc = dc_id;
            self.persist().await
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        let Some(mut option) = self.state()?.dc_options.get(&dc_id).cloned() else {
            return Ok(None);
        };
        if self.proxy.enabled {
            let mut tunnels = self.tunnels.lock().map_err(|_| SessionError::Poisoned)?;
            if let std::collections::hash_map::Entry::Vacant(entry) = tunnels.entry(option.ipv4) {
                entry.insert(DcTunnel::new(&self.proxy, option.ipv4)?);
            }
            option.ipv4 = tunnels[&option.ipv4].address;
        }
        Ok(Some(option))
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let mut dc_option = dc_option.clone();
        Box::pin(async move {
            // grammers 会把查询所得的 DC 地址连同新 auth_key 写回，必须还原真实地址。
            {
                let tunnels = self.tunnels.lock().map_err(|_| SessionError::Poisoned)?;
                if let Some((destination, _)) = tunnels
                    .iter()
                    .find(|(_, tunnel)| tunnel.address == dc_option.ipv4)
                {
                    dc_option.ipv4 = *destination;
                }
            }
            self.state()?.dc_options.insert(dc_option.id, dc_option);
            self.persist().await
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move { Ok(self.state()?.peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        let peer = peer.clone();
        Box::pin(async move {
            self.state()?
                .peer_infos
                .entry(peer.id())
                .or_insert_with(|| peer.clone())
                .extend_info(&peer);
            self.persist().await
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move { Ok(self.state()?.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            {
                let mut state = self.state()?;
                match update {
                    UpdateState::All(updates_state) => state.updates_state = updates_state,
                    UpdateState::Primary { pts, date, seq } => {
                        state.updates_state.pts = pts;
                        state.updates_state.date = date;
                        state.updates_state.seq = seq;
                    }
                    UpdateState::Secondary { qts } => state.updates_state.qts = qts,
                    UpdateState::Channel { id, pts } => {
                        state
                            .updates_state
                            .channels
                            .retain(|channel| channel.id != id);
                        state.updates_state.channels.push(ChannelState { id, pts });
                    }
                }
            }
            self.persist().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    struct ActiveFetch(Arc<AtomicUsize>);

    impl Drop for ActiveFetch {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn parallel_reads_preserve_range_bytes_and_bound_concurrency() {
        let part_size = REQUEST_SIZE as usize;
        let data = Arc::new(
            (0..part_size * 10 + 137)
                .map(|i| (i % 251) as u8)
                .collect::<Vec<_>>(),
        );
        for concurrency in [1, 4, 8] {
            for offset in [0, 19, part_size + 31, data.len() - 7] {
                let active = Arc::new(AtomicUsize::new(0));
                let peak = Arc::new(AtomicUsize::new(0));
                let slots = Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY));
                let mut reader = open_parallel_stream(
                    data.len() as u64,
                    offset as u64,
                    concurrency,
                    slots.clone(),
                    {
                        let data = data.clone();
                        let active = active.clone();
                        let peak = peak.clone();
                        move |part_offset| {
                            let data = data.clone();
                            let active = active.clone();
                            let peak = peak.clone();
                            async move {
                                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                                let _guard = ActiveFetch(active);
                                peak.fetch_max(now, Ordering::SeqCst);
                                // 故意让靠前的分段更慢，验证输出顺序不受完成顺序影响。
                                let delay = if (part_offset / REQUEST_SIZE as u64).is_multiple_of(4)
                                {
                                    8
                                } else {
                                    1
                                };
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                                let start = part_offset as usize;
                                Ok(data[start..(start + part_size).min(data.len())].to_vec())
                            }
                        }
                    },
                )
                .await
                .unwrap();
                let mut actual = Vec::new();
                reader.read_to_end(&mut actual).await.unwrap();
                assert_eq!(actual, data[offset..]);
                assert!(peak.load(Ordering::SeqCst) <= concurrency);
                if offset == 0 {
                    assert_eq!(peak.load(Ordering::SeqCst), concurrency);
                }
                assert_eq!(active.load(Ordering::SeqCst), 0);
                assert_eq!(slots.available_permits(), MAX_DOWNLOAD_CONCURRENCY);
            }
        }
    }

    #[tokio::test]
    async fn pausing_bounds_prefetch_and_dropping_cancels_pending_reads() {
        for concurrency in [1, 4, 8] {
            let started = Arc::new(AtomicUsize::new(0));
            let active = Arc::new(AtomicUsize::new(0));
            let slots = Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY));
            let mut reader =
                open_parallel_stream(REQUEST_SIZE as u64 * 100, 0, concurrency, slots.clone(), {
                    let started = started.clone();
                    let active = active.clone();
                    move |part_offset| {
                        let started = started.clone();
                        let active = active.clone();
                        async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            active.fetch_add(1, Ordering::SeqCst);
                            let _guard = ActiveFetch(active);
                            if part_offset == 0 {
                                tokio::task::yield_now().await;
                                Ok(vec![7; REQUEST_SIZE as usize])
                            } else {
                                std::future::pending().await
                            }
                        }
                    }
                })
                .await
                .unwrap();
            let mut prefix = [0; 37];
            reader.read_exact(&mut prefix).await.unwrap();
            assert_eq!(prefix, [7; 37]);
            tokio::task::yield_now().await;
            assert_eq!(started.load(Ordering::SeqCst), concurrency);
            assert_eq!(active.load(Ordering::SeqCst), concurrency - 1);
            // 模拟关闭预览或 seek 丢弃旧流；所有未完成分段及其并发许可立即释放。
            drop(reader);
            assert_eq!(active.load(Ordering::SeqCst), 0);
            assert_eq!(slots.available_permits(), MAX_DOWNLOAD_CONCURRENCY);
            assert_eq!(started.load(Ordering::SeqCst), concurrency);
        }
    }

    #[tokio::test]
    async fn multiple_streams_share_the_bot_request_limit() {
        let slots = Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY));
        let active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let readers = (0..3).map(|_| {
            open_parallel_stream(REQUEST_SIZE as u64 * 100, 0, 8, slots.clone(), {
                let active = active.clone();
                let started = started.clone();
                move |_| {
                    let active = active.clone();
                    let started = started.clone();
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        active.fetch_add(1, Ordering::SeqCst);
                        let _guard = ActiveFetch(active);
                        std::future::pending().await
                    }
                }
            })
        });
        let mut opening = Box::pin(futures_util::future::join_all(readers));
        assert!(futures_util::poll!(opening.as_mut()).is_pending());
        assert_eq!(started.load(Ordering::SeqCst), MAX_DOWNLOAD_CONCURRENCY);
        assert_eq!(active.load(Ordering::SeqCst), MAX_DOWNLOAD_CONCURRENCY);
        drop(opening);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(slots.available_permits(), MAX_DOWNLOAD_CONCURRENCY);
    }

    #[tokio::test]
    async fn truncated_parts_and_network_errors_are_reported() {
        for first_bad in [true, false] {
            let reader = open_parallel_stream(
                REQUEST_SIZE as u64 + 13,
                0,
                4,
                Arc::new(Semaphore::new(8)),
                move |offset| async move {
                    if offset == 0 {
                        Ok(vec![0; REQUEST_SIZE as usize - usize::from(first_bad)])
                    } else {
                        Ok(vec![0; 12])
                    }
                },
            )
            .await;
            if first_bad {
                assert!(
                    matches!(reader, Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
                );
            } else {
                let error = reader
                    .unwrap()
                    .read_to_end(&mut Vec::new())
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
            }
        }
        let mut reader = open_parallel_stream(
            REQUEST_SIZE as u64 * 3,
            0,
            4,
            Arc::new(Semaphore::new(8)),
            |offset| async move {
                if offset == 0 {
                    Ok(vec![0; REQUEST_SIZE as usize])
                } else {
                    Err(AppError::Telegram("注入下载错误".into()))
                }
            },
        )
        .await
        .unwrap();
        let mut actual = Vec::new();
        let error = reader.read_to_end(&mut actual).await.unwrap_err();
        assert!(error.to_string().contains("注入下载错误"));
        assert_eq!(actual.len(), REQUEST_SIZE as usize);
    }

    #[tokio::test]
    async fn eof_and_invalid_offsets_do_not_fetch_data() {
        let calls = Arc::new(AtomicU64::new(0));
        for offset in [17, 18] {
            let result = open_parallel_stream(17, offset, 4, Arc::new(Semaphore::new(8)), {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok(Vec::new()) }
                }
            })
            .await;
            if offset == 17 {
                assert_eq!(result.unwrap().read(&mut [0; 1]).await.unwrap(), 0);
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn proxy_dc_addresses_are_reused_but_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("proxy-session.db").to_str().unwrap())
            .await
            .unwrap();
        let key = "test.proxy.session";
        let mut session = PersistentSession::load(db.clone(), key.into())
            .await
            .unwrap();
        let dc = session.home_dc_id().unwrap();
        let original = session.dc_option(dc).unwrap().unwrap();
        session.proxy = ProxyConfig::new(true, "http://proxy-test.invalid:7890").unwrap();
        let mut proxied = session.dc_option(dc).unwrap().unwrap();
        assert!(proxied.ipv4.ip().is_loopback());
        assert_ne!(proxied.ipv4, original.ipv4);
        assert_eq!(session.dc_option(dc).unwrap().unwrap().ipv4, proxied.ipv4);
        assert_eq!(session.tunnels.lock().unwrap().len(), 1);
        proxied.auth_key = Some([7; 256]);
        session.set_dc_option(&proxied).await.unwrap();

        let reloaded = PersistentSession::load(db, key.into()).await.unwrap();
        let saved = reloaded.dc_option(dc).unwrap().unwrap();
        assert_eq!(saved.ipv4, original.ipv4);
        assert_eq!(saved.ipv6, original.ipv6);
        assert_eq!(saved.auth_key, proxied.auth_key);
        assert!(reloaded.tunnels.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn historical_targets_for_one_bot_share_request_slots() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("limits.db").to_str().unwrap())
            .await
            .unwrap();
        let config = MtprotoConfig::from_values(db, "1", "test-hash")
            .unwrap()
            .unwrap();
        let network = Arc::new(ProxyClient::new(ProxyConfig::default()).unwrap());
        let first = MtprotoClient::new(config.clone(), 1, "same-bot", network.clone());
        let historical = MtprotoClient::new(config.clone(), 2, "same-bot", network.clone());
        let another = MtprotoClient::new(config.clone(), 3, "another-bot", network);
        assert!(Arc::ptr_eq(&first.request_slots, &historical.request_slots));
        assert!(!Arc::ptr_eq(&first.request_slots, &another.request_slots));
        config.set_download_concurrency(8);
        assert_eq!(first.config.download_concurrency(), 8);
        assert_eq!(historical.config.download_concurrency(), 8);
    }

    #[tokio::test]
    async fn short_ranges_only_fetch_the_parts_they_cover() {
        let part_size = REQUEST_SIZE as u64;
        for (offset, length) in [(13, 1), (part_size - 7, 29), (part_size + 31, 17)] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut reader =
                open_parallel_stream(offset + length, offset, 8, Arc::new(Semaphore::new(8)), {
                    let requests = requests.clone();
                    move |start| {
                        requests.lock().unwrap().push(start);
                        async move {
                            Ok((start..start + part_size)
                                .map(|i| (i % 251) as u8)
                                .collect())
                        }
                    }
                })
                .await
                .unwrap();
            let mut actual = Vec::new();
            reader.read_to_end(&mut actual).await.unwrap();
            assert_eq!(
                actual,
                (offset..offset + length)
                    .map(|i| (i % 251) as u8)
                    .collect::<Vec<_>>()
            );
            let expected = (offset / part_size..(offset + length).div_ceil(part_size))
                .map(|i| i * part_size)
                .collect::<Vec<_>>();
            assert_eq!(*requests.lock().unwrap(), expected);
        }
    }

    fn encode_file_id(file_type: i32, file_reference: &[u8], id: i64, hash: i64) -> String {
        let mut raw = Vec::new();
        raw.extend_from_slice(&(file_type | FILE_REFERENCE_FLAG).to_le_bytes());
        raw.extend_from_slice(&4i32.to_le_bytes());
        raw.push(file_reference.len() as u8);
        raw.extend_from_slice(file_reference);
        raw.resize(raw.len().div_ceil(4) * 4, 0);
        raw.extend_from_slice(&id.to_le_bytes());
        raw.extend_from_slice(&hash.to_le_bytes());
        // file_id 格式尾部的版本号不参与 location 解析。
        raw.extend_from_slice(&[4, 0, 0, 0, 61, 0, 0, 0]);

        let mut packed = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            if raw[pos] == 0 {
                let start = pos;
                while pos < raw.len() && raw[pos] == 0 && pos - start < u8::MAX as usize {
                    pos += 1;
                }
                packed.extend_from_slice(&[0, (pos - start) as u8]);
            } else {
                packed.push(raw[pos]);
                pos += 1;
            }
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(packed)
    }

    #[test]
    fn document_file_id_can_be_decoded() {
        let file_id = encode_file_id(4, &[1, 2, 3], 123, 456);
        let location = decode_document(&file_id, 99).unwrap().unwrap();
        assert_eq!(location.size, 99);
        match location.raw {
            tl::enums::InputFileLocation::InputDocumentFileLocation(location) => {
                assert_eq!(location.id, 123);
                assert_eq!(location.access_hash, 456);
                assert_eq!(location.file_reference, vec![1, 2, 3]);
            }
            _ => panic!("应解析为文档 location"),
        }
    }

    #[test]
    fn photo_file_id_is_left_for_bot_api() {
        let file_id = encode_file_id(2, &[], 123, 456);
        assert!(decode_document(&file_id, 99).unwrap().is_none());
    }

    #[tokio::test]
    async fn session_state_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("session.db").to_str().unwrap())
            .await
            .unwrap();
        let session = PersistentSession::load(db.clone(), "test.session".into())
            .await
            .unwrap();
        session.set_home_dc_id(4).await.unwrap();

        let reloaded = PersistentSession::load(db, "test.session".into())
            .await
            .unwrap();
        assert_eq!(reloaded.home_dc_id().unwrap(), 4);
    }
}
