//! SQLite 元数据层:目录树(nodes)与分块映射(chunks)
//!
//! 约定:根目录 id=1;列表/解析只看 status='ready' 的行;
//! 覆盖写通过 uploading 影子行 + finalize 事务原子替换实现。

use crate::error::{AppError, AppResult};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "apps/db.rs"]
pub mod apps;
mod copy;
mod delete_jobs;
pub mod devices;
pub mod entries;
mod logs;
mod query;
pub mod reading;
mod resumable;
mod revision;
mod search;
mod settings;
mod shares;
mod targets;
mod trash;
mod tree;
mod upload;

pub const ROOT_ID: i64 = 1;

/// 仅标记已核实的收件入口，缺少记录的历史节点仍属于来源未知。
#[derive(Clone, Copy)]
pub enum InboxSource {
    Bot,
    Channel,
}

impl InboxSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Channel => "channel",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    /// ready | uploading | trashed;常规查询均按 ready 过滤
    #[allow(dead_code)]
    pub status: String,
    pub created_at: i64,
    pub modified_at: i64,
    /// 文件内容 sha256(hex);历史文件与目录为 None
    pub sha256: Option<String>,
    /// 是否已收藏；目录与文件都可以收藏。
    pub favorite: bool,
}

impl Node {
    /// 文件移动或改名不改变版本，覆盖写的新节点以及不同内容必须产生新版本。
    /// 与应用 RPC 共用同一算法；目录版本不代表子树修订。
    pub fn content_version(&self) -> String {
        use sha2::{Digest, Sha256};
        let source = format!(
            "{}:{}:{}:{}",
            self.id,
            self.is_dir,
            self.size,
            self.sha256
                .clone()
                .unwrap_or_else(|| self.created_at.to_string())
        );
        crate::engine::upload::to_hex(&Sha256::digest(source.as_bytes()))
    }
}

/// 分块行;部分字段仅由 SQL 侧使用,保留以完整映射
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct Chunk {
    pub id: i64,
    pub node_id: i64,
    pub idx: i64,
    pub size: i64,
    pub tg_file_id: String,
    pub tg_message_id: i64,
    /// 创建该分块时使用的 Bot、端点与频道；旧库首次迁移时可能暂为空。
    pub storage_target_id: Option<i64>,
    /// Telegram 在同一媒体消息中返回的缩略图；只在第 0 块保存。
    pub tg_thumbnail_file_id: Option<String>,
}

/// 文件或目录属性中的递归逻辑用量；不读取 Telegram 内容。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeUsage {
    pub file_count: i64,
    pub dir_count: i64,
    pub total_size: i64,
    pub chunk_count: i64,
}

/// 一个可长期读取历史分块的 Telegram 存储目标。
#[derive(Clone, sqlx::FromRow)]
pub struct StorageTarget {
    pub id: i64,
    pub api_base: String,
    pub bot_token: String,
    pub chat_id: String,
    #[allow(dead_code)]
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct StorageMigrationSource {
    pub storage_target_id: i64,
    pub bot_id: String,
    pub chat_id: String,
    pub messages: i64,
    pub chunks: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct StorageMigrationJob {
    pub main_target_id: i64,
    pub status: String,
    pub total_messages: i64,
    pub migrated_messages: i64,
    pub total_chunks: i64,
    pub migrated_chunks: i64,
    pub last_error: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StorageMigrationCandidate {
    pub source_target_id: i64,
    pub source_chat_id: String,
    pub source_message_id: i64,
}

/// Telegram 消息的完整身份；message_id 只在同一存储目标内有意义。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct StoredMessage {
    pub storage_target_id: i64,
    pub message_id: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeleteJob {
    pub id: i64,
    pub storage_target_id: i64,
    pub tg_message_id: i64,
    pub attempts: i64,
}

/// 面向管理员展示的关键后台事件摘要。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct BackendLog {
    pub id: i64,
    pub level: String,
    pub category: String,
    pub message: String,
    pub created_at: i64,
}

/// 后台日志筛选条件；字段为 `None` 或空串表示该维度不限制。
#[derive(Debug, Clone, Default)]
pub struct BackendLogFilter {
    /// debug | info | warn | error；其余取值按“不限制”处理
    pub level: Option<String>,
    pub category: Option<String>,
    /// 在类别与事件内容里做不区分大小写的子串匹配
    pub keyword: Option<String>,
}

/// 供管理页筛选下拉用的日志切面：全库出现过的类别，以及各级别条数。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackendLogFacets {
    pub categories: Vec<String>,
    pub level_counts: HashMap<String, i64>,
}

/// 后台队列概况：待处理量与卡住的原因，供管理页「后台任务」分区展示。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QueueOverview {
    pub pending: i64,
    /// 已经失败重试过（attempts > 0）的任务数
    pub retrying: i64,
    /// 队列里重试次数最多的那条试了几次
    pub max_attempts: i64,
    /// 重试次数最多那条的错误原因，通常就是卡住整个队列的根因
    pub last_error: Option<String>,
}

const BACKEND_LOG_KEEP: i64 = 2_000;
/// 删除消息前的宽限期：正在播放或下载的流还能读完已打开的分块。
/// 后台「立即执行」无视该延迟。测试里为零，保持删除立即可见。
#[cfg(not(test))]
const DELETE_GRACE_SECS: i64 = 5 * 60;
#[cfg(test)]
const DELETE_GRACE_SECS: i64 = 0;

