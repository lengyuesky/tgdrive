//! 流式分块上传管线
//!
//! 生产者(HTTP 上传 / WebDAV PUT)通过 mpsc 通道喂入字节,管线在独立任务中
//! 攒块并发上传;生产者中途消失(未发 Eof 就断开)视为失败,自动回滚已传分块。

use crate::db::{Db, Node, StoredMessage};
use crate::error::{AppError, AppResult};
use crate::telegram::{TgBinding, TgRegistry, UploadBody};
use bytes::{Bytes, BytesMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Semaphore, mpsc};
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

pub struct UploadHandle {
    pub tx: mpsc::Sender<UploadMsg>,
    pub join: tokio::task::JoinHandle<AppResult<Node>>,
}

impl UploadHandle {
    pub async fn feed(&self, data: Bytes) -> AppResult<()> {
        self.tx
            .send(UploadMsg::Data(data))
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("上传管线已终止")))
    }

    /// 宣告输入结束并等待管线收尾,返回最终节点
    pub async fn finish(self) -> AppResult<Node> {
        let _ = self.tx.send(UploadMsg::Eof).await;
        self.join
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("上传任务崩溃: {e}")))?
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
}

impl UploadKnobs {
    pub fn new(chunk_size: u64, tmp_dir: PathBuf, concurrency: usize, send_video: bool) -> Self {
        UploadKnobs {
            chunk_size: AtomicU64::new(chunk_size),
            tmp_dir: RwLock::new(tmp_dir),
            semaphore: RwLock::new(Arc::new(Semaphore::new(concurrency))),
            send_video: AtomicBool::new(send_video),
        }
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

#[derive(Clone)]
pub struct Uploader {
    pub tg: Arc<TgRegistry>,
    pub db: Db,
    pub knobs: Arc<UploadKnobs>,
    /// 分块超过该值转入临时文件(生产用 MEMORY_CHUNK_LIMIT,测试可调小)
    pub mem_limit: u64,
    delete_running: Arc<AtomicBool>,
}

impl Uploader {
    pub fn new(tg: Arc<TgRegistry>, db: Db, knobs: Arc<UploadKnobs>, mem_limit: u64) -> Self {
        Uploader {
            tg,
            db,
            knobs,
            mem_limit,
            delete_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 启动一次上传:先建 uploading 占位节点,返回喂数据的句柄
    pub async fn start(
        &self,
        parent_id: i64,
        name: &str,
        display_path: &str,
    ) -> AppResult<UploadHandle> {
        // 一次上传固定使用同一个存储目标；后台改配置只影响之后开始的上传。
        let target = self.tg.current()?;
        let node_id = self.db.create_uploading(parent_id, name).await?;
        let (tx, rx) = mpsc::channel::<UploadMsg>(4);
        let this = self.clone();
        let name = name.to_string();
        let display_path = display_path.to_string();
        let join = tokio::spawn(async move {
            match this
                .pipeline(node_id, &name, &display_path, target, rx)
                .await
            {
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
        Ok(UploadHandle { tx, join })
    }

    /// 删除任务已经和元数据变更在同一事务内持久化；这里负责立即唤醒处理器。
    pub fn delete_messages_best_effort(&self, messages: Vec<StoredMessage>) {
        if messages.is_empty() {
            return;
        }
        self.process_pending_deletes();
    }

    /// 处理到期的持久化删除任务。并发触发会合并成一个工作循环。
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
            'worker: loop {
                let jobs = match this.db.delete_jobs_due(100).await {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        tracing::warn!("读取 Telegram 删除队列失败: {e}");
                        break;
                    }
                };
                if jobs.is_empty() {
                    // 常驻轻量 worker，确保指数退避到期后能按时重试，而不是最多
                    // 等待下一轮 30 分钟维护任务。
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }
                for job in jobs {
                    let result = match this.tg.binding(job.storage_target_id) {
                        Ok(binding) => binding.api.delete_message(job.tg_message_id).await,
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(()) => {
                            if let Err(e) = this.db.delete_job_done(job.id).await {
                                tracing::warn!("确认 Telegram 删除任务 {} 失败: {e}", job.id);
                                // Telegram 已删但数据库未确认时不能在紧循环里反复请求；
                                // 退出并延时重启，避免紧循环重复请求。
                                break 'worker;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "删除存储目标 {} 的消息 {} 失败，将自动重试: {e}",
                                job.storage_target_id,
                                job.tg_message_id
                            );
                            if let Err(db_err) = this
                                .db
                                .delete_job_failed(job.id, job.attempts, &e.to_string())
                                .await
                            {
                                tracing::error!("更新 Telegram 删除任务 {} 失败: {db_err}", job.id);
                                break 'worker;
                            }
                        }
                    }
                }
            }
            this.delete_running.store(false, Ordering::Release);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            this.process_pending_deletes();
        });
    }

    async fn pipeline(
        &self,
        node_id: i64,
        name: &str,
        display_path: &str,
        target: TgBinding,
        mut rx: mpsc::Receiver<UploadMsg>,
    ) -> AppResult<Node> {
        let mut uploads: JoinSet<AppResult<()>> = JoinSet::new();
        let result = self
            .pipeline_inner(&mut uploads, node_id, name, display_path, target, &mut rx)
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

    async fn pipeline_inner(
        &self,
        uploads: &mut JoinSet<AppResult<()>>,
        node_id: i64,
        name: &str,
        display_path: &str,
        target: TgBinding,
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
                                display_path,
                                idx,
                                full,
                                false,
                                target.clone(),
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
                display_path,
                idx,
                last,
                idx == 0,
                target,
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
        display_path: &str,
        idx: i64,
        body: UploadBody,
        single_chunk: bool,
        target: TgBinding,
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
        // caption 记录路径、分块序号与上传节点号，供异常核对或元数据库丢失时人工恢复。
        let cap_path: String = display_path.chars().take(700).collect();
        let caption = format!(
            "tgdrive {{\"p\":\"{}\",\"i\":{},\"u\":{}}}",
            cap_path.replace('"', "'"),
            idx,
            node_id
        );
        uploads.spawn(async move {
            let _permit = permit;
            let size = body.len();
            let msg = match mime {
                Some(m) => {
                    // 从文件头解析宽高/时长;解析不了就不带(Telegram 会按默认比例显示)
                    let meta = crate::engine::probe::probe_body(m, &body);
                    if meta.is_none() {
                        tracing::debug!("{filename} 未解析出视频宽高,按无元数据发送");
                    }
                    match tg
                        .send_video(&filename, m, meta, Some(caption.clone()), &body)
                        .await
                    {
                        Ok(msg) => msg,
                        // 只有 Telegram 明确拒绝视频格式时才能降级；结果不确定时再次发送会重复。
                        Err(AppError::TelegramRejected(detail)) => {
                            tracing::warn!("{filename} 以视频消息发送失败({detail}),回退为文档");
                            tg.send_document(&filename, Some(caption), &body).await?
                        }
                        Err(e) => return Err(e),
                    }
                }
                None => tg.send_document(&filename, Some(caption), &body).await?,
            };
            if let Err(e) = db
                .add_chunk(
                    node_id,
                    idx,
                    size as i64,
                    &msg.file_id,
                    msg.message_id,
                    storage_target_id,
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
