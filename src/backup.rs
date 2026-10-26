//! SQLite 一致性快照的多渠道备份。
//!
//! 同一份快照可同时投递到 Telegram、WebDAV 与 S3；每个渠道独立记录
//! 最近成功时间，因此自动任务只重试失败或到期的渠道。

use crate::db::{Db, StoredMessage, now_ts};
use crate::error::{AppError, AppResult};
use crate::extras::Extras;
use crate::settings::Runtime;
use crate::telegram::UploadBody;
use crate::vfs::Vfs;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

const BACKUP_INTERVAL_SECS: i64 = 86_400;
const BACKUP_KEEP: usize = 7;
const TELEGRAM_HISTORY_KEY: &str = "extra.backup_msg_groups";
const LEGACY_TELEGRAM_HISTORY_KEY: &str = "extra.backup_msg_ids";
const WEBDAV_HISTORY_KEY: &str = "extra.backup_webdav_history";
const S3_HISTORY_KEY: &str = "extra.backup_s3_history";
const LEGACY_LAST_KEY: &str = "extra.backup_last_ts";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) static BACKUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Telegram,
    Webdav,
    S3,
}

impl Destination {
    fn kind(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Webdav => "webdav",
            Self::S3 => "s3",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram 频道",
            Self::Webdav => "WebDAV",
            Self::S3 => "S3 对象存储",
        }
    }

    fn last_key(self) -> &'static str {
        match self {
            Self::Telegram => "extra.backup_telegram_last_ts",
            Self::Webdav => "extra.backup_webdav_last_ts",
            Self::S3 => "extra.backup_s3_last_ts",
        }
    }

    /// 「无变化跳过」的基准：该渠道上次成功备份时的用户数据修订号。
    /// 按渠道独立记录，失败渠道不会拖累健康渠道的跳过判定。
    fn revision_key(self) -> &'static str {
        match self {
            Self::Telegram => "extra.backup_telegram_data_revision",
            Self::Webdav => "extra.backup_webdav_data_revision",
            Self::S3 => "extra.backup_s3_data_revision",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Selection {
    telegram: bool,
    webdav: bool,
    s3: bool,
}

impl Selection {
    fn enabled(extras: &Extras) -> Self {
        Self {
            telegram: extras.backup_telegram_enabled,
            webdav: extras.backup_webdav_enabled,
            s3: extras.backup_s3_enabled,
        }
    }

    fn any(self) -> bool {
        self.telegram || self.webdav || self.s3
    }
}

/// 管理端展示的各渠道最近成功时间。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupLastTimes {
    pub telegram: Option<i64>,
    pub webdav: Option<i64>,
    pub s3: Option<i64>,
}

/// 单个渠道的执行结果。某个渠道失败不影响其他渠道继续上传。
#[derive(Debug, Clone, Serialize)]
pub struct DestinationOutcome {
    pub kind: String,
    pub label: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupOutcome {
    pub ok: bool,
    pub name: String,
    /// 兼容旧管理端：Telegram 启用时为其分块数，否则为 1。
    pub parts: u64,
    pub succeeded: usize,
    pub failed: usize,
    pub destinations: Vec<DestinationOutcome>,
}

struct Snapshot {
    _temp_dir: tempfile::TempDir,
    path: PathBuf,
    len: u64,
    name: String,
    ts: i64,
    /// 快照生成时的用户数据修订号；快照期间的并发写入留在主库，
    /// 次日比对不相等会再补备一份，宁多勿漏。
    revision: i64,
}

/// 一次数据库备份可能对应多条 Telegram 分块消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TelegramBackupRecord {
    pub name: String,
    pub messages: Vec<StoredMessage>,
}

/// 远端历史与目标身份绑定；修改端点、桶或目录后不误删旧目标中的对象。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RemoteHistory {
    identity: String,
    objects: Vec<String>,
    #[serde(default)]
    pending_delete: Vec<String>,
}

pub async fn last_times(db: &Db) -> AppResult<BackupLastTimes> {
    let mut result = BackupLastTimes {
        telegram: setting_i64(db, Destination::Telegram.last_key()).await?,
        webdav: setting_i64(db, Destination::Webdav.last_key()).await?,
        s3: setting_i64(db, Destination::S3.last_key()).await?,
    };
    // 老版本只有一个时间键，且唯一渠道就是 Telegram。
    if result.telegram.is_none() && result.webdav.is_none() && result.s3.is_none() {
        result.telegram = setting_i64(db, LEGACY_LAST_KEY).await?;
    }
    Ok(result)
}

async fn setting_i64(db: &Db, key: &str) -> AppResult<Option<i64>> {
    Ok(db
        .setting_get(key)
        .await?
        .and_then(|value| value.parse().ok()))
}

/// 管理端手动触发：对当前所有已启用渠道执行一次备份。
pub async fn backup_now(vfs: &Vfs, runtime: &Runtime) -> AppResult<BackupOutcome> {
    let extras = crate::extras::load(&vfs.db).await?;
    backup_selected(vfs, runtime, &extras, Selection::enabled(&extras)).await
}

