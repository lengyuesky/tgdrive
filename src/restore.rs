//! Telegram 数据库恢复：候选离线校验，确认只写标记，下次进程才切库。
mod discovery;
mod storage;
#[cfg(test)]
mod tests;

use crate::api::AppState;
use crate::backup::{BACKUP_LOCK, TelegramBackupRecord};
use crate::db::{Db, now_ts};
use crate::error::{AppError, AppResult};
use crate::telegram::{TgApi, TgRegistry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};

pub use discovery::DiscoveryResult;
pub use storage::{InstanceLock, require_cli_idle, startup};
const MAX_PARTS: usize = 128;
const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TASK_TIME: Duration = Duration::from_secs(30 * 60);
const READY_TTL: i64 = 30 * 60;

#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct RestoreStatus {
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    pub warnings: Vec<String>,
    pub safe_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_path: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Summary {
    pub name: String,
    pub bytes: u64,
    pub files: i64,
    pub directories: i64,
    pub encrypted: bool,
}

/// 不保存密码、Bot Token 或来源链接。路径全部由固定目录和服务端 ID 推导。
#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    status: RestoreStatus,
    stage: String,
    digest: Option<String>,
    updated: i64,
    previous_safe_mode: bool,
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            status: RestoreStatus {
                phase: "idle".into(),
                ..Default::default()
            },
            stage: "done".into(),
            digest: None,
            updated: now_ts(),
            previous_safe_mode: false,
        }
    }
}

struct Inner {
    journal: Journal,
    backup_guard: Option<tokio::sync::MutexGuard<'static, ()>>,
}

