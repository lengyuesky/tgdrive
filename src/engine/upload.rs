//! 流式分块上传管线
//!
//! 生产者(HTTP 上传 / WebDAV PUT)通过 mpsc 通道喂入字节,管线在独立任务中
//! 攒块并发上传;生产者中途消失(未发 Eof 就断开)视为失败,自动回滚已传分块。

use crate::db::{Db, Node, StoredMessage, UploadPartClaim, UploadSessionInfo};
use crate::error::{AppError, AppResult};
use crate::telegram::{TgApi, TgBinding, TgMessage, TgRegistry, UploadBody};
use bytes::{Bytes, BytesMut};
use futures_util::future::{AbortHandle, Abortable};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc};
use tokio::task::JoinSet;

pub enum UploadMsg {
    Data(Bytes),
    /// 输入正常结束(与「通道被丢弃」区分,后者按失败回滚)
    Eof,
}

/// 字节 → 十六进制(sha256 入库格式)
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 按扩展名判断能否作为 Telegram 视频消息发送,返回对应 MIME。
/// 只收 mp4 家族:Telegram 客户端仅保证播放 MPEG-4 视频,
/// mkv/webm 常见的 HEVC/VP9 轨道在手机端只有声音没画面,发视频消息反而更糟。
pub fn video_mime(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" => Some("video/mp4"),
        "m4v" => Some("video/x-m4v"),
        "mov" => Some("video/quicktime"),
        _ => None,
    }
}

/// 后台日志不能影响实际任务；数据库暂时不可写时保留 tracing 诊断即可。
async fn record_backend_log(db: &Db, level: &str, category: &str, message: &str) {
    if let Err(error) = db.append_backend_log(level, category, message).await {
        tracing::warn!("记录后台日志失败: {error}");
    }
}

async fn send_body_to_telegram(
    tg: Arc<dyn TgApi>,
    filename: &str,
    mime: Option<&str>,
    thumbnail: Option<&Bytes>,
    body: &UploadBody,
    upstream_pending: &AtomicU64,
    persisting_stage: Option<&AtomicU8>,
) -> AppResult<TgMessage> {
    // 等待共享发送时隙时尚未向 Bot API 发请求，因此必须先等待、再标记在途；
    // 用户在限流倒计时中取消任务不会误触发 Bot API 重启。
    tg.wait_upload_turn().await;
    // 只有请求确定返回后才清零。future 被取消或结果不确定时保持非零，
    // 外层据此重启自建 Bot API，终止它已经接手的 TDLib 上传。
    upstream_pending.fetch_add(1, Ordering::AcqRel);
    let result = match mime {
        Some(mime) => {
            // 从文件头解析宽高/时长；解析不了就让 Telegram 使用默认显示比例。
            let meta = crate::engine::probe::probe_body(mime, body).await;
            if meta.is_none() {
                tracing::debug!("{filename} 未解析出视频宽高，按无元数据发送");
            }
            match tg
                .send_video(filename, mime, meta, None, thumbnail, body)
                .await
            {
                Ok(message) => Ok(message),
                // 只有 Telegram 明确拒绝格式时才安全降级；不确定结果不能重复发送。
                Err(AppError::TelegramRejected(detail)) => {
                    tracing::warn!("{filename} 以视频消息发送失败({detail})，回退为文档");
                    upstream_pending.fetch_sub(1, Ordering::AcqRel);
                    tg.wait_upload_turn().await;
                    upstream_pending.fetch_add(1, Ordering::AcqRel);
                    tg.send_document(filename, None, thumbnail, body).await
                }
                Err(error) => Err(error),
            }
        }
        None => tg.send_document(filename, None, thumbnail, body).await,
    };
    // 成功响应后必须先进入落库阶段，再清除在途标记。这样另一个取消任务
    // 在重启共享 Bot API 时不会误杀“消息已经创建、只差落库”的分块。
    if result.is_ok()
        && let Some(stage) = persisting_stage
    {
        stage.store(PART_STAGE_PERSISTING, Ordering::Release);
    }
    if !matches!(&result, Err(AppError::TelegramUncertain(_))) {
        upstream_pending.fetch_sub(1, Ordering::AcqRel);
    }
    result
}

pub struct UploadHandle {
    tx: mpsc::Sender<UploadMsg>,
    join: tokio::task::JoinHandle<AppResult<Node>>,
    cancel: AbortHandle,
}

pub struct ResumablePartHandle {
    tx: mpsc::Sender<UploadMsg>,
    join: tokio::task::JoinHandle<AppResult<()>>,
    cancel: AbortHandle,
    cancel_reason: Arc<AtomicU8>,
}

impl ResumablePartHandle {
    pub async fn feed(&self, data: Bytes) -> AppResult<()> {
        self.tx
            .send(UploadMsg::Data(data))
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("分块上传管线已终止")))
    }

    pub async fn finish(mut self) -> AppResult<()> {
        let _ = self.tx.send(UploadMsg::Eof).await;
        (&mut self.join)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("分块上传任务崩溃: {e}")))?
    }
}