/// 自动任务：只投递已超过 24 小时或从未成功过的渠道。
pub async fn automatic_if_due(
    vfs: &Vfs,
    runtime: &Runtime,
    extras: &Extras,
) -> AppResult<Option<BackupOutcome>> {
    if !extras.backup_auto {
        return Ok(None);
    }
    let enabled = Selection::enabled(extras);
    if !enabled.any() {
        return Err(AppError::BadRequest(
            "每日自动备份已开启，但没有启用任何备份渠道".into(),
        ));
    }
    let now = now_ts();
    let last = last_times(&vfs.db).await?;
    let due = |value: Option<i64>| {
        value
            .map(|timestamp| now.saturating_sub(timestamp) >= BACKUP_INTERVAL_SECS)
            .unwrap_or(true)
    };
    // 「无变化跳过」：修订号与该渠道上次成功备份一致则本轮不投递。
    // 基准键在排除清单里，备份写它不会自增修订号；stored 为 None（从未成功
    // 备份过，含老库升级与新启用渠道）视为已变化，保证先拿到一份完整备份。
    // 与各渠道自己的基准比较，失败渠道不会拖累健康渠道的跳过判定。
    let unchanged = |destination: Destination| async move {
        let current = vfs.db.data_revision().await?;
        setting_i64(&vfs.db, destination.revision_key())
            .await
            .map(|stored| stored == Some(current))
    };
    let skip_unchanged = if extras.backup_skip_if_unchanged {
        [
            unchanged(Destination::Telegram).await?,
            unchanged(Destination::Webdav).await?,
            unchanged(Destination::S3).await?,
        ]
    } else {
        [false, false, false]
    };
    let selection = Selection {
        telegram: extras.backup_telegram_enabled
            && due(last.telegram)
            && !skip_unchanged[0],
        webdav: extras.backup_webdav_enabled && due(last.webdav) && !skip_unchanged[1],
        s3: extras.backup_s3_enabled && due(last.s3) && !skip_unchanged[2],
    };
    if !selection.any() {
        return Ok(None);
    }

    let outcome = backup_selected(vfs, runtime, extras, selection).await?;
    if outcome.failed > 0 {
        let failed = outcome
            .destinations
            .iter()
            .filter(|item| !item.ok)
            .map(|item| {
                format!(
                    "{}：{}",
                    item.label,
                    item.error.as_deref().unwrap_or("失败")
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        return Err(AppError::Internal(anyhow::anyhow!(
            "数据库自动备份部分失败，下一轮只重试失败渠道：{failed}"
        )));
    }
    Ok(Some(outcome))
}

async fn backup_selected(
    vfs: &Vfs,
    runtime: &Runtime,
    extras: &Extras,
    selection: Selection,
) -> AppResult<BackupOutcome> {
    if !selection.any() {
        return Err(AppError::BadRequest("请至少启用一个数据库备份渠道".into()));
    }
    vfs.db.require_background_enabled()?;
    let _guard = BACKUP_LOCK
        .try_lock()
        .map_err(|_| AppError::Conflict("备份或恢复任务正在运行".into()))?;
    vfs.db.require_background_enabled()?;
    let snapshot = create_snapshot(vfs, runtime, extras).await?;
    let proxy = runtime.current().proxy.clone();
    let _proxy_snapshot = crate::proxy::ProxySnapshot::new(proxy.clone())?;

    let telegram = async {
        if selection.telegram {
            Some(
                backup_telegram(vfs, runtime, extras, &snapshot)
                    .await
                    .map(|parts| (Destination::Telegram, parts)),
            )
        } else {
            None
        }
    };
    let webdav = async {
        if selection.webdav {
            Some(
                backup_webdav(&vfs.db, extras, &snapshot, &proxy)
                    .await
                    .map(|()| (Destination::Webdav, 1)),
            )
        } else {
            None
        }
    };
    let s3 = async {
        if selection.s3 {
            Some(
                backup_s3(&vfs.db, extras, &snapshot, &proxy)
                    .await
                    .map(|()| (Destination::S3, 1)),
            )
        } else {
            None
        }
    };
    let (telegram, webdav, s3) = tokio::join!(telegram, webdav, s3);

    let mut destinations = Vec::new();
    let mut legacy_parts = 1;
    for (destination, result) in [
        (Destination::Telegram, telegram),
        (Destination::Webdav, webdav),
        (Destination::S3, s3),
    ] {
        let Some(result) = result else { continue };
        match result {
            Ok((_, parts)) => {
                if destination == Destination::Telegram {
                    legacy_parts = parts;
                }
                destinations.push(DestinationOutcome {
                    kind: destination.kind().into(),
                    label: destination.label().into(),
                    ok: true,
                    parts: (destination == Destination::Telegram).then_some(parts),
                    error: None,
                });
            }
            Err(error) => {
                let summary = destination_error_summary(destination, &error);
                tracing::warn!(target = destination.kind(), error = %summary, "数据库备份渠道失败");
                destinations.push(DestinationOutcome {
                    kind: destination.kind().into(),
                    label: destination.label().into(),
                    ok: false,
                    parts: None,
                    error: Some(summary),
                });
            }
        }
    }
    let succeeded = destinations.iter().filter(|item| item.ok).count();
    let failed = destinations.len() - succeeded;
    if succeeded > 0 {
        // 保留旧键供旧管理端展示；渠道调度只读取各自的新键。
        vfs.db
            .setting_set(LEGACY_LAST_KEY, &snapshot.ts.to_string())
            .await?;
    }
    Ok(BackupOutcome {
        ok: succeeded > 0 && failed == 0,
        name: snapshot.name,
        parts: legacy_parts,
        succeeded,
        failed,
        destinations,
    })
}

fn destination_error_summary(destination: Destination, error: &AppError) -> String {
    match error {
        AppError::BadRequest(message) => message.clone(),
        AppError::Telegram(_) | AppError::TelegramRejected(_) | AppError::TelegramUncertain(_) => {
            error.to_string()
        }
        AppError::Db(_) => "记录备份状态失败".into(),
        AppError::Io(_) => "读取数据库快照失败".into(),
        _ => format!("{}备份失败，请查看服务日志", destination.label()),
    }
}

async fn create_snapshot(vfs: &Vfs, runtime: &Runtime, extras: &Extras) -> AppResult<Snapshot> {
    let cfg = runtime.current();
    // 在 VACUUM INTO 之前读修订号：这是本快照覆盖的数据状态。
    let revision = vfs.db.data_revision().await?;
    let temp_dir = tempfile::Builder::new()
        .prefix("tgdrive-backup-")
        .tempdir_in(&cfg.tmp_dir)?;
    let path = temp_dir.path().join("snapshot.db");
    let path_str = path
        .to_str()
        .ok_or_else(|| AppError::BadRequest("临时目录路径无效".into()))?;
    vfs.db.vacuum_into(path_str).await?;
    scrub_snapshot(path_str).await?;
    let ts = now_ts();
    let mut name = format!("tgdrive-backup-{}.db", fmt_compact_utc8(ts));
    let path = if extras.backup_encryption_password.is_empty() {
        path
    } else {
        let encrypted = temp_dir.path().join("snapshot.db.age");
        encrypt_file_blocking(
            path.clone(),
            encrypted.clone(),
            extras.backup_encryption_password.clone(),
        )
        .await?;
        // 明文快照不在临时目录里多留
        tokio::fs::remove_file(&path).await?;
        name.push_str(ENCRYPTED_SUFFIX);
        encrypted
    };
    let len = tokio::fs::metadata(&path).await?.len();
    Ok(Snapshot {
        _temp_dir: temp_dir,
        path,
        len,
        name,
        ts,
        revision,
    })
}

/// 加密备份的文件名后缀；内容是标准 age 格式，可用 `age -d` 或 `tgdrive decrypt-backup` 解密。
pub const ENCRYPTED_SUFFIX: &str = ".age";

async fn encrypt_file_blocking(src: PathBuf, dst: PathBuf, passphrase: String) -> AppResult<()> {
    tokio::task::spawn_blocking(move || encrypt_file(&src, &dst, passphrase, None))
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("备份加密任务失败：{error}")))?
}

/// 用口令把文件加密为 age 格式。`work_factor` 仅供测试调低 scrypt 成本；
/// 生产环境由 age 按本机性能自动标定（约 1 秒）。
fn encrypt_file(
    src: &Path,
    dst: &Path,
    passphrase: String,
    work_factor: Option<u8>,
) -> AppResult<()> {
    use std::io::Write;
    let passphrase = age::secrecy::SecretString::from(passphrase);
    let encryptor = match work_factor {
        None => age::Encryptor::with_user_passphrase(passphrase),
        Some(log_n) => {
            let mut recipient = age::scrypt::Recipient::new(passphrase);
            recipient.set_work_factor(log_n);
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .map_err(|error| AppError::Internal(anyhow::anyhow!("备份加密失败：{error}")))?
        }
    };
    let mut input = std::io::BufReader::new(std::fs::File::open(src)?);
    let output = std::io::BufWriter::new(std::fs::File::create(dst)?);
    let mut writer = encryptor.wrap_output(output)?;
    std::io::copy(&mut input, &mut writer)?;
    writer.finish()?.flush()?;
    Ok(())
}

/// 解密 age 口令加密的备份，供 `tgdrive decrypt-backup` 使用。
pub fn decrypt_file(src: &Path, dst: &Path, passphrase: String) -> anyhow::Result<()> {
    decrypt_file_limited(src, dst, passphrase, 8 * 1024 * 1024 * 1024)
}

/// 下载内容不可信：限制 scrypt 成本和明文输出，失败时不保留部分明文。
pub(crate) fn decrypt_file_limited(
    src: &Path,
    dst: &Path,
    passphrase: String,
    limit: u64,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut created = false;
    let result = (|| {
        let input = std::io::BufReader::new(std::fs::File::open(src)?);
        let decryptor = age::Decryptor::new_buffered(input)
            .map_err(|_| anyhow::anyhow!("不是有效的 age 加密文件"))?;
        let mut identity = age::scrypt::Identity::new(age::secrecy::SecretString::from(passphrase));
        // log_n=20 约 1 GiB KDF 内存，兼容本程序通常校准为 18/19 的历史备份。
        identity.set_max_work_factor(20);
        let reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|error| match error {
                age::DecryptError::ExcessiveWork { .. } => anyhow::anyhow!(
                    "加密工作因子超出在线恢复限制（最高 20），请在受控环境使用 age 离线解密后恢复"
                ),
                _ => anyhow::anyhow!("解密失败：备份密码错误或加密文件已损坏"),
            })?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dst)?;
        created = true;
        let mut output = std::io::BufWriter::new(file);
        let copied = std::io::copy(&mut reader.take(limit + 1), &mut output)?;
        anyhow::ensure!(copied <= limit, "解密输出超过安全上限");
        output.flush()?;
        output.get_ref().sync_all()?;
        Ok(())
    })();
    if result.is_err() && created {
        // 只清理本次确实创建的输出，不覆盖或删除已有文件。
        let _ = std::fs::remove_file(dst);
    }
    result
}

