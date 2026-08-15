//! 通过 MTProto 按需读取 Telegram 文件。
//!
//! 本地 Bot API 的 `getFile` 会在返回路径前先下载完整文件，不适合浏览器 Range
//! 播放。这里直接解析 Bot API `file_id`，每次最多向 Telegram 请求 512 KiB，
//! 再通过有背压的管道交给现有 `ChunkedReader`。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::telegram::ByteReader;
use base64::Engine as _;
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
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;

/// Telegram 单次 `upload.getFile` 请求允许的最大值。
const REQUEST_SIZE: i32 = 512 * 1024;
/// 小管道用于制造背压，避免播放器暂停时继续预取很多数据。
const PIPE_CAPACITY: usize = 64 * 1024;
const SESSION_VERSION: u8 = 1;

const WEB_LOCATION_FLAG: i32 = 1 << 24;
const FILE_REFERENCE_FLAG: i32 = 1 << 25;

/// 启动时固定的 MTProto 应用配置。api_id/api_hash 改动后需重启 tgdrive。
#[derive(Clone)]
pub struct MtprotoConfig {
    db: Db,
    api_id: i32,
    api_hash: Arc<str>,
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
        }))
    }
}

/// 每个不可变存储目标各自持有一个 Bot MTProto 会话。
pub struct MtprotoClient {
    config: MtprotoConfig,
    storage_target_id: i64,
    bot_token: Arc<str>,
    state: OnceCell<ClientState>,
}

struct ClientState {
    client: Client,
    handle: SenderPoolFatHandle,
    /// 保留任务句柄；连接由 `handle.quit()` 结束。
    _runner_task: tokio::task::JoinHandle<()>,
}

impl Drop for ClientState {
    fn drop(&mut self) {
        self.handle.quit();
    }
}

impl MtprotoClient {
    pub fn new(config: MtprotoConfig, storage_target_id: i64, bot_token: &str) -> Self {
        Self {
            config,
            storage_target_id,
            bot_token: Arc::from(bot_token),
            state: OnceCell::new(),
        }
    }

    async fn state(&self) -> AppResult<&ClientState> {
        self.state
            .get_or_try_init(|| async {
                let key = format!("mtproto.session.{}", self.storage_target_id);
                let session = Arc::new(PersistentSession::load(self.config.db.clone(), key).await?);
                let SenderPool { runner, handle, .. } =
                    SenderPool::new(session, self.config.api_id);
                let client = Client::new(handle.clone());
                let runner_task = tokio::spawn(runner.run());

                let authorized = match client.is_authorized().await {
                    Ok(value) => value,
                    Err(error) => {
                        handle.quit();
                        runner_task.abort();
                        return Err(mtproto_error("检查登录状态失败", error));
                    }
                };
                if !authorized
                    && let Err(error) = client
                        .bot_sign_in(&self.bot_token, &self.config.api_hash)
                        .await
                {
                    handle.quit();
                    runner_task.abort();
                    return Err(mtproto_error("Bot 登录失败", error));
                }

                tracing::info!(
                    storage_target_id = self.storage_target_id,
                    "MTProto 流式下载会话已就绪"
                );
                Ok(ClientState {
                    client,
                    handle,
                    _runner_task: runner_task,
                })
            })
            .await
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
    ) -> AppResult<Option<ByteReader>> {
        if offset > size {
            return Err(AppError::Telegram(format!(
                "分块偏移 {offset} 超过分块大小 {size}"
            )));
        }
        if offset == size {
            return Ok(Some(Box::pin(std::io::Cursor::new(Vec::<u8>::new()))));
        }

        let Some(location) = decode_document(file_id, size)? else {
            return Ok(None);
        };
        let state = self.state().await?;

        // Telegram 要求 offset 与请求块大小对齐；只多取当前位置之前不足 512 KiB 的前缀。
        let aligned = offset / REQUEST_SIZE as u64 * REQUEST_SIZE as u64;
        let skip = usize::try_from(offset - aligned)
            .map_err(|_| AppError::Telegram("分块偏移无法转换".into()))?;
        let skip_chunks = i32::try_from(aligned / REQUEST_SIZE as u64)
            .map_err(|_| AppError::Telegram("分块偏移过大".into()))?;
        let mut download = state
            .client
            .iter_download(&location)
            .chunk_size(REQUEST_SIZE)
            .skip_chunks(skip_chunks);

        // 首次请求在返回 HTTP body 前完成，使解析、鉴权和首块网络错误能正常返回。
        let first = download
            .next()
            .await
            .map_err(|error| mtproto_error("读取首个数据块失败", error))?
            .ok_or_else(|| AppError::Telegram("Telegram 未返回分块数据".into()))?;
        if skip >= first.len() {
            return Err(AppError::Telegram(
                "Telegram 返回的首个数据块比请求偏移短".into(),
            ));
        }

        let mut remaining = size - offset;
        let first_len = (first.len() - skip).min(remaining as usize);
        let first = first[skip..skip + first_len].to_vec();
        remaining -= first_len as u64;

        let (mut writer, reader) = tokio::io::duplex(PIPE_CAPACITY);
        let storage_target_id = self.storage_target_id;
        tokio::spawn(async move {
            if writer.write_all(&first).await.is_err() {
                return;
            }
            while remaining > 0 {
                let bytes = match download.next().await {
                    Ok(Some(bytes)) if !bytes.is_empty() => bytes,
                    Ok(_) => {
                        tracing::warn!(
                            storage_target_id,
                            message_id,
                            remaining,
                            "MTProto 分块提前结束"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            storage_target_id,
                            message_id,
                            error = %error,
                            "MTProto 分段读取中断"
                        );
                        return;
                    }
                };
                let take = bytes.len().min(remaining as usize);
                // HTTP 客户端关闭预览后 reader 会被丢弃，此处立刻失败并停止后续请求。
                if writer.write_all(&bytes[..take]).await.is_err() {
                    return;
                }
                remaining -= take as u64;
            }
            let _ = writer.shutdown().await;
        });

        Ok(Some(Box::pin(reader)))
    }
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
        Ok(self.state()?.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
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