/// 分享链接行
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Share {
    pub id: i64,
    pub token: String,
    pub node_id: i64,
    /// 提取码;序列化到管理接口没问题(仅登录用户可见),公开接口不返回本结构
    pub password: Option<String>,
    pub expires_at: Option<i64>,
    pub max_downloads: Option<i64>,
    pub downloads: i64,
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct SearchRow {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    is_dir: bool,
    size: i64,
    status: String,
    created_at: i64,
    modified_at: i64,
    sha256: Option<String>,
    favorite: bool,
    path: String,
}

/// 高级搜索条件。`None` 表示不限制，时间均为 Unix 秒。
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub query: Option<String>,
    /// `Some(true)` 只查目录，`Some(false)` 只查文件。
    pub is_dir: Option<bool>,
    /// 不带前导点的小写后缀；支持 `tar.gz` 这样的多段后缀。
    pub extensions: Vec<String>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub favorite_only: bool,
    /// 只在该目录子树内搜索，形如 `/影视/短片`；`None` 表示整库。
    pub under: Option<String>,
    pub sort: SearchSort,
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum SearchSort {
    #[default]
    Name,
    Size,
    Modified,
    Created,
}

/// 一个可跨浏览器刷新和服务重启继续的上传会话。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UploadSession {
    pub id: String,
    pub node_id: i64,
    pub display_path: String,
    pub expected_size: i64,
    pub sha256: String,
    pub chunk_size: i64,
    pub storage_target_id: i64,
    pub overwrite: bool,
    pub thumbnail: Option<Vec<u8>>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct UploadSessionInfo {
    pub session: UploadSession,
    pub node: Node,
    pub uploaded_parts: Vec<i64>,
    pub uploaded_bytes: i64,
    pub in_flight_parts: Vec<i64>,
    pub uncertain_parts: Vec<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UploadReceipt {
    pub id: String,
    pub status: String,
    pub display_path: String,
    pub expected_size: i64,
    pub sha256: String,
    #[serde(skip)]
    pub node_json: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

impl UploadSessionInfo {
    pub fn part_count(&self) -> i64 {
        if self.session.expected_size == 0 {
            0
        } else {
            self.session.expected_size / self.session.chunk_size
                + i64::from(self.session.expected_size % self.session.chunk_size != 0)
        }
    }

    pub fn expected_part_size(&self, idx: i64) -> Option<i64> {
        if idx < 0 || idx >= self.part_count() {
            return None;
        }
        let start = idx.checked_mul(self.session.chunk_size)?;
        Some(
            self.session
                .chunk_size
                .min(self.session.expected_size - start),
        )
    }
}

#[derive(Debug, Clone)]
pub enum UploadPartClaim {
    Complete,
    Claimed(Box<UploadSessionInfo>),
}

/// URL 安全随机 token
fn random_token(len: usize) -> String {
    use rand::RngExt as _;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    /// 已安装应用记录的进程内缓存；`apps` 表的每条写入路径都会使其失效（见 apps/db.rs）。
    apps_cache: std::sync::Arc<apps::AppCache>,
    /// 应用封面缓存库（主库旁的独立文件，见 apps/covers_db.rs）。
    covers: SqlitePool,
    /// 恢复安全模式/维护状态，随所有 Db 克隆共享；后台入口必须检查。
    background_blocked: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// 有待执行迁移的既有库先做一份一致性快照：部分迁移会删表重建，无法回退。
/// 全新库（尚无迁移记录）无需快照。
async fn snapshot_before_migration(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
    db_path: &str,
) -> anyhow::Result<()> {
    let has_history: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;
    if has_history.is_none() {
        return Ok(());
    }
    let applied: Vec<(i64,)> = sqlx::query_as("SELECT version FROM _sqlx_migrations")
        .fetch_all(pool)
        .await?;
    let pending = migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .any(|m| !applied.iter().any(|(v,)| *v == m.version));
    if !pending {
        return Ok(());
    }
    let latest = applied.iter().map(|(v,)| *v).max().unwrap_or(0);
    let dest = format!("{db_path}.pre-migrate-{latest}-{}.bak", now_ts());
    tracing::info!("数据库有待执行的迁移，先备份到 {dest}");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "VACUUM INTO '{}'",
        dest.replace('\'', "''")
    )))
    .execute(pool)
    .await?;
    Ok(())
}

impl Db {
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        if let Some(dir) = std::path::Path::new(db_path).parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // WAL 下 NORMAL 已足够安全：应用/容器崩溃不丢已提交事务，仅断电或系统
            // 崩溃可能丢失最后几笔提交；换来分块元数据等高频写入免每笔 fsync 的吞吐。
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(10))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let migrator = sqlx::migrate!("./migrations");
        let initialized = async {
            snapshot_before_migration(&pool, &migrator, db_path).await?;
            migrator.run(&pool).await?;
            apps::open_covers(db_path).await
        }
        .await;
        let covers = match initialized {
            Ok(covers) => covers,
            Err(error) => {
                // 恢复验证失败必须等主库连接全部关闭，才能离线回滚文件。
                pool.close().await;
                return Err(error);
            }
        };
        Ok(Db {
            pool,
            apps_cache: Default::default(),
            covers,
            background_blocked: Default::default(),
        })
    }

    pub fn background_blocked(&self) -> bool {
        self.background_blocked
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_background_blocked(&self, blocked: bool) {
        self.background_blocked
            .store(blocked, std::sync::atomic::Ordering::Release);
    }

    pub fn require_background_enabled(&self) -> AppResult<()> {
        if self.background_blocked() {
            return Err(AppError::Forbidden(
                "恢复安全模式或维护中，后台任务已暂停".into(),
            ));
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
        self.covers.close().await;
    }

    pub async fn ping(&self) -> AppResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