/// 快照会离开本机，恢复时用不到的凭据一律删掉。
/// Bot Token 必须保留：分块的 file_id 只能由对应 Bot 下载。
/// 备份加密密码不删：它只会出现在用自己加密的快照里，恢复后自动备份照常加密。
const SCRUBBED_SETTINGS: &[&str] = &[
    crate::auth::SECRET_KEY,
    crate::auth::TOTP_SECRET_KEY,
    crate::auth::TOTP_RECOVERY_KEY,
    crate::auth::WEBDAV_PASSWORD_KEY,
    "cfg.password",
    "cfg.telegram_api_hash",
    "extra.backup_webdav_password",
    "extra.backup_s3_secret_key",
    "extra.backup_s3_session_token",
];

async fn scrub_snapshot(path: &str) -> AppResult<()> {
    use sqlx::Connection;
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
    let mut conn = sqlx::SqliteConnection::connect_with(&opts).await?;
    for key in SCRUBBED_SETTINGS {
        sqlx::query("DELETE FROM settings WHERE k = ?")
            .bind(key)
            .execute(&mut conn)
            .await?;
    }
    // MTProto 会话等同登录凭据，恢复后会用 Bot Token 重新登录。
    sqlx::query("DELETE FROM settings WHERE k LIKE 'mtproto.session.%'")
        .execute(&mut conn)
        .await?;
    // 原生设备凭据同样不能随快照离开实例；兼容历史快照和仅有 settings 的夹具。
    for (table, deletion) in [
        ("device_refresh_tokens", "DELETE FROM device_refresh_tokens"),
        ("device_sessions", "DELETE FROM device_sessions"),
        ("upload_receipts", "DELETE FROM upload_receipts"),
    ] {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&mut conn)
        .await?;
        if exists > 0 {
            sqlx::query(deletion).execute(&mut conn).await?;
        }
    }
    // 重写文件，确保被删的值不残留在空闲页里。
    sqlx::query("VACUUM").execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

async fn mark_success(db: &Db, destination: Destination, snapshot: &Snapshot) -> AppResult<()> {
    // 修订号基准随渠道各自记录；写入失败时下轮比对不相等，会再备一次，自愈。
    db.setting_set(destination.revision_key(), &snapshot.revision.to_string())
        .await?;
    db.setting_set(destination.last_key(), &snapshot.ts.to_string())
        .await
}

// ---------- Telegram ----------