#[derive(Clone)]
pub struct RestoreManager {
    discovery: Arc<Mutex<discovery::Discovery>>,
    root: Arc<PathBuf>,
    inner: Arc<Mutex<Inner>>,
    cancelled: Arc<AtomicBool>,
    maintenance: Arc<AtomicBool>,
    /// 普通写请求持读锁；确认持写锁，等待已有配置修改结束再捕获安全设置。
    pub write_gate: Arc<RwLock<()>>,
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl RestoreManager {
    pub fn new(db_path: &str) -> anyhow::Result<Self> {
        let root = storage::root(db_path)?;
        let journal = storage::load(&root)?;
        Ok(Self {
            discovery: Arc::new(Mutex::new(discovery::Discovery::default())),
            root: Arc::new(root),
            inner: Arc::new(Mutex::new(Inner {
                journal,
                backup_guard: None,
            })),
            cancelled: Arc::new(AtomicBool::new(false)),
            maintenance: Arc::new(AtomicBool::new(false)),
            write_gate: Arc::new(RwLock::new(())),
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub fn maintenance(&self) -> bool {
        self.maintenance.load(Ordering::Acquire)
    }

    pub async fn status(&self) -> RestoreStatus {
        self.inner.lock().await.journal.status.clone()
    }

    pub async fn prepare(&self, state: AppState, req: PrepareRequest) -> AppResult<RestoreStatus> {
        if !req.acknowledge_forward {
            return Err(bad("请先确认允许在来源频道临时转发并清理本次消息"));
        }
        let mut inner = self.inner.lock().await;
        if matches!(
            inner.journal.status.phase.as_str(),
            "preparing" | "ready" | "restarting"
        ) {
            return Err(AppError::Conflict("已有恢复任务，请取消或等待完成".into()));
        }
        let guard = BACKUP_LOCK
            .try_lock()
            .map_err(|_| AppError::Conflict("备份或恢复任务正在运行".into()))?;
        // 来源检查不产生 Telegram 写入。
        let source = resolve_source(&state, &req).await?;
        storage::remove_candidate(&self.root)?;
        let id = crate::auth::random_secret();
        let safe_mode = inner.journal.status.safe_mode;
        inner.journal = Journal {
            status: RestoreStatus {
                phase: "preparing".into(),
                id: Some(id),
                message: Some("正在下载和校验；当前数据库不会被替换".into()),
                safe_mode,
                warnings: vec![
                    "旧备份不含可信签名，完整性检查不能证明备份来源真实。".into(),
                    "只恢复索引和配置，不重新下载所有文件；需保留关联 Bot 的访问权限。".into(),
                ],
                ..Default::default()
            },
            stage: "preparing".into(),
            digest: None,
            updated: now_ts(),
            previous_safe_mode: safe_mode,
        };
        storage::save(&self.root, &inner.journal)?;
        inner.backup_guard = Some(guard);
        self.cancelled.store(false, Ordering::Release);
        let initial = inner.journal.status.clone();
        drop(inner);
        let manager = self.clone();
        tokio::spawn(async move {
            let result = manager.download_candidate(source, req.password).await;
            let mut inner = manager.inner.lock().await;
            match result {
                Ok((summary, digest, warnings)) if !manager.cancelled.load(Ordering::Acquire) => {
                    // ID 含候选内容摘要，确认时还会重新计算摘要，不能换文件重用令牌。
                    let nonce = inner.journal.status.id.as_deref().unwrap_or("");
                    inner.journal.status.id = Some(format!("{nonce}-{digest}"));
                    inner.journal.status.phase = "ready".into();
                    inner.journal.status.summary = Some(summary);
                    inner.journal.status.warnings.extend(warnings);
                    inner.journal.status.message = Some(
                        "校验通过，请在 30 分钟内确认。确认后应用退出，需进程管理器或手动重启。"
                            .into(),
                    );
                    inner.journal.digest = Some(digest);
                    inner.journal.stage = "ready".into();
                }
                outcome => {
                    let cancelled = manager.cancelled.load(Ordering::Acquire);
                    inner.journal.status.phase =
                        if cancelled { "cancelled" } else { "failed" }.into();
                    inner.journal.status.message = Some(match outcome {
                        Err(e) => e.to_string(),
                        _ => "恢复已取消".into(),
                    });
                    inner.journal.stage = "done".into();
                    inner.backup_guard = None;
                    let _ = storage::remove_candidate(&manager.root);
                }
            }
            inner.journal.updated = now_ts();
            if let Err(e) = storage::save(&manager.root, &inner.journal) {
                inner.journal.status.phase = "failed".into();
                inner.journal.status.message = Some(format!("保存恢复状态失败：{e}"));
                inner.backup_guard = None;
                let _ = storage::remove_candidate(&manager.root);
            }
            let ready_id = inner.journal.status.id.clone();
            drop(inner);
            tokio::time::sleep(Duration::from_secs(READY_TTL as u64)).await;
            let mut inner = manager.inner.lock().await;
            if inner.journal.status.phase == "ready" && inner.journal.status.id == ready_id {
                inner.journal.status.phase = "cancelled".into();
                inner.journal.status.message = Some("候选已过期，请重新准备".into());
                inner.journal.stage = "done".into();
                inner.backup_guard = None;
                let _ = storage::remove_candidate(&manager.root);
                let _ = storage::save(&manager.root, &inner.journal);
            }
        });
        Ok(initial)
    }

    pub async fn cancel(&self, id: &str) -> AppResult<RestoreStatus> {
        let mut inner = self.inner.lock().await;
        require_id(&inner.journal, id)?;
        match inner.journal.status.phase.as_str() {
            "preparing" => {
                self.cancelled.store(true, Ordering::Release);
                inner.journal.status.message = Some("正在取消并清理临时消息，请稍候".into());
            }
            "ready" => {
                storage::remove_candidate(&self.root)?;
                inner.journal.status.phase = "cancelled".into();
                inner.journal.status.message = Some("候选已取消，当前数据库未改变".into());
                inner.journal.stage = "done".into();
                inner.backup_guard = None;
            }
            _ => return Err(AppError::Conflict("此阶段不能取消恢复".into())),
        }
        storage::save(&self.root, &inner.journal)?;
        Ok(inner.journal.status.clone())
    }

    /// 调用者已持 write_gate 写锁并完成二次认证。此处绝不替换运行中的数据库。
    pub async fn confirm(&self, state: &AppState, id: &str) -> AppResult<RestoreStatus> {
        let mut inner = self.inner.lock().await;
        require_id(&inner.journal, id)?;
        if inner.journal.status.phase != "ready" || now_ts() - inner.journal.updated > READY_TTL {
            return Err(AppError::Conflict("候选不再可确认，请重新准备".into()));
        }
        let candidate = self.root.join("candidate.db");
        let digest = storage::digest(&candidate).await?;
        if inner.journal.digest.as_deref() != Some(&digest) {
            return Err(bad("候选摘要不一致，请重新准备"));
        }
        storage::preserve_settings(&candidate, state).await?;
        let digest = storage::digest(&candidate).await?;
        let mut journal = inner.journal.clone();
        journal.digest = Some(digest);
        journal.stage = "pending".into();
        journal.status.phase = "restarting".into();
        journal.status.message =
            Some("已确认。应用正在退出；下一进程将创建含 WAL 的回滚快照并恢复。".into());
        journal.updated = now_ts();
        // 提前测试状态写入；成功后只能重启/离线处理，不能重新开放写请求。
        storage::save(&self.root, &journal)?;
        inner.journal = journal;
        self.maintenance.store(true, Ordering::Release);
        state.vfs.db.set_background_blocked(true);
        self.shutdown.cancel();
        Ok(inner.journal.status.clone())
    }

    pub async fn resume(&self, state: &AppState) -> AppResult<RestoreStatus> {
        let mut inner = self.inner.lock().await;
        if !inner.journal.status.safe_mode
            || matches!(
                inner.journal.status.phase.as_str(),
                "preparing" | "ready" | "restarting"
            )
        {
            return Err(AppError::Conflict("当前不能解除安全模式".into()));
        }
        let mut journal = inner.journal.clone();
        journal.stage = "resume".into();
        journal.status.phase = "restarting".into();
        journal.status.message =
            Some("已确认恢复后台任务，正在重启；历史清理任务将重新执行。".into());
        storage::save(&self.root, &journal)?;
        inner.journal = journal;
        self.maintenance.store(true, Ordering::Release);
        state.vfs.db.set_background_blocked(true);
        self.shutdown.cancel();
        Ok(inner.journal.status.clone())
    }

    async fn download_candidate(
        &self,
        source: Source,
        password: Option<String>,
    ) -> anyhow::Result<(Summary, String, Vec<String>)> {
        let deadline = tokio::time::Instant::now() + TASK_TIME;
        let temp = tempfile::Builder::new()
            .prefix("download-")
            .tempdir_in(&*self.root)?;
        let mut docs = Vec::new();
        let mut warnings = Vec::new();
        let source_ids: HashSet<i64> = source.ids.iter().copied().collect();
        let mut temporary = HashSet::new();
        // forward 的响应不能被 cancel 丢弃，否则已发送临时消息 ID 不可知。
        let download_result = async {
            for id in &source.ids {
                self.check_cancel(deadline)?;
                let doc = source.api.forward_backup_document(&source.channel, *id).await?;
                anyhow::ensure!(doc.message_id > 0 && !source_ids.contains(&doc.message_id), "Telegram 返回的临时消息标识不安全；未删除任何源消息");
                anyhow::ensure!(temporary.insert(doc.message_id), "Telegram 返回重复临时消息 ID");
                self.check_cancel(deadline)?;
                let name = doc.file_name.as_deref().ok_or_else(|| anyhow::anyhow!("备份消息缺少 document 文件名"))?;
                let part = parse_part(name)?;
                let size = doc.file_size.filter(|size| *size > 0 && *size <= MAX_BYTES).ok_or_else(|| anyhow::anyhow!("备份消息缺少有效文件长度或超过 8 GiB 上限"))?;
                let total: u64 = docs.iter().map(|(_, _, size): &(Part, PathBuf, u64)| *size).sum();
                anyhow::ensure!(total + size <= MAX_BYTES, "备份总大小超过 8 GiB 上限");
                let path = temp.path().join(format!("part-{}", docs.len()));
                let copy = async {
                    let mut reader = source.api.open_chunk_stream(&doc.file_id, doc.message_id, size, 0).await?;
                    let mut output = tokio::fs::File::create(&path).await?;
                    let mut copied = 0u64;
                    let mut buf = vec![0u8; 128 * 1024];
                    loop {
                        self.check_cancel(deadline)?;
                        let n = reader.read(&mut buf).await?;
                        if n == 0 { break; }
                        copied += n as u64;
                        anyhow::ensure!(copied <= size, "备份分块实际长度大于声明长度");
                        output.write_all(&buf[..n]).await?;
                    }
                    anyhow::ensure!(copied == size, "备份分块下载不完整：应为 {size} 字节，实为 {copied} 字节");
                    output.sync_all().await?;
                    Ok::<(), anyhow::Error>(())
                };
                tokio::select! {
                    result = tokio::time::timeout_at(deadline, copy) => result.map_err(|_| anyhow::anyhow!("备份下载超过 30 分钟；检查 Bot API 大文件限制、MTProto 和代理"))??,
                    _ = async { while !self.cancelled.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(100)).await; } } => anyhow::bail!("恢复已取消"),
                }
                docs.push((part, path, size));
            }
            validate_parts(&docs.iter().map(|(part, _, _)| part.clone()).collect::<Vec<_>>())?;
            Ok::<(), anyhow::Error>(())
        }.await;
        // 不把临时消息放进旧删除队列；只有本次返回且不在任何源 ID 中的消息可删。
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        for id in temporary {
            let remaining = cleanup_deadline.saturating_duration_since(tokio::time::Instant::now());
            let cleanup = tokio::time::timeout(
                remaining.min(Duration::from_secs(10)),
                source.api.delete_message(id),
            )
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                warnings.push(format!(
                    "临时转发消息 {id} 清理失败，可在来源频道手动删除该临时消息（不是源备份）。"
                ));
            }
        }
        if !warnings.is_empty() {
            let mut inner = self.inner.lock().await;
            inner.journal.status.warnings.extend(warnings.clone());
        }
        download_result?;
        self.check_cancel(deadline)?;
        docs.sort_by_key(|(part, _, _)| part.index);
        let name = docs[0].0.base.clone();
        let encrypted = name.ends_with(".db.age");
        let merged = temp.path().join("merged");
        let mut output = tokio::fs::File::create(&merged).await?;
        for (_, path, _) in &docs {
            self.check_cancel(deadline)?;
            let mut input = tokio::fs::File::open(path).await?;
            tokio::io::copy(&mut input, &mut output).await?;
        }
        output.sync_all().await?;
        drop(output);
        let plain = temp.path().join("plain.db");
        if encrypted {
            let password = password
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("此备份已加密，请输入备份密码"))?;
            let src = merged.clone();
            let dst = plain.clone();
            // 阻塞解密不允许被 timeout 丢弃；等待它退出才能清理明文和释放互斥。
            tokio::task::spawn_blocking(move || {
                crate::backup::decrypt_file_limited(&src, &dst, password, MAX_BYTES)
            })
            .await??;
        } else {
            tokio::fs::rename(&merged, &plain).await?;
        }
        self.check_cancel(deadline)?;
        let (files, directories) = storage::validate_candidate(&plain).await?;
        self.check_cancel(deadline)?;
        let bytes = tokio::fs::metadata(&plain).await?.len();
        let candidate = self.root.join("candidate.db");
        storage::install_copy(&plain, &candidate)?;
        let digest = storage::digest(&candidate).await?;
        // 清理告警已加入任务状态，不在成功路径重复加入。
        Ok((
            Summary {
                name,
                bytes,
                files,
                directories,
                encrypted,
            },
            digest,
            Vec::new(),
        ))
    }