impl Drop for ResumablePartHandle {
    fn drop(&mut self) {
        // JoinHandle 被丢弃只会让任务脱离等待，不会停止它；必须显式取消。
        let _ = self.cancel_reason.compare_exchange(
            PART_CANCEL_NONE,
            PART_CANCEL_USER,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.cancel.abort();
    }
}

pub enum ResumablePartStart {
    Complete,
    Upload(ResumablePartHandle),
}

impl UploadHandle {
    pub async fn feed(&self, data: Bytes) -> AppResult<()> {
        self.tx
            .send(UploadMsg::Data(data))
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("上传管线已终止")))
    }

    /// 宣告输入结束并等待管线收尾,返回最终节点
    pub async fn finish(mut self) -> AppResult<Node> {
        let _ = self.tx.send(UploadMsg::Eof).await;
        (&mut self.join)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("上传任务崩溃: {e}")))?
    }
}

impl Drop for UploadHandle {
    fn drop(&mut self) {
        self.cancel.abort();
    }
}

/// 可在运行时热调的上传参数。放进 `Arc` 共享,后台改配置后
/// 已被克隆的 Uploader(含 DavHandler 内部那份)立即看到新值。
pub struct UploadKnobs {
    chunk_size: AtomicU64,
    tmp_dir: RwLock<PathBuf>,
    /// 改并发度即整体替换信号量;在途任务持旧信号量正常收尾
    semaphore: RwLock<Arc<Semaphore>>,
    /// 单块以内的视频改用 sendVideo 发送(Telegram 里可直接播放)
    send_video: AtomicBool,
    /// 关掉后 Telegram 消息清理只入队不自动消费,等后台手动执行
    auto_delete_message: AtomicBool,
}

impl UploadKnobs {
    pub fn new(chunk_size: u64, tmp_dir: PathBuf, concurrency: usize, send_video: bool) -> Self {
        UploadKnobs {
            chunk_size: AtomicU64::new(chunk_size),
            tmp_dir: RwLock::new(tmp_dir),
            semaphore: RwLock::new(Arc::new(Semaphore::new(concurrency))),
            send_video: AtomicBool::new(send_video),
            // 默认手动：消息清理任务照常入队，由管理员决定何时执行。
            auto_delete_message: AtomicBool::new(false),
        }
    }

    pub fn auto_delete_message(&self) -> bool {
        self.auto_delete_message.load(Ordering::Relaxed)
    }

    pub fn set_auto_delete_message(&self, v: bool) {
        self.auto_delete_message.store(v, Ordering::Relaxed);
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size.load(Ordering::Relaxed)
    }

    pub fn set_chunk_size(&self, v: u64) {
        self.chunk_size.store(v, Ordering::Relaxed);
    }

    pub fn send_video(&self) -> bool {
        self.send_video.load(Ordering::Relaxed)
    }

    pub fn set_send_video(&self, v: bool) {
        self.send_video.store(v, Ordering::Relaxed);
    }

    /// 取值均先 clone 出来,guard 不跨 await
    pub fn tmp_dir(&self) -> PathBuf {
        self.tmp_dir.read().unwrap().clone()
    }

    pub fn set_tmp_dir(&self, v: PathBuf) {
        *self.tmp_dir.write().unwrap() = v;
    }

    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.read().unwrap().clone()
    }

    pub fn set_concurrency(&self, n: usize) {
        *self.semaphore.write().unwrap() = Arc::new(Semaphore::new(n));
    }
}

/// 这一轮队列处理是谁触发的。手动触发要绕开指数退避——重试过十几次的任务
/// 退避已经拉到一小时，不绕开的话后台点「立即执行」会像没反应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueTrigger {
    Auto,
    Manual,
}

impl QueueTrigger {
    fn ignores_backoff(self) -> bool {
        self == QueueTrigger::Manual
    }

    /// 手动执行时有人在网页那头等着，给一轮设个时间上限：Telegram 慢或不可达时
    /// 200 条挨个超时能拖上好几分钟。到点收工，剩余量交给用户再点一次。
    /// 自动 worker 在后台跑，没人等，不设限。
    fn budget(self) -> Option<std::time::Duration> {
        match self {
            QueueTrigger::Auto => None,
            QueueTrigger::Manual => Some(std::time::Duration::from_secs(20)),
        }
    }

    /// 本轮预算还剩多少，用来给单条任务的 Telegram 调用封顶；`None` = 不限时。
    /// 已经超支时返回 `Some(0)`，调用方会立刻判超时收工。
    fn remaining(self, started: std::time::Instant) -> Option<std::time::Duration> {
        self.budget()
            .map(|budget| budget.saturating_sub(started.elapsed()))
    }

    /// 手动模式下没有下一次自动重试，日志不能承诺“将自动重试”
    fn retry_hint(self) -> &'static str {
        match self {
            QueueTrigger::Auto => "，将自动重试",
            QueueTrigger::Manual => "",
        }
    }
}

/// 一轮队列处理的结果，既回给后台「立即执行」展示，也用于自动模式写日志。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QueueRunSummary {
    pub completed: usize,
    pub failed: usize,
    /// Telegram 已确认不可恢复，已安全退出队列的任务数。
    pub discarded: usize,
    /// 这一轮结束后队列里还剩多少条
    pub pending: i64,
    /// 首个失败原因，后台直接显示，省得再去翻日志
    pub last_error: Option<String>,
    /// 是否因为超时提前收工；为真时前端提示可以再点一次继续
    pub stopped_early: bool,
}