async fn backup_telegram(
    vfs: &Vfs,
    runtime: &Runtime,
    extras: &Extras,
    snapshot: &Snapshot,
) -> AppResult<u64> {
    let cfg = runtime.current();
    if !cfg.is_configured() {
        return Err(AppError::BadRequest(
            "还没配置 Telegram 存储，无法备份到频道".into(),
        ));
    }
    let target_id = cfg
        .storage_target_id
        .ok_or_else(|| AppError::BadRequest("当前 Telegram 配置缺少存储目标".into()))?;
    let target = vfs.tg.binding(target_id)?;
    crate::extras::validate_backup_chunk_size(&cfg.api_base, extras.backup_chunk_size)?;
    let part_count = snapshot.len.div_ceil(extras.backup_chunk_size).max(1);
    let mut uploaded = Vec::with_capacity(part_count as usize);

    for part_index in 0..part_count {
        let offset = part_index * extras.backup_chunk_size;
        let part_len = (snapshot.len - offset).min(extras.backup_chunk_size);
        let body_file =
            match create_backup_part(snapshot.path.clone(), cfg.tmp_dir.clone(), offset, part_len)
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    enqueue_telegram_cleanup(vfs, &uploaded).await;
                    return Err(error);
                }
            };
        let filename = backup_part_filename(&snapshot.name, part_index, part_count);
        let caption = backup_part_caption(
            part_index,
            part_count,
            snapshot.name.ends_with(ENCRYPTED_SUFFIX),
        );
        let body = UploadBody::TempFile {
            file: body_file,
            len: part_len,
        };
        target.api.wait_upload_turn().await;
        let message = match target
            .api
            .send_document(&filename, Some(caption), None, &body)
            .await
        {
            Ok(message) => message,
            Err(error) => {
                enqueue_telegram_cleanup(vfs, &uploaded).await;
                return Err(error);
            }
        };
        uploaded.push(StoredMessage {
            storage_target_id: target_id,
            message_id: message.message_id,
        });
    }

    let mut history = match load_telegram_history(&vfs.db).await {
        Ok(history) => history,
        Err(error) => {
            enqueue_telegram_cleanup(vfs, &uploaded).await;
            return Err(error);
        }
    };
    let remove_count = history.len().saturating_add(1).saturating_sub(BACKUP_KEEP);
    let expired: Vec<TelegramBackupRecord> = history.drain(..remove_count).collect();
    history.push(TelegramBackupRecord {
        name: snapshot.name.clone(),
        messages: uploaded.clone(),
    });
    let serialized =
        serde_json::to_string(&history).map_err(|error| AppError::Internal(error.into()))?;
    if let Err(error) = vfs.db.setting_set(TELEGRAM_HISTORY_KEY, &serialized).await {
        enqueue_telegram_cleanup(vfs, &uploaded).await;
        return Err(error);
    }
    if let Err(error) = vfs.db.setting_del(LEGACY_TELEGRAM_HISTORY_KEY).await {
        tracing::warn!("移除旧版数据库备份记录失败，下次将继续使用新记录：{error}");
    }
    for record in expired {
        for message in record.messages {
            if let Err(error) = vfs.db.enqueue_delete_message(message).await {
                tracing::warn!("记录过期 Telegram 备份清理任务失败：{error}");
            }
        }
    }
    mark_success(&vfs.db, Destination::Telegram, snapshot).await?;
    vfs.uploader.process_pending_deletes();
    Ok(part_count)
}

async fn create_backup_part(
    snapshot: PathBuf,
    temp_dir: PathBuf,
    offset: u64,
    len: u64,
) -> AppResult<tempfile::NamedTempFile> {
    tokio::task::spawn_blocking(move || -> std::io::Result<tempfile::NamedTempFile> {
        let mut source = std::fs::File::open(snapshot)?;
        source.seek(SeekFrom::Start(offset))?;
        let mut part = tempfile::Builder::new()
            .prefix("tgdrive-backup-part-")
            .tempfile_in(temp_dir)?;
        let copied = std::io::copy(&mut source.take(len), part.as_file_mut())?;
        if copied != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("数据库备份分块应为 {len} 字节，实际复制 {copied} 字节"),
            ));
        }
        Ok(part)
    })
    .await
    .map_err(|error| AppError::Internal(anyhow::anyhow!("数据库备份分块任务失败：{error}")))?
    .map_err(AppError::from)
}

fn backup_part_filename(base: &str, part_index: u64, part_count: u64) -> String {
    if part_count == 1 {
        base.to_string()
    } else {
        format!("{base}.part{:04}-of-{part_count:04}", part_index + 1)
    }
}

fn backup_part_caption(part_index: u64, part_count: u64, encrypted: bool) -> String {
    let note = if encrypted {
        "，已用备份密码加密"
    } else {
        ""
    };
    if part_count == 1 {
        format!("tgdrive 数据库备份（含配置与文件索引{note}，请勿删除最近几份）")
    } else {
        format!(
            "tgdrive 数据库备份分块 {}/{}（按文件名顺序二进制合并后恢复{note}）",
            part_index + 1,
            part_count
        )
    }
}

pub(crate) async fn load_telegram_history(db: &Db) -> AppResult<Vec<TelegramBackupRecord>> {
    if let Some(raw) = db.setting_get(TELEGRAM_HISTORY_KEY).await? {
        return serde_json::from_str(&raw).map_err(|_| {
            AppError::BadRequest("数据库备份分组记录损坏，请手动输入消息链接；原记录未修改".into())
        });
    }
    let Some(raw) = db.setting_get(LEGACY_TELEGRAM_HISTORY_KEY).await? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str::<Vec<StoredMessage>>(&raw)
        .map_err(|_| {
            AppError::BadRequest("旧版数据库备份记录损坏，请手动输入消息链接；原记录未修改".into())
        })?
        .into_iter()
        .map(|message| TelegramBackupRecord {
            name: "旧版数据库备份".into(),
            messages: vec![message],
        })
        .collect())
}

async fn enqueue_telegram_cleanup(vfs: &Vfs, messages: &[StoredMessage]) {
    for message in messages {
        if let Err(error) = vfs.db.enqueue_delete_message(*message).await {
            tracing::error!(
                "清理失败数据库备份的消息 {} 时无法写入删除队列：{error}",
                message.message_id
            );
        }
    }
    vfs.uploader.process_pending_deletes();
}

// ---------- WebDAV ----------

async fn backup_webdav(
    db: &Db,
    extras: &Extras,
    snapshot: &Snapshot,
    proxy: &crate::proxy::ProxyConfig,
) -> AppResult<()> {
    let client = backup_http_client(proxy)?;
    let url = webdav_object_url(&extras.backup_webdav_url, &snapshot.name)?;
    let file = tokio::fs::File::open(&snapshot.path).await?;
    let mut request = client
        .put(url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, snapshot.len)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)));
    if !extras.backup_webdav_username.is_empty() {
        request = request.basic_auth(
            &extras.backup_webdav_username,
            Some(&extras.backup_webdav_password),
        );
    }
    let response = request
        .send()
        .await
        .map_err(|error| remote_request_error("WebDAV 上传", &error))?;
    if !response.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "WebDAV 上传返回 {}",
            response.status()
        )));
    }

    let identity = webdav_identity(extras);
    let pending =
        match update_remote_history(db, WEBDAV_HISTORY_KEY, &identity, &snapshot.name).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!("保存 WebDAV 备份历史失败，将尝试移除刚上传的文件：{error}");
                let _ = delete_webdav(&client, extras, &snapshot.name).await;
                return Err(error);
            }
        };
    let mut failed_cleanup = Vec::new();
    for name in pending {
        if let Err(error) = delete_webdav(&client, extras, &name).await {
            tracing::warn!(file = %name, "清理过期 WebDAV 备份失败：{error}");
            failed_cleanup.push(name);
        }
    }
    if let Err(error) =
        finish_remote_cleanup(db, WEBDAV_HISTORY_KEY, &identity, failed_cleanup).await
    {
        // 第一次保存的历史仍保留完整待删队列，下次会安全地重复 DELETE。
        tracing::warn!("更新 WebDAV 待删备份队列失败，下次将重新清理：{error}");
    }
    mark_success(db, Destination::Webdav, snapshot).await
}