    fn check_cancel(&self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancelled.load(Ordering::Acquire), "恢复已取消");
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "恢复准备超过 30 分钟，请检查网络和备份大小"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct PrepareRequest {
    pub backup_id: Option<String>,
    pub messages: Option<String>,
    pub channel: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub acknowledge_forward: bool,
}
#[derive(Clone, Serialize)]
pub struct HistoryItem {
    id: String,
    name: String,
    parts: usize,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
fn history_id(record: &TelegramBackupRecord) -> String {
    hex_digest(&serde_json::to_vec(record).expect("备份记录可序列化"))
}
pub async fn history(state: &AppState) -> AppResult<Vec<HistoryItem>> {
    let records = match crate::backup::load_telegram_history(&state.vfs.db).await {
        Ok(records) => records,
        Err(AppError::BadRequest(message)) => {
            return Ok(vec![HistoryItem {
                id: "invalid-history".into(),
                name: "本机备份记录损坏".into(),
                parts: 0,
                available: false,
                error: Some(message),
            }]);
        }
        Err(error) => return Err(error),
    };
    let targets = state.vfs.db.storage_targets().await?;
    Ok(records
        .iter()
        .rev()
        .map(|record| {
            let error = validate_record(record, &targets)
                .and_then(|_| {
                    state
                        .vfs
                        .tg
                        .binding(record.messages[0].storage_target_id)
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                })
                .err()
                .map(|error| error.to_string());
            HistoryItem {
                id: history_id(record),
                name: record.name.clone(),
                parts: record.messages.len(),
                available: error.is_none(),
                error,
            }
        })
        .collect())
}
fn validate_record(
    record: &TelegramBackupRecord,
    targets: &[crate::db::StorageTarget],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !record.messages.is_empty() && record.messages.len() <= MAX_PARTS,
        "本机备份消息列表为空或超限"
    );
    let target_id = record.messages[0].storage_target_id;
    anyhow::ensure!(
        targets.iter().any(|target| target.id == target_id),
        "备份关联存储目标不存在"
    );
    let mut ids = HashSet::new();
    anyhow::ensure!(
        record
            .messages
            .iter()
            .all(|message| message.storage_target_id == target_id
                && message.message_id > 0
                && ids.insert(message.message_id)),
        "备份消息混用存储目标或有重复/无效 ID"
    );
    Ok(())
}
struct Source {
    api: Arc<dyn TgApi>,
    channel: String,
    ids: Vec<i64>,
}
async fn resolve_source(state: &AppState, request: &PrepareRequest) -> AppResult<Source> {
    if let Some(id) = &request.backup_id {
        if request
            .messages
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
            || request
                .channel
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        {
            return Err(bad("不能同时选择本机历史和手动来源"));
        }
        let records = if id.starts_with("channel:") {
            state.restore.discovered_records(state).await?
        } else {
            crate::backup::load_telegram_history(&state.vfs.db).await?
        };
        let record = records
            .iter()
            .find(|record| discovery::record_id(record) == *id || history_id(record) == *id)
            .ok_or_else(|| bad("备份记录已变化或探测结果已过期，请重新探测或刷新列表"))?;
        let targets = state.vfs.db.storage_targets().await?;
        validate_record(record, &targets).map_err(|e| bad(&e.to_string()))?;
        let target = targets
            .iter()
            .find(|target| target.id == record.messages[0].storage_target_id)
            .unwrap();
        return Ok(Source {
            api: state.vfs.tg.binding(target.id)?.api,
            channel: target.chat_id.clone(),
            ids: record.messages.iter().map(|m| m.message_id).collect(),
        });
    }
    let (channel, ids) = parse_messages(
        request.messages.as_deref().unwrap_or(""),
        request.channel.as_deref(),
    )
    .map_err(|e| bad(&e.to_string()))?;
    let cfg = state.rt.current();
    if cfg.bot_token.trim().is_empty() {
        return Err(bad("请先配置当前 Bot Token"));
    }
    // 临时注册表只存在内存，不为手动来源改写当前库的 storage_targets。
    let target = crate::db::StorageTarget {
        id: 1,
        api_base: cfg.api_base.clone(),
        bot_token: cfg.bot_token.clone(),
        chat_id: channel.clone(),
        created_at: now_ts(),
    };
    let mtproto = crate::mtproto::MtprotoConfig::from_values(
        state.vfs.db.clone(),
        &cfg.telegram_api_id,
        &cfg.telegram_api_hash,
    )?;
    let registry = TgRegistry::new(&[target], Some(1), vec![1], mtproto, cfg.proxy.clone())?;
    Ok(Source {
        api: registry.binding(1)?.api,
        channel,
        ids,
    })
}

fn normalize_channel(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if let Some(username) = value.strip_prefix('@') {
        anyhow::ensure!(
            username.len() >= 5
                && username.len() <= 32
                && username
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "频道用户名无效"
        );
        return Ok(format!("@{}", username.to_ascii_lowercase()));
    }
    anyhow::ensure!(
        value.starts_with("-100")
            && value.len() > 4
            && value[1..].bytes().all(|c| c.is_ascii_digit())
            && value.parse::<i64>().is_ok(),
        "请使用 -100 开头的频道 ID 或 @频道用户名"
    );
    Ok(value.to_string())
}
fn parse_messages(input: &str, channel: Option<&str>) -> anyhow::Result<(String, Vec<i64>)> {
    anyhow::ensure!(input.len() <= 32 * 1024, "消息输入过长");
    let mut channel = channel
        .filter(|s| !s.trim().is_empty())
        .map(normalize_channel)
        .transpose()?;
    let mut ids = Vec::new();
    let mut unique = HashSet::new();
    for token in input
        .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
        .filter(|s| !s.is_empty())
    {
        anyhow::ensure!(ids.len() < MAX_PARTS, "最多支持 128 条消息");
        let id_text = if token.contains('/') {
            let value = token
                .strip_prefix("https://t.me/")
                .or_else(|| token.strip_prefix("http://t.me/"))
                .or_else(|| token.strip_prefix("t.me/"))
                .ok_or_else(|| anyhow::anyhow!("仅支持 t.me 频道消息链接，不会抓取外部 URL"))?;
            let (path, query) = value
                .split_once('?')
                .map_or((value, None), |(a, b)| (a, Some(b)));
            anyhow::ensure!(
                query.is_none_or(|query| matches!(query, "single" | "single=1")),
                "消息链接含不支持的参数"
            );
            let parts: Vec<&str> = path.split('/').collect();
            let (source, id) = match parts.as_slice() {
                ["c", cid, id] if !cid.is_empty() && cid.bytes().all(|c| c.is_ascii_digit()) => {
                    (normalize_channel(&format!("-100{cid}"))?, *id)
                }
                [username, id] => (normalize_channel(&format!("@{username}"))?, *id),
                _ => anyhow::bail!("请提供具体频道消息链接，不支持频道首页、话题或邀请链接"),
            };
            if let Some(existing) = &channel {
                anyhow::ensure!(
                    *existing == source,
                    "所有消息必须属于同一频道；频道别名和数值 ID 请勿混用"
                );
            } else {
                channel = Some(source);
            }
            id
        } else {
            token
        };
        anyhow::ensure!(
            !id_text.is_empty() && id_text.bytes().all(|c| c.is_ascii_digit()),
            "消息 ID 无效"
        );
        let id: i64 = id_text.parse()?;
        anyhow::ensure!(id > 0 && unique.insert(id), "消息 ID 必须为正数且不能重复");
        ids.push(id);
    }
    anyhow::ensure!(!ids.is_empty(), "请输入全部备份消息链接或 ID");
    Ok((
        channel.ok_or_else(|| anyhow::anyhow!("使用消息 ID 时请指定来源频道"))?,
        ids,
    ))
}

#[derive(Clone, Debug)]
struct Part {
    base: String,
    index: usize,
    total: usize,
}
fn parse_part(name: &str) -> anyhow::Result<Part> {
    anyhow::ensure!(
        name.len() <= 240 && !name.contains(['/', '\\']) && !name.chars().any(char::is_control),
        "备份文件名不合法"
    );
    let (base, index, total) = if let Some((base, suffix)) = name.rsplit_once(".part") {
        let (index, total) = suffix
            .split_once("-of-")
            .ok_or_else(|| anyhow::anyhow!("分块文件名不合法"))?;
        anyhow::ensure!(
            index.len() == 4
                && total.len() == 4
                && index
                    .bytes()
                    .chain(total.bytes())
                    .all(|c| c.is_ascii_digit()),
            "分块编号必须是四位数字"
        );
        (base, index.parse::<usize>()?, total.parse::<usize>()?)
    } else {
        (name, 1, 1)
    };
    anyhow::ensure!(
        base.ends_with(".db") || base.ends_with(".db.age"),
        "只接受 .db / .db.age 数据库备份"
    );
    anyhow::ensure!(
        index > 0 && index <= total && total <= MAX_PARTS,
        "分块编号或总数超限"
    );
    Ok(Part {
        base: base.to_string(),
        index,
        total,
    })
}
fn validate_parts(parts: &[Part]) -> anyhow::Result<()> {
    let first = parts
        .first()
        .ok_or_else(|| anyhow::anyhow!("没有备份文件"))?;
    let mut seen = HashSet::new();
    anyhow::ensure!(
        parts.len() == first.total
            && parts
                .iter()
                .all(|p| p.base == first.base && p.total == first.total && seen.insert(p.index)),
        "备份分块缺失、重复或混用不同备份；请提供完整分块消息，不能猜测连续 ID"
    );
    Ok(())
}
fn require_id(journal: &Journal, id: &str) -> AppResult<()> {
    if journal.status.id.as_deref() != Some(id) {
        return Err(AppError::Conflict("恢复任务 ID 已失效，请刷新状态".into()));
    }
    Ok(())
}
fn bad(message: &str) -> AppError {
    AppError::BadRequest(message.into())
}
fn hex_digest(bytes: &[u8]) -> String {
    crate::engine::upload::to_hex(&Sha256::digest(bytes))
}