impl QueueRunSummary {
    /// 本轮实际动过的任务数；为 0 表示没有可处理的任务
    fn attempted(&self) -> usize {
        self.completed + self.failed + self.discarded
    }

    fn note_error(&mut self, error: &str) {
        if self.last_error.is_none() {
            self.last_error = Some(error.chars().take(300).collect());
        }
    }
}

const TELEGRAM_DELETE_WINDOW_SECS: i64 = 48 * 60 * 60;

fn is_expired_undeletable_message(job: &crate::db::DeleteJob, error: &AppError) -> bool {
    matches!(error, AppError::Telegram(detail) if detail.contains("message can't be deleted"))
        && crate::db::now_ts().saturating_sub(job.created_at) >= TELEGRAM_DELETE_WINDOW_SECS
}

#[cfg(test)]
mod queue_failure_tests {
    use super::*;

    #[test]
    fn only_expired_cannot_delete_error_is_permanent() {
        let now = crate::db::now_ts();
        let old = crate::db::DeleteJob {
            id: 1,
            storage_target_id: 1,
            tg_message_id: 33,
            attempts: 3,
            created_at: now - TELEGRAM_DELETE_WINDOW_SECS,
        };
        let recent = crate::db::DeleteJob {
            created_at: now - 60,
            ..old.clone()
        };
        let denied = AppError::Telegram("[400] Bad Request: message can't be deleted".to_string());
        let offline = AppError::Telegram("deleteMessage: 连接失败".to_string());

        assert!(is_expired_undeletable_message(&old, &denied));
        assert!(!is_expired_undeletable_message(&recent, &denied));
        assert!(!is_expired_undeletable_message(&old, &offline));
    }
}

#[derive(Clone)]
pub struct Uploader {
    pub tg: Arc<TgRegistry>,
    pub db: Db,
    pub knobs: Arc<UploadKnobs>,
    /// 分块超过该值转入临时文件(生产用 MEMORY_CHUNK_LIMIT,测试可调小)
    pub mem_limit: u64,
    delete_running: Arc<AtomicBool>,
    active_resumable: Arc<Mutex<ActiveResumableUploads>>,
    bot_restart_lock: Arc<AsyncMutex<()>>,
    bot_restart_generation: Arc<AtomicU64>,
}

const PART_STAGE_RECEIVING: u8 = 0;
const PART_STAGE_SENDING: u8 = 1;
const PART_STAGE_PERSISTING: u8 = 2;

const PART_CANCEL_NONE: u8 = 0;
const PART_CANCEL_USER: u8 = 1;
const PART_CANCEL_BOT_RESTART: u8 = 2;
const PART_CANCEL_RESTART_FAILED: u8 = 3;

#[derive(Clone)]
struct ActiveResumablePart {
    cancel: AbortHandle,
    cancel_reason: Arc<AtomicU8>,
    stage: Arc<AtomicU8>,
}

#[derive(Default)]
struct ActiveResumableUploads {
    parts: HashMap<(String, i64), ActiveResumablePart>,
    /// 防止取消接口与同一会话的新分块请求交错，造成“删会话后仍开始上传”。
    cancelling_sessions: HashSet<String>,
}