fn webdav_object_url(base: &str, name: &str) -> AppResult<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|_| AppError::BadRequest("WebDAV 目录 URL 无效".into()))?;
    url.path_segments_mut()
        .map_err(|_| AppError::BadRequest("WebDAV 目录 URL 不能作为文件目录".into()))?
        .pop_if_empty()
        .push(name);
    Ok(url)
}

async fn delete_webdav(client: &reqwest::Client, extras: &Extras, name: &str) -> AppResult<()> {
    let url = webdav_object_url(&extras.backup_webdav_url, name)?;
    let mut request = client.delete(url);
    if !extras.backup_webdav_username.is_empty() {
        request = request.basic_auth(
            &extras.backup_webdav_username,
            Some(&extras.backup_webdav_password),
        );
    }
    let response = request
        .send()
        .await
        .map_err(|error| remote_request_error("WebDAV 删除", &error))?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "WebDAV 删除返回 {}",
            response.status()
        )))
    }
}

fn webdav_identity(extras: &Extras) -> String {
    sha256_hex(
        format!(
            "{}\n{}",
            extras.backup_webdav_url, extras.backup_webdav_username
        )
        .as_bytes(),
    )
}

// ---------- S3 ----------

async fn backup_s3(
    db: &Db,
    extras: &Extras,
    snapshot: &Snapshot,
    proxy: &crate::proxy::ProxyConfig,
) -> AppResult<()> {
    let client = backup_http_client(proxy)?;
    let key = s3_object_key(&extras.backup_s3_prefix, &snapshot.name);
    let url = s3_object_url(extras, &key)?;
    let payload_hash = sha256_file(&snapshot.path).await?;
    let headers = s3_signed_headers("PUT", &url, extras, &payload_hash, now_ts())?;
    let file = tokio::fs::File::open(&snapshot.path).await?;
    let response = client
        .put(url)
        .headers(headers)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, snapshot.len)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .map_err(|error| remote_request_error("S3 上传", &error))?;
    if !response.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "S3 上传返回 {}",
            response.status()
        )));
    }

    let identity = s3_identity(extras);
    let pending = match update_remote_history(db, S3_HISTORY_KEY, &identity, &key).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!("保存 S3 备份历史失败，将尝试移除刚上传的对象：{error}");
            let _ = delete_s3(&client, extras, &key).await;
            return Err(error);
        }
    };
    let mut failed_cleanup = Vec::new();
    for expired_key in pending {
        if let Err(error) = delete_s3(&client, extras, &expired_key).await {
            tracing::warn!(key = %expired_key, "清理过期 S3 备份失败：{error}");
            failed_cleanup.push(expired_key);
        }
    }
    if let Err(error) = finish_remote_cleanup(db, S3_HISTORY_KEY, &identity, failed_cleanup).await {
        tracing::warn!("更新 S3 待删备份队列失败，下次将重新清理：{error}");
    }
    mark_success(db, Destination::S3, snapshot).await
}

fn s3_object_key(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{name}", prefix.trim_matches('/'))
    }
}

fn s3_object_url(extras: &Extras, key: &str) -> AppResult<reqwest::Url> {
    let mut url = reqwest::Url::parse(&extras.backup_s3_endpoint)
        .map_err(|_| AppError::BadRequest("S3 端点无效".into()))?;
    if extras.backup_s3_path_style {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| AppError::BadRequest("S3 端点不能作为对象地址".into()))?;
        segments.pop_if_empty().push(&extras.backup_s3_bucket);
        for segment in key.split('/') {
            segments.push(segment);
        }
    } else {
        let host = url
            .host_str()
            .ok_or_else(|| AppError::BadRequest("S3 端点缺少主机名".into()))?;
        let virtual_host = format!("{}.{}", extras.backup_s3_bucket, host);
        url.set_host(Some(&virtual_host))
            .map_err(|_| AppError::BadRequest("S3 虚拟主机地址无效".into()))?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| AppError::BadRequest("S3 端点不能作为对象地址".into()))?;
        segments.pop_if_empty();
        for segment in key.split('/') {
            segments.push(segment);
        }
    }
    Ok(url)
}

async fn delete_s3(client: &reqwest::Client, extras: &Extras, key: &str) -> AppResult<()> {
    let url = s3_object_url(extras, key)?;
    let headers = s3_signed_headers("DELETE", &url, extras, EMPTY_SHA256, now_ts())?;
    let response = client
        .delete(url)
        .headers(headers)
        .send()
        .await
        .map_err(|error| remote_request_error("S3 删除", &error))?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "S3 删除返回 {}",
            response.status()
        )))
    }
}

fn s3_identity(extras: &Extras) -> String {
    sha256_hex(
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            extras.backup_s3_endpoint,
            extras.backup_s3_region,
            extras.backup_s3_bucket,
            extras.backup_s3_prefix,
            extras.backup_s3_access_key,
            extras.backup_s3_path_style
        )
        .as_bytes(),
    )
}

fn s3_signed_headers(
    method: &str,
    url: &reqwest::Url,
    extras: &Extras,
    payload_hash: &str,
    timestamp: i64,
) -> AppResult<reqwest::header::HeaderMap> {
    let host = url
        .host()
        .ok_or_else(|| AppError::BadRequest("S3 对象地址缺少主机名".into()))?;
    let mut host = host.to_string();
    if let Some(port) = url.port() {
        host.push(':');
        host.push_str(&port.to_string());
    }
    let (date, amz_date) = amz_datetime(timestamp);
    let mut canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let mut signed_headers = "host;x-amz-content-sha256;x-amz-date".to_string();
    if !extras.backup_s3_session_token.is_empty() {
        canonical_headers.push_str(&format!(
            "x-amz-security-token:{}\n",
            extras.backup_s3_session_token.trim()
        ));
        signed_headers.push_str(";x-amz-security-token");
    }
    let canonical_request = format!(
        "{method}\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        url.path()
    );
    let scope = format!("{date}/{}/s3/aws4_request", extras.backup_s3_region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let date_key = hmac_sha256(
        format!("AWS4{}", extras.backup_s3_secret_key).as_bytes(),
        date.as_bytes(),
    );
    let region_key = hmac_sha256(&date_key, extras.backup_s3_region.as_bytes());
    let service_key = hmac_sha256(&region_key, b"s3");
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    let signature = bytes_hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        extras.backup_s3_access_key
    );

    let mut headers = reqwest::header::HeaderMap::new();
    insert_header(&mut headers, reqwest::header::HOST, &host)?;
    insert_header(&mut headers, "x-amz-content-sha256", payload_hash)?;
    insert_header(&mut headers, "x-amz-date", &amz_date)?;
    if !extras.backup_s3_session_token.is_empty() {
        insert_header(
            &mut headers,
            "x-amz-security-token",
            extras.backup_s3_session_token.trim(),
        )?;
    }
    insert_header(&mut headers, reqwest::header::AUTHORIZATION, &authorization)?;
    Ok(headers)
}

fn insert_header(
    headers: &mut reqwest::header::HeaderMap,
    name: impl reqwest::header::IntoHeaderName,
    value: &str,
) -> AppResult<()> {
    let value = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| AppError::BadRequest("S3 凭据包含无效字符".into()))?;
    headers.insert(name, value);
    Ok(())
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC-SHA256 接受任意长度密钥");
    Mac::update(&mut mac, value);
    mac.finalize().into_bytes().to_vec()
}

async fn sha256_file(path: &Path) -> AppResult<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(bytes_hex(&hasher.finalize()))
}

// ---------- 远端历史与公共工具 ----------

async fn update_remote_history(
    db: &Db,
    setting_key: &str,
    identity: &str,
    object: &str,
) -> AppResult<Vec<String>> {
    let mut history = db
        .setting_get(setting_key)
        .await?
        .and_then(|raw| {
            serde_json::from_str::<RemoteHistory>(&raw)
                .map_err(|error| tracing::warn!("远端备份历史损坏，将停止管理旧对象：{error}"))
                .ok()
        })
        .filter(|history| history.identity == identity)
        .unwrap_or_else(|| RemoteHistory {
            identity: identity.to_string(),
            objects: Vec::new(),
            pending_delete: Vec::new(),
        });
    // 同名 PUT 是覆盖操作，不让重复记录在滚动清理时误删刚上传的对象。
    history.objects.retain(|item| item != object);
    history.pending_delete.retain(|item| item != object);
    history.objects.push(object.to_string());
    let remove_count = history.objects.len().saturating_sub(BACKUP_KEEP);
    let expired: Vec<String> = history.objects.drain(..remove_count).collect();
    for object in expired {
        if !history.pending_delete.contains(&object) {
            history.pending_delete.push(object);
        }
    }
    let pending = history.pending_delete.clone();
    let serialized =
        serde_json::to_string(&history).map_err(|error| AppError::Internal(error.into()))?;
    db.setting_set(setting_key, &serialized).await?;
    Ok(pending)
}

async fn finish_remote_cleanup(
    db: &Db,
    setting_key: &str,
    identity: &str,
    failed: Vec<String>,
) -> AppResult<()> {
    let Some(raw) = db.setting_get(setting_key).await? else {
        return Ok(());
    };
    let mut history: RemoteHistory = serde_json::from_str(&raw)
        .map_err(|error| AppError::Internal(anyhow::anyhow!("远端备份历史损坏：{error}")))?;
    if history.identity != identity {
        return Ok(());
    }
    history.pending_delete = failed;
    let serialized =
        serde_json::to_string(&history).map_err(|error| AppError::Internal(error.into()))?;
    db.setting_set(setting_key, &serialized).await
}

fn backup_http_client(proxy: &crate::proxy::ProxyConfig) -> AppResult<reqwest::Client> {
    proxy
        .http_builder()?
        .connect_timeout(Duration::from_secs(15))
        .timeout(HTTP_TIMEOUT)
        // 带认证的 PUT 不跨主机自动跳转，避免凭据或签名被错误转发。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| AppError::BadRequest("构造备份 HTTP 客户端失败".into()))
}

fn remote_request_error(action: &str, error: &reqwest::Error) -> AppError {
    AppError::BadRequest(format!(
        "{action}{}",
        crate::telegram::reqwest_error_summary(error)
    ))
}

fn sha256_hex(value: &[u8]) -> String {
    bytes_hex(&Sha256::digest(value))
}