impl Uploader {
    pub fn new(tg: Arc<TgRegistry>, db: Db, knobs: Arc<UploadKnobs>, mem_limit: u64) -> Self {
        Uploader {
            tg,
            db,
            knobs,
            mem_limit,
            delete_running: Arc::new(AtomicBool::new(false)),
            active_resumable: Arc::new(Mutex::new(ActiveResumableUploads::default())),
            bot_restart_lock: Arc::new(AsyncMutex::new(())),
            bot_restart_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 启动一次上传:先建 uploading 占位节点,返回喂数据的句柄
    pub async fn start(
        &self,
        parent_id: i64,
        name: &str,
        display_path: &str,
        thumbnail: Option<Bytes>,
    ) -> AppResult<UploadHandle> {
        // 一个文件从池中选择一次目标并固定下来；后台改配置只影响之后开始的上传。
        let target = self.tg.next_upload()?;
        let node_id = self.db.create_uploading(parent_id, name).await?;
        let (tx, rx) = mpsc::channel::<UploadMsg>(4);
        let this = self.clone();
        let name = name.to_string();
        let display_path = display_path.to_string();
        let (cancel, abort_registration) = AbortHandle::new_pair();
        let cancel_api = target.api.clone();
        let restart_generation = self.bot_restart_generation.load(Ordering::Acquire);
        let upstream_pending = Arc::new(AtomicU64::new(0));
        let pipeline_pending = upstream_pending.clone();
        let join = tokio::spawn(async move {
            let result = match Abortable::new(
                this.pipeline(node_id, &name, thumbnail, target, pipeline_pending, rx),
                abort_registration,
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(AppError::BadRequest("上传已取消".into())),
            };
            // 与可恢复分块共用重启协调：持锁、按代次去重、多 Bot 池不重启，
            // 被波及的可恢复分块标为可安全重试，而不是收到模糊错误。
            this.restart_detached_resumable_upload(
                &cancel_api,
                &upstream_pending,
                restart_generation,
                &(String::new(), -1),
                &display_path,
            )
            .await;
            match result {
                Ok(node) => Ok(node),
                Err(e) => {
                    tracing::warn!("上传 {display_path} 失败,回滚: {e}");
                    match this.db.abort_upload(node_id).await {
                        Ok(freed) => this.delete_messages_best_effort(freed),
                        Err(e2) => tracing::error!("回滚上传记录失败: {e2}"),
                    }
                    Err(e)
                }
            }
        });
        Ok(UploadHandle { tx, join, cancel })
    }

    /// 为一个可恢复会话启动独立分块请求。请求体必须恰好等于该分块的期望大小；
    /// 输入没有完整结束时只丢弃当前临时缓冲，已经登记的其他分块不受影响。
    pub async fn start_resumable_part(
        &self,
        session_id: &str,
        idx: i64,
    ) -> AppResult<ResumablePartStart> {
        if self
            .active_resumable
            .lock()
            .unwrap()
            .cancelling_sessions
            .contains(session_id)
        {
            return Err(AppError::BadRequest("上传会话正在取消".into()));
        }
        let info = match self.db.claim_upload_part(session_id, idx).await? {
            UploadPartClaim::Complete => return Ok(ResumablePartStart::Complete),
            UploadPartClaim::Claimed(info) => *info,
        };
        let target = match self.tg.binding(info.session.storage_target_id) {
            Ok(target) => target,
            Err(error) => {
                let _ = self.db.release_upload_part(session_id, idx).await;
                return Err(error);
            }
        };
        let (tx, rx) = mpsc::channel::<UploadMsg>(4);
        let this = self.clone();
        let id = session_id.to_string();
        let key = (id.clone(), idx);
        let (cancel, abort_registration) = AbortHandle::new_pair();
        let cancel_reason = Arc::new(AtomicU8::new(PART_CANCEL_NONE));
        let stage = Arc::new(AtomicU8::new(PART_STAGE_RECEIVING));
        // 注册任务与 Bot API 重启共用同一把异步锁，避免任务在“递增代次”与
        // “中断旧任务”的间隙进入，从而逃过本轮重启协调。
        let restart_guard = self.bot_restart_lock.lock().await;
        let restart_generation = self.bot_restart_generation.load(Ordering::Acquire);
        let accepted = {
            let mut active = self.active_resumable.lock().unwrap();
            if active.cancelling_sessions.contains(session_id) {
                false
            } else {
                active.parts.insert(
                    key.clone(),
                    ActiveResumablePart {
                        cancel: cancel.clone(),
                        cancel_reason: cancel_reason.clone(),
                        stage: stage.clone(),
                    },
                );
                true
            }
        };
        drop(restart_guard);
        if !accepted {
            let _ = self.db.release_upload_part(session_id, idx).await;
            return Err(AppError::BadRequest("上传会话正在取消".into()));
        }
        let active_resumable = self.active_resumable.clone();
        let upstream_pending = Arc::new(AtomicU64::new(0));
        let pipeline_pending = upstream_pending.clone();
        let pipeline_stage = stage.clone();
        let task_cancel_reason = cancel_reason.clone();
        let join = tokio::spawn(async move {
            let pipeline_result = Abortable::new(
                this.resumable_part_pipeline(
                    &id,
                    idx,
                    info,
                    target.clone(),
                    pipeline_pending,
                    pipeline_stage,
                    rx,
                ),
                abort_registration,
            )
            .await;
            this.restart_detached_resumable_upload(
                &target.api,
                &upstream_pending,
                restart_generation,
                &key,
                &format!("会话 {id} 分块 {idx}"),
            )
            .await;
            // 其他任务必须等重启领导者释放锁后再读取原因；若监督器重启失败，
            // 绝不能把该分块标为可自动重试，否则可能制造重复 Telegram 消息。
            let result = match pipeline_result {
                Ok(result) => result,
                Err(_) => match task_cancel_reason.load(Ordering::Acquire) {
                    PART_CANCEL_BOT_RESTART => Err(AppError::UploadInterrupted),
                    PART_CANCEL_RESTART_FAILED => Err(AppError::TelegramUncertain(
                        "共享 Bot API 重启失败，其他并发分块已停止".into(),
                    )),
                    _ => Err(AppError::BadRequest("分块上传已取消".into())),
                },
            };
            if result.is_err()
                && let Err(release_error) = this.db.release_upload_part(&id, idx).await
            {
                tracing::warn!("释放上传会话 {id} 的分块 {idx} 失败: {release_error}");
            }
            active_resumable.lock().unwrap().parts.remove(&key);
            result
        });
        Ok(ResumablePartStart::Upload(ResumablePartHandle {
            tx,
            join,
            cancel,
            cancel_reason,
        }))
    }

    /// 停止一个可恢复会话的所有在途分块，并阻止取消期间启动新分块。
    pub async fn begin_cancel_resumable_session(&self, session_id: &str) {
        let tokens = {
            let mut active = self.active_resumable.lock().unwrap();
            active.cancelling_sessions.insert(session_id.to_string());
            active
                .parts
                .iter()
                .filter(|((id, _), _)| id == session_id)
                .map(|(_, part)| part.clone())
                .collect::<Vec<_>>()
        };
        for part in tokens {
            let _ = part.cancel_reason.compare_exchange(
                PART_CANCEL_NONE,
                PART_CANCEL_USER,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            part.cancel.abort();
        }

        // 包含 Bot API 监督器的 10 秒确认期限，再留出任务清理余量。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
        loop {
            let running = self
                .active_resumable
                .lock()
                .unwrap()
                .parts
                .keys()
                .any(|(id, _)| id == session_id);
            if !running || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub fn finish_cancel_resumable_session(&self, session_id: &str) {
        self.active_resumable
            .lock()
            .unwrap()
            .cancelling_sessions
            .remove(session_id);
    }

    /// 一个上传或分块被中止时，TDLib 可能仍在后台上传。共享 Bot API 必须只重启
    /// 一次；同一代的其他分块先被标记为可安全重试并中断，避免收到模糊的 502。
    async fn restart_detached_resumable_upload(
        &self,
        api: &Arc<dyn TgApi>,
        upstream_pending: &AtomicU64,
        started_generation: u64,
        own_key: &(String, i64),
        label: &str,
    ) {
        // 自建 Bot API 是所有 Bot 共用的单进程；多 Bot 池下重启它会同时打断
        // 其他 Bot 的在途请求，宁可让单个取消请求自然收尾，也不要制造全局 503。
        if self.tg.upload_bot_count() > 1 {
            return;
        }
        if upstream_pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let _restart_guard = self.bot_restart_lock.lock().await;
        if self.bot_restart_generation.load(Ordering::Acquire) != started_generation {
            return;
        }
        self.bot_restart_generation.fetch_add(1, Ordering::AcqRel);

        let interrupted = {
            let active = self.active_resumable.lock().unwrap();
            active
                .parts
                .iter()
                .filter(|(key, part)| {
                    *key != own_key && part.stage.load(Ordering::Acquire) < PART_STAGE_PERSISTING
                })
                .map(|(_, part)| part.clone())
                .collect::<Vec<_>>()
        };
        for part in &interrupted {
            let _ = part.cancel_reason.compare_exchange(
                PART_CANCEL_NONE,
                PART_CANCEL_BOT_RESTART,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            part.cancel.abort();
        }

        if let Err(error) = api.cancel_pending_uploads().await {
            for part in interrupted {
                let _ = part.cancel_reason.compare_exchange(
                    PART_CANCEL_BOT_RESTART,
                    PART_CANCEL_RESTART_FAILED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            tracing::error!("终止 Bot API 在途上传失败（{label}）: {error}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn resumable_part_pipeline(
        &self,
        session_id: &str,
        idx: i64,
        info: UploadSessionInfo,
        target: TgBinding,
        upstream_pending: Arc<AtomicU64>,
        stage: Arc<AtomicU8>,
        mut rx: mpsc::Receiver<UploadMsg>,
    ) -> AppResult<()> {
        let expected = info
            .expected_part_size(idx)
            .ok_or_else(|| AppError::BadRequest("分块下标超出范围".into()))?;
        let mut buffer = ChunkBuf::new(expected as u64, self.mem_limit, self.knobs.tmp_dir());
        let mut received = 0i64;
        let mut complete_body: Option<UploadBody> = None;
        let mut clean_eof = false;
        loop {
            match rx.recv().await {
                Some(UploadMsg::Data(mut data)) => {
                    received = received
                        .checked_add(data.len() as i64)
                        .ok_or_else(|| AppError::BadRequest("分块大小溢出".into()))?;
                    if received > expected {
                        return Err(AppError::BadRequest(format!(
                            "分块大小不匹配：期望 {expected} 字节"
                        )));
                    }
                    while !data.is_empty() {
                        if let Some(body) = buffer.push(&mut data).await?
                            && complete_body.replace(body).is_some()
                        {
                            return Err(AppError::BadRequest("分块包含多余数据".into()));
                        }
                    }
                }
                Some(UploadMsg::Eof) => {
                    clean_eof = true;
                    break;
                }
                None => break,
            }
        }
        if !clean_eof {
            return Err(AppError::BadRequest("分块上传中断".into()));
        }
        if received != expected {
            return Err(AppError::BadRequest(format!(
                "分块大小不匹配：期望 {expected} 字节，实际 {received} 字节"
            )));
        }
        let body = match complete_body {
            Some(body) => body,
            None => buffer
                .take_partial()
                .await?
                .ok_or_else(|| AppError::BadRequest("分块内容为空".into()))?,
        };

        let _permit = self
            .knobs
            .semaphore()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("信号量已关闭")))?;
        let filename = if idx == 0 {
            info.node.name.clone()
        } else {
            format!("{}.part{idx}", info.node.name)
        };
        let mime = (info.part_count() == 1 && self.knobs.send_video())
            .then(|| video_mime(&info.node.name))
            .flatten();
        let thumbnail = (idx == 0)
            .then(|| info.session.thumbnail.clone().map(Bytes::from))
            .flatten();
        stage.store(PART_STAGE_SENDING, Ordering::Release);
        let msg = send_body_to_telegram(
            target.api.clone(),
            &filename,
            mime,
            thumbnail.as_ref(),
            &body,
            &upstream_pending,
            Some(&stage),
        )
        .await?;
        let stored = StoredMessage {
            storage_target_id: target.storage_target_id,
            message_id: msg.message_id,
        };
        if let Err(error) = self
            .db
            .finish_upload_part(
                session_id,
                idx,
                body.len() as i64,
                &msg.file_id,
                msg.message_id,
                target.storage_target_id,
                msg.thumbnail_file_id.as_deref(),
            )
            .await
        {
            // 用户可能在 Telegram 请求进行期间取消了会话；消息必须进入可靠删除队列。
            if let Err(queue_error) = self.db.enqueue_delete_message(stored).await {
                tracing::error!("分块 {idx} 落库失败({error})，且登记补偿删除失败: {queue_error}");
            } else {
                self.process_pending_deletes();
            }
            return Err(error);
        }
        Ok(())
    }

    /// 删除任务已经和元数据变更在同一事务内持久化；这里负责立即唤醒处理器。
    pub fn delete_messages_best_effort(&self, messages: Vec<StoredMessage>) {
        if messages.is_empty() {
            return;
        }
        self.process_pending_deletes();
    }

    /// 处理到期的持久化删除任务。并发触发会合并成一个工作循环。
    /// 手动模式下循环只空转,不消费队列——任务照常积压,等后台点「立即执行」。
    pub fn process_pending_deletes(&self) {
        if self
            .delete_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut had_work = false;
            loop {
                if !this.knobs.auto_delete_message() {
                    had_work = false;
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }
                match this.run_delete_batch(100, QueueTrigger::Auto).await {
                    Ok(summary) if summary.attempted() == 0 => {
                        if had_work {
                            let (level, message) = if summary.pending == 0 {
                                ("info", "Telegram 消息清理队列已清空；剩余 0 条".to_string())
                            } else {
                                (
                                    "warn",
                                    format!(
                                        "Telegram 消息清理本轮完成；剩余 {} 条，等待退避重试",
                                        summary.pending
                                    ),
                                )
                            };
                            record_backend_log(&this.db, level, "消息清理", &message).await;
                            had_work = false;
                        }
                        // 常驻轻量 worker，确保指数退避到期后能按时重试，而不是最多
                        // 等待下一轮 30 分钟维护任务。
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                    Ok(summary) => {
                        had_work = true;
                        record_backend_log(
                            &this.db,
                            if summary.failed + summary.discarded > 0 {
                                "warn"
                            } else {
                                "info"
                            },
                            "消息清理",
                            &format!(
                                "Telegram 消息清理进度：完成 {} 条，永久跳过 {} 条，失败 {} 条，剩余 {} 条",
                                summary.completed,
                                summary.discarded,
                                summary.failed,
                                summary.pending
                            ),
                        )
                        .await;
                    }
                    // 落库确认失败等情况:退出循环,由下面的延时重启兜底,避免紧循环重复请求
                    Err(_) => break,
                }
            }
            this.delete_running.store(false, Ordering::Release);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            this.process_pending_deletes();
        });
    }

    /// 消费一批删除任务,每条最多尝试一次。自动 worker 与后台「立即执行」共用。
    pub async fn run_delete_batch(
        &self,
        limit: i64,
        trigger: QueueTrigger,
    ) -> AppResult<QueueRunSummary> {
        let jobs = match self
            .db
            .delete_jobs_due(limit, trigger.ignores_backoff())
            .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!("读取 Telegram 删除队列失败: {e}");
                record_backend_log(
                    &self.db,
                    "error",
                    "消息清理",
                    &format!("读取 Telegram 删除队列失败：{e}"),
                )
                .await;
                return Err(e);
            }
        };
        let mut summary = QueueRunSummary::default();
        let started = std::time::Instant::now();
        for job in jobs {
            if let Some(budget) = trigger.budget()
                && started.elapsed() >= budget
            {
                summary.stopped_early = true;
                break;
            }
            let attempt = async {
                match self.tg.binding(job.storage_target_id) {
                    Ok(binding) => binding.api.delete_message(job.tg_message_id).await,
                    Err(e) => Err(e),
                }
            };
            // 单条调用也要封顶，否则客户端内部的 5 次重试会拖垮一整轮。
            let result = if let Some(left) = trigger.remaining(started) {
                match tokio::time::timeout(left, attempt).await {
                    Ok(result) => result,
                    Err(_) => {
                        summary.stopped_early = true;
                        break;
                    }
                }
            } else {
                attempt.await
            };
            match result {
                Ok(()) => {
                    if let Err(e) = self.db.delete_job_done(job.id).await {
                        tracing::warn!("确认 Telegram 删除任务 {} 失败: {e}", job.id);
                        record_backend_log(
                            &self.db,
                            "error",
                            "消息清理",
                            &format!("确认 Telegram 删除任务 {} 失败：{e}", job.id),
                        )
                        .await;
                        // Telegram 已删但数据库未确认时不能继续,否则会反复请求同一条
                        return Err(e);
                    }
                    summary.completed += 1;
                }
                Err(e) => {
                    if is_expired_undeletable_message(&job, &e) {
                        self.db.delete_job_done(job.id).await?;
                        summary.discarded += 1;
                        record_backend_log(
                            &self.db,
                            "warn",
                            "消息清理",
                            &format!(
                                "消息 {} 已超过 Telegram Bot API 的 48 小时删除窗口，停止重试；如频道中仍存在，请在 Telegram 客户端手动删除",
                                job.tg_message_id
                            ),
                        )
                        .await;
                        continue;
                    }
                    summary.failed += 1;
                    summary.note_error(&e.to_string());
                    tracing::warn!(
                        "删除存储目标 {} 的消息 {} 失败: {e}",
                        job.storage_target_id,
                        job.tg_message_id
                    );
                    record_backend_log(
                        &self.db,
                        "warn",
                        "消息清理",
                        &format!(
                            "消息 {} 删除失败（第 {} 次尝试）{}：{}",
                            job.tg_message_id,
                            job.attempts + 1,
                            trigger.retry_hint(),
                            e
                        ),
                    )
                    .await;
                    if let Err(db_err) = self
                        .db
                        .delete_job_failed(job.id, job.attempts, &e.to_string())
                        .await
                    {
                        tracing::error!("更新 Telegram 删除任务 {} 失败: {db_err}", job.id);
                        record_backend_log(
                            &self.db,
                            "error",
                            "消息清理",
                            &format!("更新消息清理任务 {} 的重试状态失败：{db_err}", job.id),
                        )
                        .await;
                        return Err(db_err);
                    }
                }
            }
        }
        summary.pending = self.db.pending_delete_count().await.unwrap_or(0);
        Ok(summary)
    }

    async fn pipeline(
        &self,
        node_id: i64,
        name: &str,
        thumbnail: Option<Bytes>,
        target: TgBinding,
        upstream_pending: Arc<AtomicU64>,
        mut rx: mpsc::Receiver<UploadMsg>,
    ) -> AppResult<Node> {
        let mut uploads: JoinSet<AppResult<()>> = JoinSet::new();
        let result = self
            .pipeline_inner(
                &mut uploads,
                node_id,
                name,
                thumbnail,
                target,
                upstream_pending,
                &mut rx,
            )
            .await;
        if result.is_err() {
            // 出错时不 abort 在途任务:消息可能已发到 TG,必须等它落定并记入
            // chunks 表,随后的 abort_upload 才能把它们一并清理,避免孤儿消息
            while let Some(res) = uploads.join_next().await {
                if let Ok(Err(e)) = res {
                    tracing::debug!("在途分块在回滚前失败: {e}");
                }
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn pipeline_inner(
        &self,
        uploads: &mut JoinSet<AppResult<()>>,
        node_id: i64,
        name: &str,
        thumbnail: Option<Bytes>,
        target: TgBinding,
        upstream_pending: Arc<AtomicU64>,
        rx: &mut mpsc::Receiver<UploadMsg>,
    ) -> AppResult<Node> {
        // 整次上传用同一份分块参数快照,中途改配置不会切出大小不一的块
        let mut buf = ChunkBuf::new(
            self.knobs.chunk_size(),
            self.mem_limit,
            self.knobs.tmp_dir(),
        );
        let mut idx: i64 = 0;
        let mut total: u64 = 0;
        let mut clean_eof = false;
        // 顺路算内容哈希,供秒传去重索引
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();

        'outer: loop {
            match rx.recv().await {
                Some(UploadMsg::Data(mut data)) => {
                    total += data.len() as u64;
                    sha2::Digest::update(&mut hasher, &data);
                    while !data.is_empty() {
                        if let Some(full) = buf.push(&mut data).await? {
                            // 攒满整块即被切分,该文件必然多于一块,不是视频消息候选
                            self.spawn_chunk(
                                uploads,
                                node_id,
                                name,
                                idx,
                                full,
                                false,
                                if idx == 0 { thumbnail.clone() } else { None },
                                target.clone(),
                                upstream_pending.clone(),
                            )
                            .await?;
                            idx += 1;
                        }
                        // 上传任务出错时尽早终止,不再收数据
                        while let Some(res) = uploads.try_join_next() {
                            res.map_err(|e| anyhow::anyhow!("上传任务崩溃: {e}"))??;
                        }
                    }
                }
                Some(UploadMsg::Eof) => {
                    clean_eof = true;
                    break 'outer;
                }
                None => break 'outer, // 生产者异常消失
            }
        }
        if !clean_eof {
            return Err(AppError::BadRequest("上传中断:输入流提前关闭".into()));
        }
        // 机会性提前发现已失败的分块,避免再白传最后一块
        while let Some(res) = uploads.try_join_next() {
            res.map_err(|e| anyhow::anyhow!("上传任务崩溃: {e}"))??;
        }
        if let Some(last) = buf.take_partial().await? {
            // EOF 时的尾块若是第 0 块,说明整个文件只有这一块 → 视频消息候选
            self.spawn_chunk(
                uploads,
                node_id,
                name,
                idx,
                last,
                idx == 0,
                if idx == 0 { thumbnail } else { None },
                target,
                upstream_pending,
            )
            .await?;
        }
        while let Some(res) = uploads.join_next().await {
            res.map_err(|e| anyhow::anyhow!("上传任务崩溃: {e}"))??;
        }

        let sha = to_hex(&sha2::Digest::finalize(hasher));
        // 节点随 finalize 事务一并读出，提交成功后不再做可能失败的数据库操作。
        // 否则可能出现“已经提交成功，却因随后一次查询失败而被外层回滚”的假失败。
        let (node, freed) = self
            .db
            .finalize_upload_with_node(node_id, total as i64, Some(&sha))
            .await?;
        self.delete_messages_best_effort(freed); // 覆盖写释放的旧版本
        Ok(node)
    }

    /// 信号量内并发上传一个分块;插 chunks 行在任务内完成。
    /// `single_chunk`:该块就是整个文件(仅此时才可能改用 sendVideo)
    #[allow(clippy::too_many_arguments)]
    async fn spawn_chunk(
        &self,
        uploads: &mut JoinSet<AppResult<()>>,
        node_id: i64,
        name: &str,
        idx: i64,
        body: UploadBody,
        single_chunk: bool,
        thumbnail: Option<Bytes>,
        target: TgBinding,
        upstream_pending: Arc<AtomicU64>,
    ) -> AppResult<()> {
        let permit = self
            .knobs
            .semaphore()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("信号量已关闭")))?;
        let tg = target.api;
        let storage_target_id = target.storage_target_id;
        let db = self.db.clone();
        // 分块 0 用原始文件名(单块文件在 TG 里保持原名),后续块加 .partN 后缀
        let filename = if idx == 0 {
            name.to_string()
        } else {
            format!("{name}.part{idx}")
        };
        // 完整的视频文件按「视频消息」发,Telegram 里可直接播放;开关关闭或非视频照旧走文档
        let mime = (single_chunk && self.knobs.send_video())
            .then(|| video_mime(name))
            .flatten();
        uploads.spawn(async move {
            let _permit = permit;
            let size = body.len();
            let msg = send_body_to_telegram(
                tg.clone(),
                &filename,
                mime,
                thumbnail.as_ref(),
                &body,
                &upstream_pending,
                None,
            )
            .await?;
            if let Err(e) = db
                .add_chunk(
                    node_id,
                    idx,
                    size as i64,
                    &msg.file_id,
                    msg.message_id,
                    storage_target_id,
                    msg.thumbnail_file_id.as_deref(),
                )
                .await
            {
                // Telegram 已成功但元数据登记失败时立即补偿删除，避免频道残留孤儿分块。
                if let Err(cleanup) = tg.delete_message(msg.message_id).await {
                    tracing::error!(
                        "分块 {idx} 落库失败({e})，且清理 TG 消息 {} 失败: {cleanup}",
                        msg.message_id
                    );
                }
                return Err(e);
            }
            Ok(())
        });
        Ok(())
    }
}

/// 攒块缓冲:内存起步,超过 mem_limit 落临时文件
struct ChunkBuf {
    chunk_size: u64,
    mem_limit: u64,
    tmp_dir: PathBuf,
    mem: BytesMut,
    spill: Option<SpillFile>,
}

struct SpillFile {
    owner: tempfile::NamedTempFile,
    writer: tokio::fs::File,
    len: u64,
}

impl ChunkBuf {
    fn new(chunk_size: u64, mem_limit: u64, tmp_dir: PathBuf) -> Self {
        ChunkBuf {
            chunk_size,
            mem_limit,
            tmp_dir,
            mem: BytesMut::new(),
            spill: None,
        }
    }

    fn filled(&self) -> u64 {
        self.spill.as_ref().map(|spill| spill.len).unwrap_or(0) + self.mem.len() as u64
    }

    /// 从 data 消费字节;攒满一个分块时返回之(data 剩余部分留给下一块)
    async fn push(&mut self, data: &mut Bytes) -> AppResult<Option<UploadBody>> {
        let room = (self.chunk_size - self.filled()) as usize;
        let take = room.min(data.len());
        let piece = data.split_to(take);

        if let Some(spill) = &mut self.spill {
            spill.writer.write_all(&piece).await?;
            spill.len += piece.len() as u64;
        } else {
            self.mem.extend_from_slice(&piece);
            // 内存超限且分块还没攒满 → 转入临时文件
            if self.mem.len() as u64 > self.mem_limit && (self.mem.len() as u64) < self.chunk_size {
                let owner = tempfile::NamedTempFile::new_in(&self.tmp_dir)?;
                let writer = tokio::fs::File::from_std(owner.reopen()?);
                let n = self.mem.len() as u64;
                let mut spill = SpillFile {
                    owner,
                    writer,
                    len: n,
                };
                spill.writer.write_all(&self.mem).await?;
                self.mem = BytesMut::new();
                self.spill = Some(spill);
            }
        }
        if self.filled() >= self.chunk_size {
            return self.take_partial().await;
        }
        Ok(None)
    }

    /// 取出当前已攒的(可能不满的)分块;空则返回 None
    async fn take_partial(&mut self) -> AppResult<Option<UploadBody>> {
        if let Some(mut spill) = self.spill.take() {
            spill.writer.flush().await?;
            drop(spill.writer);
            return Ok(Some(UploadBody::TempFile {
                file: spill.owner,
                len: spill.len,
            }));
        }
        if self.mem.is_empty() {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.mem).freeze();
        Ok(Some(UploadBody::Memory(bytes)))
    }
}