fn bytes_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(value.len() * 2);
    for byte in value {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

/// unix 秒 → 东八区文件名时间，精确到秒以减少并发备份重名。
fn fmt_compact_utc8(timestamp: i64) -> String {
    let timestamp = timestamp + 8 * 3600;
    let (year, month, day) = civil_from_days(timestamp.div_euclid(86_400));
    let seconds = timestamp.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn amz_datetime(timestamp: i64) -> (String, String) {
    let (year, month, day) = civil_from_days(timestamp.div_euclid(86_400));
    let seconds = timestamp.rem_euclid(86_400);
    let date = format!("{year:04}{month:02}{day:02}");
    let date_time = format!(
        "{date}T{:02}{:02}{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    );
    (date, date_time)
}

/// 天数（自 1970-01-01）→ 公历年月日；Howard Hinnant 的 civil_from_days。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = (if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::to_bytes;
    use axum::extract::{Request, State};
    use axum::http::{HeaderMap, Method, StatusCode};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct CapturedRequest {
        method: Method,
        path: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    async fn capture_request(
        State(requests): State<Arc<Mutex<Vec<CapturedRequest>>>>,
        request: Request,
    ) -> StatusCode {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
        requests.lock().unwrap().push(CapturedRequest {
            method: parts.method,
            path: parts.uri.path().to_string(),
            headers: parts.headers,
            body,
        });
        StatusCode::OK
    }

    async fn mock_storage_server() -> (
        String,
        Arc<Mutex<Vec<CapturedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(capture_request)
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), requests, task)
    }

    fn test_snapshot(data: &[u8]) -> Snapshot {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("snapshot.db");
        std::fs::write(&path, data).unwrap();
        Snapshot {
            _temp_dir: temp_dir,
            path,
            len: data.len() as u64,
            name: "tgdrive-backup-test.db".into(),
            ts: 1_786_492_800,
            revision: 0,
        }
    }

    fn s3_extras() -> Extras {
        Extras {
            trash_retention_days: 30,
            backup_auto: true,
            backup_skip_if_unchanged: false,
            backup_chunk_size: crate::extras::DEFAULT_BACKUP_CHUNK_SIZE,
            backup_telegram_enabled: false,
            backup_webdav_enabled: false,
            backup_webdav_url: String::new(),
            backup_webdav_username: String::new(),
            backup_webdav_password: String::new(),
            backup_s3_enabled: true,
            backup_s3_endpoint: "https://s3.example.com/root".into(),
            backup_s3_region: "us-east-1".into(),
            backup_s3_bucket: "backup-bucket".into(),
            backup_s3_prefix: "数据库/每日".into(),
            backup_s3_access_key: "AKIDEXAMPLE".into(),
            backup_s3_secret_key: "secret".into(),
            backup_s3_session_token: String::new(),
            backup_s3_path_style: true,
            inbox_bot_enabled: false,
            inbox_channel_enabled: false,
            inbox_allowed_chats: String::new(),
            inbox_dir: "/收件箱".into(),
            inbox_video_dir: "/收件箱/视频".into(),
            inbox_image_dir: "/收件箱/图片".into(),
            inbox_book_dir: "/收件箱/书籍".into(),
            inbox_rules: Vec::new(),
            auto_delete_message: false,
            backup_encryption_password: String::new(),
        }
    }

    #[tokio::test]
    async fn snapshot_scrub_removes_credentials_but_keeps_bot_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.db");
        let path = path.to_str().unwrap();
        {
            use sqlx::Connection;
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true);
            let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("CREATE TABLE device_sessions (id TEXT PRIMARY KEY, name TEXT)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("CREATE TABLE device_refresh_tokens (token_hash TEXT, session_id TEXT)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("INSERT INTO device_sessions VALUES ('device-XYZ','iPhone-XYZ')")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("INSERT INTO device_refresh_tokens VALUES ('refresh-XYZ','device-XYZ')")
                .execute(&mut conn)
                .await
                .unwrap();
            for (k, v) in [
                ("secret", "sign-key-XYZ"),
                ("cfg.password", "hunter2-XYZ"),
                ("cfg.bot_token", "123:bot"),
                ("mtproto.session.1", "authkey-XYZ"),
                ("extra.backup_s3_secret_key", "s3-XYZ"),
                ("auth.totp_secret", "totp-XYZ"),
                ("auth.totp_recovery", "recovery-XYZ"),
                ("auth.webdav_password", "webdav-XYZ"),
            ] {
                sqlx::query("INSERT INTO settings (k, v) VALUES (?, ?)")
                    .bind(k)
                    .bind(v)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            conn.close().await.unwrap();
        }
        scrub_snapshot(path).await.unwrap();
        let raw = std::fs::read(path).unwrap();
        assert!(
            !raw.windows(4).any(|w| w == b"-XYZ"),
            "凭据残留在快照文件里"
        );
        assert!(raw.windows(7).any(|w| w == b"123:bot"));
    }

    #[test]
    fn encrypted_backup_roundtrip_and_wrong_password() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("snap.db");
        let encrypted = dir.path().join("snap.db.age");
        let restored = dir.path().join("restored.db");
        let content: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(&plain, &content).unwrap();

        encrypt_file(&plain, &encrypted, "correct horse".into(), Some(10)).unwrap();
        let raw = std::fs::read(&encrypted).unwrap();
        assert!(raw.starts_with(b"age-encryption.org/v1"));
        assert!(!raw.windows(64).any(|w| w == &content[..64]));

        decrypt_file(&encrypted, &restored, "correct horse".into()).unwrap();
        assert_eq!(std::fs::read(&restored).unwrap(), content);
        assert!(decrypt_file(&encrypted, &restored, "wrong".into()).is_err());
        assert!(decrypt_file(&plain, &restored, "correct horse".into()).is_err());
    }

    #[test]
    fn default_encryption_is_accepted_by_restore_limits() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("snap.db");
        let encrypted = dir.path().join("snap.db.age");
        let restored = dir.path().join("restored.db");
        std::fs::write(&plain, b"default encryption regression").unwrap();
        // 不降低工作因子，覆盖实际备份使用的默认自动校准分支。
        encrypt_file(&plain, &encrypted, "current-default".into(), None).unwrap();
        decrypt_file_limited(&encrypted, &restored, "current-default".into(), 4096).unwrap();
        assert_eq!(
            std::fs::read(&restored).unwrap(),
            b"default encryption regression"
        );
    }

    #[test]
    fn dates_and_backup_names_are_stable() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_949), (2024, 8, 14));
        assert_eq!(fmt_compact_utc8(1_786_492_800), "20260812-080000");
        assert_eq!(
            amz_datetime(1_786_492_800),
            ("20260812".into(), "20260812T000000Z".into())
        );
    }

    #[test]
    fn backup_part_names_are_stable_and_sortable() {
        let base = "tgdrive-backup-20260816-120000.db";
        assert_eq!(backup_part_filename(base, 0, 1), base);
        assert_eq!(
            backup_part_filename(base, 0, 12),
            "tgdrive-backup-20260816-120000.db.part0001-of-0012"
        );
        assert_eq!(
            backup_part_filename(base, 11, 12),
            "tgdrive-backup-20260816-120000.db.part0012-of-0012"
        );
    }

    #[test]
    fn remote_urls_encode_each_object_path_segment() {
        let webdav = webdav_object_url("https://dav.example.com/数据库", "备份 1.db").unwrap();
        assert_eq!(
            webdav.as_str(),
            "https://dav.example.com/%E6%95%B0%E6%8D%AE%E5%BA%93/%E5%A4%87%E4%BB%BD%201.db"
        );

        let extras = s3_extras();
        let key = s3_object_key(&extras.backup_s3_prefix, "备份 1.db");
        let url = s3_object_url(&extras, &key).unwrap();
        assert_eq!(
            url.as_str(),
            "https://s3.example.com/root/backup-bucket/%E6%95%B0%E6%8D%AE%E5%BA%93/%E6%AF%8F%E6%97%A5/%E5%A4%87%E4%BB%BD%201.db"
        );
        let headers = s3_signed_headers("PUT", &url, &extras, EMPTY_SHA256, 1_786_492_800).unwrap();
        assert!(
            headers[reqwest::header::AUTHORIZATION]
                .to_str()
                .unwrap()
                .ends_with(
                    "Signature=2d6812d26d7241aff82178c3badb61ead5ad322b7b42490ef5646ab3b7876f9b"
                )
        );
    }

    #[test]
    fn s3_signature_contains_expected_scope_and_signed_headers() {
        let extras = s3_extras();
        let url = s3_object_url(&extras, "tgdrive/test.db").unwrap();
        let headers = s3_signed_headers("PUT", &url, &extras, EMPTY_SHA256, 1_786_492_800).unwrap();
        assert_eq!(headers["x-amz-date"], "20260812T000000Z");
        let authorization = headers[reqwest::header::AUTHORIZATION].to_str().unwrap();
        assert!(
            authorization.contains("Credential=AKIDEXAMPLE/20260812/us-east-1/s3/aws4_request")
        );
        assert!(authorization.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(authorization.ends_with(
            "Signature=e15f7017bac8fa2eaa3b4a4b6b6dcdcfdd4d1c031917b36787f768ee8e90e2e5"
        ));
    }

    #[tokio::test]
    async fn webdav_upload_streams_snapshot_with_basic_auth() {
        let (endpoint, requests, server) = mock_storage_server().await;
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("webdav.db").to_str().unwrap())
            .await
            .unwrap();
        let mut extras = s3_extras();
        extras.backup_webdav_enabled = true;
        extras.backup_webdav_url = format!("{endpoint}/backups");
        extras.backup_webdav_username = "alice".into();
        extras.backup_webdav_password = "secret".into();
        let snapshot = test_snapshot(b"sqlite snapshot");

        backup_webdav(
            &db,
            &extras,
            &snapshot,
            &crate::proxy::ProxyConfig::default(),
        )
        .await
        .unwrap();
        server.abort();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, Method::PUT);
            assert_eq!(requests[0].path, "/backups/tgdrive-backup-test.db");
            assert_eq!(
                requests[0].headers["authorization"],
                "Basic YWxpY2U6c2VjcmV0"
            );
            assert_eq!(requests[0].body, b"sqlite snapshot");
        }
        assert_eq!(
            setting_i64(&db, Destination::Webdav.last_key())
                .await
                .unwrap(),
            Some(snapshot.ts)
        );
    }

    #[tokio::test]
    async fn s3_upload_streams_snapshot_with_sigv4() {
        let (endpoint, requests, server) = mock_storage_server().await;
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("s3.db").to_str().unwrap())
            .await
            .unwrap();
        let mut extras = s3_extras();
        extras.backup_s3_endpoint = format!("{endpoint}/api");
        extras.backup_s3_prefix = "tgdrive".into();
        let snapshot = test_snapshot(b"sqlite snapshot");

        backup_s3(
            &db,
            &extras,
            &snapshot,
            &crate::proxy::ProxyConfig::default(),
        )
        .await
        .unwrap();
        server.abort();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, Method::PUT);
            assert_eq!(
                requests[0].path,
                "/api/backup-bucket/tgdrive/tgdrive-backup-test.db"
            );
            assert!(requests[0].headers.contains_key("x-amz-content-sha256"));
            assert!(
                requests[0].headers["authorization"]
                    .to_str()
                    .unwrap()
                    .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            );
            assert_eq!(requests[0].body, b"sqlite snapshot");
        }
        assert_eq!(
            setting_i64(&db, Destination::S3.last_key()).await.unwrap(),
            Some(snapshot.ts)
        );
    }

    #[tokio::test]
    async fn remote_backups_use_proxy_without_replacing_destination_credentials() {
        let (endpoint, requests, server) = mock_storage_server().await;
        let proxy_url = endpoint.replacen("http://", "http://proxy-user:proxy-pass@", 1);
        let proxy = crate::proxy::ProxyConfig::new(true, &proxy_url).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("proxy-backup.db").to_str().unwrap())
            .await
            .unwrap();
        let mut extras = s3_extras();
        extras.backup_webdav_url = "http://dav-proxy-test.invalid/backups".into();
        extras.backup_webdav_username = "alice".into();
        extras.backup_webdav_password = "secret".into();
        extras.backup_s3_endpoint = "http://s3-proxy-test.invalid/api".into();
        let snapshot = test_snapshot(b"proxied snapshot");
        backup_webdav(&db, &extras, &snapshot, &proxy)
            .await
            .unwrap();
        backup_s3(&db, &extras, &snapshot, &proxy).await.unwrap();
        server.abort();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"],
            "Basic YWxpY2U6c2VjcmV0"
        );
        assert!(
            requests[1].headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("AWS4-HMAC-SHA256")
        );
        for request in requests.iter() {
            assert_eq!(request.method, Method::PUT);
            assert_eq!(
                request.headers["proxy-authorization"],
                "Basic cHJveHktdXNlcjpwcm94eS1wYXNz"
            );
            assert_eq!(request.body, b"proxied snapshot");
        }
    }

    #[tokio::test]
    async fn backup_part_copies_only_requested_range() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory.path().join("snapshot.db");
        std::fs::write(&snapshot, b"0123456789").unwrap();

        let part = create_backup_part(snapshot, directory.path().to_path_buf(), 3, 4)
            .await
            .unwrap();
        assert_eq!(std::fs::read(part.path()).unwrap(), b"3456");
    }

    #[tokio::test]
    async fn legacy_telegram_messages_become_single_part_groups() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("history.db").to_str().unwrap())
            .await
            .unwrap();
        let legacy = vec![
            StoredMessage {
                storage_target_id: 1,
                message_id: 10,
            },
            StoredMessage {
                storage_target_id: 2,
                message_id: 20,
            },
        ];
        db.setting_set(
            LEGACY_TELEGRAM_HISTORY_KEY,
            &serde_json::to_string(&legacy).unwrap(),
        )
        .await
        .unwrap();

        let history = load_telegram_history(&db).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].messages, [legacy[0]]);
        assert_eq!(history[1].messages, [legacy[1]]);
    }

    #[tokio::test]
    async fn remote_history_deduplicates_overwrites_and_keeps_seven() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("remote.db").to_str().unwrap())
            .await
            .unwrap();
        for index in 0..8 {
            update_remote_history(&db, S3_HISTORY_KEY, "target-a", &format!("{index}.db"))
                .await
                .unwrap();
        }
        let pending = update_remote_history(&db, S3_HISTORY_KEY, "target-a", "7.db")
            .await
            .unwrap();
        assert_eq!(pending, ["0.db"]);
        finish_remote_cleanup(&db, S3_HISTORY_KEY, "target-a", Vec::new())
            .await
            .unwrap();
        let history: RemoteHistory =
            serde_json::from_str(&db.setting_get(S3_HISTORY_KEY).await.unwrap().unwrap()).unwrap();
        assert_eq!(history.objects.len(), 7);
        assert_eq!(history.objects.last().unwrap(), "7.db");
        assert!(history.pending_delete.is_empty());
    }

    #[tokio::test]
    async fn partial_failure_records_revision_only_for_successful_channels() {
        let (endpoint, requests, server) = mock_storage_server().await;
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("partial.db").to_str().unwrap())
            .await
            .unwrap();
        let mut extras = s3_extras();
        // WebDAV 指向 mock 必成功；S3 指向不可达端口必失败。
        extras.backup_webdav_enabled = true;
        extras.backup_webdav_url = format!("{endpoint}/backups");
        extras.backup_s3_endpoint = "http://127.0.0.1:9/unreachable".into();
        let snapshot = test_snapshot(b"partial snapshot");

        backup_webdav(&db, &extras, &snapshot, &crate::proxy::ProxyConfig::default())
            .await
            .unwrap();
        assert!(backup_s3(
            &db,
            &extras,
            &snapshot,
            &crate::proxy::ProxyConfig::default(),
        )
        .await
        .is_err());
        server.abort();

        // 成功渠道记录了基准；失败渠道不记录，重试轮不会被 skip 阻断。
        assert_eq!(
            setting_i64(&db, Destination::Webdav.revision_key())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            setting_i64(&db, Destination::S3.revision_key())
                .await
                .unwrap(),
            None,
            "失败渠道不应记录修订号基准"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
