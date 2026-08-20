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

pub const ROOT_ID: i64 = 1;

#[derive(Debug, Clone, sqlx::FromRow)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CaptionSyncJob {
    pub id: i64,
    pub storage_target_id: i64,
    pub tg_message_id: i64,
    pub node_id: i64,
    pub chunk_idx: i64,
    pub attempts: i64,
    pub generation: i64,
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

const BACKEND_LOG_KEEP: i64 = 2_000;

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
            .busy_timeout(std::time::Duration::from_secs(10))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Db { pool })
    }

    pub async fn ping(&self) -> AppResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // ---------- 存储目标 ----------

    /// 取得或创建一个不可变的存储目标。敏感字段只留在服务端，不会直接序列化到 API。
    pub async fn ensure_storage_target(
        &self,
        api_base: &str,
        bot_token: &str,
        chat_id: &str,
    ) -> AppResult<StorageTarget> {
        sqlx::query(
            "INSERT INTO storage_targets (api_base, bot_token, chat_id, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(api_base, bot_token, chat_id) DO NOTHING",
        )
        .bind(api_base.trim_end_matches('/'))
        .bind(bot_token)
        .bind(chat_id)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        sqlx::query_as::<_, StorageTarget>(
            "SELECT * FROM storage_targets
             WHERE api_base = ? AND bot_token = ? AND chat_id = ?",
        )
        .bind(api_base.trim_end_matches('/'))
        .bind(bot_token)
        .bind(chat_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn storage_targets(&self) -> AppResult<Vec<StorageTarget>> {
        Ok(sqlx::query_as("SELECT * FROM storage_targets ORDER BY id")
            .fetch_all(&self.pool)
            .await?)
    }

    /// v3 及更早版本的分块没有存储目标。迁移时只能采用当前生效配置，且只执行一次。
    pub async fn backfill_unassigned_chunks(&self, storage_target_id: i64) -> AppResult<u64> {
        Ok(
            sqlx::query("UPDATE chunks SET storage_target_id = ? WHERE storage_target_id IS NULL")
                .bind(storage_target_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub async fn unassigned_chunk_count(&self) -> AppResult<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM chunks WHERE storage_target_id IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    // ---------- 查询 ----------

    pub async fn node(&self, id: i64) -> AppResult<Node> {
        sqlx::query_as::<_, Node>("SELECT * FROM nodes WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    /// 按名字在目录下查可见子节点
    pub async fn child(&self, parent_id: i64, name: &str) -> AppResult<Option<Node>> {
        Ok(sqlx::query_as::<_, Node>(
            "SELECT * FROM nodes WHERE parent_id = ? AND name = ? AND status = 'ready'",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 同名文件是否仍在后台上传，用于客户端在响应丢失后等待最终结果。
    pub async fn uploading_child_exists(&self, parent_id: i64, name: &str) -> AppResult<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'uploading' LIMIT 1",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// 从根按路径段解析;段必须已通过 vfs 规范化
    pub async fn resolve(&self, segments: &[&str]) -> AppResult<Option<Node>> {
        let mut cur = self.node(ROOT_ID).await?;
        for seg in segments {
            match self.child(cur.id, seg).await? {
                Some(n) => cur = n,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    pub async fn list_children(&self, parent_id: i64) -> AppResult<Vec<Node>> {
        Ok(sqlx::query_as::<_, Node>(
            "SELECT * FROM nodes WHERE parent_id = ? AND status = 'ready'
             ORDER BY is_dir DESC, name COLLATE NOCASE",
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn chunks_of(&self, node_id: i64) -> AppResult<Vec<Chunk>> {
        Ok(
            sqlx::query_as::<_, Chunk>("SELECT * FROM chunks WHERE node_id = ? ORDER BY idx")
                .bind(node_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn set_favorite(&self, node_id: i64, favorite: bool) -> AppResult<Node> {
        let changed =
            sqlx::query("UPDATE nodes SET favorite = ? WHERE id = ? AND status = 'ready'")
                .bind(favorite)
                .bind(node_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        if changed == 0 {
            return Err(AppError::NotFound);
        }
        self.node(node_id).await
    }

    /// 全局统计:文件数、目录数、逻辑总大小
    pub async fn stats(&self) -> AppResult<(i64, i64, i64)> {
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT
               COUNT(*) FILTER (WHERE is_dir = 0),
               COUNT(*) FILTER (WHERE is_dir = 1) - 1,
               COALESCE(SUM(size) FILTER (WHERE is_dir = 0), 0)
             FROM nodes WHERE status = 'ready'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// 统计一个可见节点。目录递归计算，目录本身不计入 dir_count。
    pub async fn node_usage(&self, node_id: i64) -> AppResult<NodeUsage> {
        sqlx::query_as::<_, NodeUsage>(
            "WITH RECURSIVE sub(id, is_dir, size) AS (
                 SELECT id, is_dir, size FROM nodes
                 WHERE id = ? AND status = 'ready'
                 UNION ALL
                 SELECT n.id, n.is_dir, n.size
                 FROM nodes n JOIN sub s ON n.parent_id = s.id
                 WHERE n.status = 'ready'
             )
             SELECT
                 COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0) AS file_count,
                 COALESCE(SUM(CASE WHEN is_dir = 1 AND id != ? THEN 1 ELSE 0 END), 0)
                     AS dir_count,
                 COALESCE(SUM(CASE WHEN is_dir = 0 THEN size ELSE 0 END), 0) AS total_size,
                 (SELECT COUNT(*) FROM chunks WHERE node_id IN (SELECT id FROM sub))
                     AS chunk_count
             FROM sub",
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    // ---------- 目录/重命名 ----------

    pub async fn mkdir(&self, parent_id: i64, name: &str) -> AppResult<Node> {
        let parent = self.node(parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if self.child(parent_id, name).await?.is_some() {
            return Err(AppError::AlreadyExists);
        }
        let ts = now_ts();
        let id = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at)
             VALUES (?, ?, 1, 0, 'ready', ?, ?)",
        )
        .bind(parent_id)
        .bind(name)
        .bind(ts)
        .bind(ts)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        self.node(id).await
    }

    /// 检查 ancestor 是否为 node 的祖先(含相等),防止把目录移进自己子树
    pub async fn is_ancestor(&self, ancestor: i64, node: i64) -> AppResult<bool> {
        let mut cur = Some(node);
        while let Some(id) = cur {
            if id == ancestor {
                return Ok(true);
            }
            cur = self.node(id).await?.parent_id;
        }
        Ok(false)
    }

    /// 移动/重命名(同为改 parent_id+name)
    pub async fn move_node(&self, id: i64, new_parent: i64, new_name: &str) -> AppResult<u64> {
        let node = self.node(id).await?;
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能移动根目录".into()));
        }
        let parent = self.node(new_parent).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if node.is_dir && self.is_ancestor(id, new_parent).await? {
            return Err(AppError::BadRequest("不能把目录移动到自己内部".into()));
        }
        if let Some(existing) = self.child(new_parent, new_name).await?
            && existing.id != id
        {
            return Err(AppError::AlreadyExists);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, modified_at = ? WHERE id = ?")
            .bind(new_parent)
            .bind(new_name)
            .bind(now_ts())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let queued = Self::enqueue_caption_sync_subtree_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(queued)
    }

    /// 为文件或目录树内的独占 Telegram 消息登记 caption 路径同步任务。
    /// 同一消息被多个虚拟文件共享时无法表达唯一当前路径，因此安全地跳过。
    async fn enqueue_caption_sync_subtree_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        node_id: i64,
    ) -> AppResult<u64> {
        Ok(sqlx::query(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM nodes WHERE id = ?
                 UNION ALL
                 SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             INSERT INTO caption_sync_jobs
                 (storage_target_id, tg_message_id, node_id, chunk_idx, attempts,
                  next_retry_at, last_error, generation, created_at)
             SELECT c.storage_target_id, c.tg_message_id, c.node_id, c.idx,
                    0, 0, NULL, 1, ?
             FROM chunks c JOIN sub s ON s.id = c.node_id
             WHERE c.storage_target_id IS NOT NULL
               AND (
                   SELECT COUNT(*) FROM chunks refs
                   WHERE refs.storage_target_id = c.storage_target_id
                     AND refs.tg_message_id = c.tg_message_id
               ) = 1
             ON CONFLICT(storage_target_id, tg_message_id) DO UPDATE SET
                 node_id = excluded.node_id,
                 chunk_idx = excluded.chunk_idx,
                 attempts = 0,
                 next_retry_at = 0,
                 last_error = NULL,
                 generation = caption_sync_jobs.generation + 1,
                 created_at = excluded.created_at",
        )
        .bind(node_id)
        .bind(now_ts())
        .execute(&mut **tx)
        .await?
        .rows_affected())
    }

    // ---------- 后台事件日志 ----------

    /// 写入一条面向管理员的后台事件摘要。
    /// 日志写入失败不应影响主业务，调用方通常只记录 tracing 警告后继续。
    pub async fn append_backend_log(
        &self,
        level: &str,
        category: &str,
        message: &str,
    ) -> AppResult<()> {
        let level = match level {
            "debug" | "info" | "warn" | "error" => level,
            _ => "info",
        };
        let category: String = category.chars().take(80).collect();
        let message: String = message.chars().take(1_000).collect();
        let category = if category.trim().is_empty() {
            "系统"
        } else {
            category.trim()
        };
        let message = if message.trim().is_empty() {
            "后台事件"
        } else {
            message.trim()
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO backend_logs (level, category, message, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(level)
        .bind(category)
        .bind(message)
        .bind(now_ts())
        .execute(&mut *tx)
        .await?;
        // 按自增 id 保留最近固定数量，避免高频后台任务让数据库无限增长。
        sqlx::query(
            "DELETE FROM backend_logs
             WHERE id <= (SELECT COALESCE(MAX(id), 0) - ? FROM backend_logs)",
        )
        .bind(BACKEND_LOG_KEEP)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 按 id 倒序读取后台日志；`before` 用于向前翻页。
    pub async fn backend_logs(
        &self,
        limit: i64,
        before: Option<i64>,
    ) -> AppResult<(Vec<BackendLog>, bool)> {
        let limit = limit.clamp(1, 100);
        let fetch_limit = limit + 1;
        let mut rows = if let Some(before) = before {
            sqlx::query_as::<_, BackendLog>(
                "SELECT id, level, category, message, created_at
                 FROM backend_logs
                 WHERE id < ?
                 ORDER BY id DESC
                 LIMIT ?",
            )
            .bind(before)
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, BackendLog>(
                "SELECT id, level, category, message, created_at
                 FROM backend_logs
                 ORDER BY id DESC
                 LIMIT ?",
            )
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await?
        };
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok((rows, has_more))
    }

    // ---------- 上传生命周期 ----------

    /// 创建 uploading 状态的文件占位节点(允许与同名 ready 文件并存,finalize 时替换)
    pub async fn create_uploading(&self, parent_id: i64, name: &str) -> AppResult<i64> {
        let parent = self.node(parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if let Some(existing) = self.child(parent_id, name).await?
            && existing.is_dir
        {
            return Err(AppError::IsDirectory); // 不允许文件覆盖目录
        }
        let ts = now_ts();
        // 条件插入在 SQLite 的单条写语句内完成，防止两个请求同时为同一路径建上传影子。
        let inserted = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at)
             SELECT ?, ?, 0, 0, 'uploading', ?, ?
             WHERE NOT EXISTS (
                 SELECT 1 FROM nodes
                 WHERE parent_id = ? AND name = ? AND status = 'uploading'
             )",
        )
        .bind(parent_id)
        .bind(name)
        .bind(ts)
        .bind(ts)
        .bind(parent_id)
        .bind(name)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(AppError::AlreadyExists);
        }
        Ok(inserted.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_chunk(
        &self,
        node_id: i64,
        idx: i64,
        size: i64,
        tg_file_id: &str,
        tg_message_id: i64,
        storage_target_id: i64,
        tg_thumbnail_file_id: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(node_id)
        .bind(idx)
        .bind(size)
        .bind(tg_file_id)
        .bind(tg_message_id)
        .bind(storage_target_id)
        .bind(tg_thumbnail_file_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 完成上传:置 ready 并原子替换同名旧文件;返回旧文件释放的 message_id(需在 TG 侧删除)
    pub async fn finalize_upload(
        &self,
        node_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> AppResult<Vec<StoredMessage>> {
        let (_, freed) = self
            .finalize_upload_with_node(node_id, size, sha256)
            .await?;
        Ok(freed)
    }

    /// 完成上传并在同一事务内返回最终节点，避免提交后再次查询造成“已成功却报失败”。
    pub async fn finalize_upload_with_node(
        &self,
        node_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> AppResult<(Node, Vec<StoredMessage>)> {
        let node = self.node(node_id).await?;
        let parent_id = node.parent_id.ok_or(AppError::NotFound)?;

        let mut tx = self.pool.begin().await?;
        // 旧同名 ready 文件(覆盖写场景)
        let old: Option<(i64, bool)> = sqlx::query_as(
            "SELECT id, favorite FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'ready' AND id != ?",
        )
        .bind(parent_id)
        .bind(&node.name)
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;

        let mut candidates: Vec<StoredMessage> = Vec::new();
        let inherited_favorite = old.map(|(_, favorite)| favorite).unwrap_or(false);
        if let Some((old_id, _)) = old {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT storage_target_id, tg_message_id FROM chunks
                 WHERE node_id = ? AND storage_target_id IS NOT NULL",
            )
            .bind(old_id)
            .fetch_all(&mut *tx)
            .await?;
            candidates = rows
                .into_iter()
                .map(|(storage_target_id, message_id)| StoredMessage {
                    storage_target_id,
                    message_id,
                })
                .collect();
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
        // 上传会话只在 uploading 阶段存在；和最终节点切换在同一事务内完成。
        sqlx::query("DELETE FROM upload_sessions WHERE node_id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE nodes
             SET status = 'ready', size = ?, sha256 = ?, modified_at = ?,
                 favorite = CASE WHEN ? THEN 1 ELSE favorite END
             WHERE id = ?",
        )
        .bind(size)
        .bind(sha256)
        .bind(now_ts())
        .bind(inherited_favorite)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        let ready = sqlx::query_as::<_, Node>("SELECT * FROM nodes WHERE id = ?")
            .bind(node_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((ready, freed))
    }

    /// 放弃上传(失败/断开/启动清理):删节点行,返回已上传且未被共享的 message_id
    pub async fn abort_upload(&self, node_id: i64) -> AppResult<Vec<StoredMessage>> {
        self.abort_upload_inner(node_id, true).await
    }

    /// 收件箱登记的是用户原始频道消息；登记失败只能回滚元数据，不能删除原消息。
    pub async fn abort_upload_keep_messages(&self, node_id: i64) -> AppResult<()> {
        self.abort_upload_inner(node_id, false).await.map(|_| ())
    }

    async fn abort_upload_inner(
        &self,
        node_id: i64,
        enqueue_delete: bool,
    ) -> AppResult<Vec<StoredMessage>> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT storage_target_id, tg_message_id FROM chunks
             WHERE node_id = ? AND storage_target_id IS NOT NULL",
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|(storage_target_id, message_id)| StoredMessage {
                storage_target_id,
                message_id,
            })
            .collect();
        sqlx::query("DELETE FROM nodes WHERE id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        let freed = if enqueue_delete {
            Self::freed_messages(&mut tx, candidates).await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        Ok(freed)
    }

    /// 启动时清理旧上传管线的 uploading 残留；可恢复会话必须保留。
    pub async fn cleanup_stale_uploads(&self) -> AppResult<Vec<StoredMessage>> {
        let ids: Vec<(i64,)> = sqlx::query_as(
            "SELECT n.id FROM nodes n
             LEFT JOIN upload_sessions s ON s.node_id = n.id
             WHERE n.status = 'uploading' AND s.id IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            freed.extend(self.abort_upload(id).await?);
        }
        Ok(freed)
    }

    // ---------- 可恢复上传 ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn create_upload_session(
        &self,
        parent_id: i64,
        name: &str,
        display_path: &str,
        expected_size: i64,
        sha256: &str,
        chunk_size: i64,
        storage_target_id: i64,
    ) -> AppResult<UploadSessionInfo> {
        if expected_size < 0 || chunk_size <= 0 {
            return Err(AppError::BadRequest("上传大小或分块大小无效".into()));
        }
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::BadRequest("SHA-256 格式无效".into()));
        }

        let existing: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT s.id, s.expected_size, s.sha256
             FROM upload_sessions s JOIN nodes n ON n.id = s.node_id
             WHERE n.parent_id = ? AND n.name = ? AND n.status = 'uploading'",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((id, size, existing_sha)) = existing {
            if size == expected_size && existing_sha.eq_ignore_ascii_case(sha256) {
                return self.upload_session_info(&id).await;
            }
            return Err(AppError::BadRequest(
                "同一路径已有另一个未完成上传，请先在上传队列中取消它".into(),
            ));
        }

        let node_id = self.create_uploading(parent_id, name).await?;
        let id = random_token(32);
        let ts = now_ts();
        let inserted = sqlx::query(
            "INSERT INTO upload_sessions
                (id, node_id, display_path, expected_size, sha256, chunk_size,
                 storage_target_id, created_at, updated_at)
             VALUES (?, ?, ?, ?, lower(?), ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(node_id)
        .bind(display_path)
        .bind(expected_size)
        .bind(sha256)
        .bind(chunk_size)
        .bind(storage_target_id)
        .bind(ts)
        .bind(ts)
        .execute(&self.pool)
        .await;
        if let Err(error) = inserted {
            let _ = self.abort_upload(node_id).await;
            return Err(error.into());
        }
        self.upload_session_info(&id).await
    }

    async fn upload_session_row(&self, id: &str) -> AppResult<UploadSession> {
        sqlx::query_as::<_, UploadSession>("SELECT * FROM upload_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub async fn upload_session_info(&self, id: &str) -> AppResult<UploadSessionInfo> {
        let session = self.upload_session_row(id).await?;
        let node = self.node(session.node_id).await?;
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT idx, size FROM chunks WHERE node_id = ? ORDER BY idx")
                .bind(session.node_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(UploadSessionInfo {
            session,
            node,
            uploaded_parts: rows.iter().map(|(idx, _)| *idx).collect(),
            uploaded_bytes: rows.iter().map(|(_, size)| *size).sum(),
        })
    }

    pub async fn upload_sessions(&self) -> AppResult<Vec<UploadSessionInfo>> {
        let ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM upload_sessions ORDER BY updated_at, id")
                .fetch_all(&self.pool)
                .await?;
        let mut out = Vec::with_capacity(ids.len());
        for (id,) in ids {
            out.push(self.upload_session_info(&id).await?);
        }
        Ok(out)
    }

    pub async fn set_upload_thumbnail(&self, id: &str, thumbnail: &[u8]) -> AppResult<()> {
        let changed = sqlx::query(
            "UPDATE upload_sessions
             SET thumbnail = ?, updated_at = ?
             WHERE id = ? AND NOT EXISTS (
                 SELECT 1 FROM chunks c
                 WHERE c.node_id = upload_sessions.node_id AND c.idx = 0
             )",
        )
        .bind(thumbnail)
        .bind(now_ts())
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            // 已经完成第 0 块时上传缩略图是幂等无害的；不存在的会话才报错。
            self.upload_session_row(id).await?;
        }
        Ok(())
    }

    pub async fn claim_upload_part(&self, id: &str, idx: i64) -> AppResult<UploadPartClaim> {
        let info = self.upload_session_info(id).await?;
        if info.expected_part_size(idx).is_none() {
            return Err(AppError::BadRequest("分块下标超出范围".into()));
        }
        if info.uploaded_parts.contains(&idx) {
            return Ok(UploadPartClaim::Complete);
        }
        let inserted = sqlx::query(
            "INSERT INTO upload_part_claims(session_id, idx, started_at)
             VALUES (?, ?, ?) ON CONFLICT(session_id, idx) DO NOTHING",
        )
        .bind(id)
        .bind(idx)
        .bind(now_ts())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(AppError::BadRequest("该分块正在上传，请稍后重试".into()));
        }
        sqlx::query("UPDATE upload_sessions SET updated_at = ? WHERE id = ?")
            .bind(now_ts())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(UploadPartClaim::Claimed(Box::new(info)))
    }

    pub async fn release_upload_part(&self, id: &str, idx: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_part_claims WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn finish_upload_part(
        &self,
        id: &str,
        idx: i64,
        size: i64,
        tg_file_id: &str,
        tg_message_id: i64,
        storage_target_id: i64,
        tg_thumbnail_file_id: Option<&str>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        let session: UploadSession = sqlx::query_as("SELECT * FROM upload_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let part_count = if session.expected_size == 0 {
            0
        } else {
            session.expected_size / session.chunk_size
                + i64::from(session.expected_size % session.chunk_size != 0)
        };
        if idx < 0 || idx >= part_count {
            return Err(AppError::BadRequest("分块下标超出范围".into()));
        }
        let expected = session
            .chunk_size
            .min(session.expected_size - idx * session.chunk_size);
        if size != expected || storage_target_id != session.storage_target_id {
            return Err(AppError::BadRequest("分块大小或存储目标不匹配".into()));
        }
        let claimed: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM upload_part_claims WHERE session_id = ? AND idx = ?")
                .bind(id)
                .bind(idx)
                .fetch_optional(&mut *tx)
                .await?;
        if claimed.is_none() {
            return Err(AppError::BadRequest("分块上传声明已失效".into()));
        }
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session.node_id)
        .bind(idx)
        .bind(size)
        .bind(tg_file_id)
        .bind(tg_message_id)
        .bind(storage_target_id)
        .bind(tg_thumbnail_file_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM upload_part_claims WHERE session_id = ? AND idx = ?")
            .bind(id)
            .bind(idx)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE upload_sessions
             SET updated_at = ?, thumbnail = CASE WHEN ? = 0 THEN NULL ELSE thumbnail END
             WHERE id = ?",
        )
        .bind(now_ts())
        .bind(idx)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn complete_upload_session(&self, id: &str) -> AppResult<Node> {
        let info = self.upload_session_info(id).await?;
        let expected: Vec<i64> = (0..info.part_count()).collect();
        if info.uploaded_parts != expected || info.uploaded_bytes != info.session.expected_size {
            return Err(AppError::BadRequest("仍有分块尚未上传完成".into()));
        }
        let (claims,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM upload_part_claims WHERE session_id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        if claims != 0 {
            return Err(AppError::BadRequest("仍有分块正在上传".into()));
        }
        self.finalize_upload_with_node(
            info.session.node_id,
            info.session.expected_size,
            Some(&info.session.sha256),
        )
        .await
        .map(|(node, _)| node)
    }

    pub async fn cancel_upload_session(&self, id: &str) -> AppResult<Vec<StoredMessage>> {
        let session = self.upload_session_row(id).await?;
        self.abort_upload(session.node_id).await
    }

    pub async fn reset_upload_part_claims(&self) -> AppResult<()> {
        sqlx::query("DELETE FROM upload_part_claims")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_expired_upload_sessions(
        &self,
        cutoff: i64,
    ) -> AppResult<Vec<StoredMessage>> {
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM upload_sessions WHERE updated_at < ? ORDER BY updated_at",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            match self.cancel_upload_session(&id).await {
                Ok(messages) => freed.extend(messages),
                Err(AppError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(freed)
    }

    // ---------- 删除/复制 ----------

    /// 递归删除节点;返回引用归零、需要在 TG 删除的 message_id
    pub async fn delete_node(&self, id: i64) -> AppResult<Vec<StoredMessage>> {
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能删除根目录".into()));
        }
        let mut tx = self.pool.begin().await?;
        // 收集整棵子树的节点 id(SQLite 递归 CTE)
        let node_ids: Vec<(i64,)> = sqlx::query_as(
            "WITH RECURSIVE sub(id) AS (
               SELECT id FROM nodes WHERE id = ?
               UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             ) SELECT id FROM sub",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;

        let mut candidates: Vec<StoredMessage> = Vec::new();
        for (nid,) in &node_ids {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT storage_target_id, tg_message_id FROM chunks
                 WHERE node_id = ? AND storage_target_id IS NOT NULL",
            )
            .bind(nid)
            .fetch_all(&mut *tx)
            .await?;
            candidates.extend(rows.into_iter().map(|(storage_target_id, message_id)| {
                StoredMessage {
                    storage_target_id,
                    message_id,
                }
            }));
        }
        // 子节点行由 ON DELETE CASCADE 级联;显式删整棵树避免依赖外键深度
        for (nid,) in node_ids.iter().rev() {
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(nid)
                .execute(&mut *tx)
                .await?;
        }
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(freed)
    }

    /// 复制文件:仅复制元数据行,共享 Telegram 消息(零流量)
    pub async fn copy_file(&self, src_id: i64, dst_parent: i64, dst_name: &str) -> AppResult<i64> {
        let src = self.node(src_id).await?;
        if src.is_dir {
            return Err(AppError::IsDirectory);
        }
        if self.child(dst_parent, dst_name).await?.is_some() {
            return Err(AppError::AlreadyExists);
        }
        let ts = now_ts();
        let mut tx = self.pool.begin().await?;
        let new_id = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at, sha256)
             VALUES (?, ?, 0, ?, 'ready', ?, ?, ?)",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .bind(src.size)
        .bind(ts)
        .bind(ts)
        .bind(&src.sha256)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             SELECT ?, idx, size, tg_file_id, tg_message_id, storage_target_id,
                    tg_thumbnail_file_id
             FROM chunks WHERE node_id = ?",
        )
        .bind(new_id)
        .bind(src_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(new_id)
    }

    // ---------- 回收站 ----------

    /// 软删除:整棵子树置 trashed(保持 ready ⇒ 可见的不变式),该子树的分享一并删除
    pub async fn trash_node(&self, id: i64, orig_path: &str) -> AppResult<()> {
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能删除根目录".into()));
        }
        let node = self.node(id).await?;
        let parent_id = node.parent_id.ok_or(AppError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE nodes SET status = 'trashed' WHERE id IN (SELECT id FROM sub) AND status = 'ready'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             DELETE FROM shares WHERE node_id IN (SELECT id FROM sub)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO trash (node_id, orig_parent_id, orig_path, trashed_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(parent_id)
        .bind(orig_path)
        .bind(now_ts())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 回收站列表:(子树根节点, 原路径, 删除时间)
    pub async fn trash_list(&self) -> AppResult<Vec<(Node, String, i64)>> {
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT node_id, orig_path, trashed_at FROM trash ORDER BY trashed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (nid, path, at) in rows {
            out.push((self.node(nid).await?, path, at));
        }
        Ok(out)
    }

    /// 恢复:优先回原目录(仍可见时),否则回根目录;同名冲突自动改名。返回恢复后的完整路径
    pub async fn trash_restore(&self, node_id: i64) -> AppResult<String> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT orig_parent_id FROM trash WHERE node_id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await?;
        let (orig_parent,) = row.ok_or(AppError::NotFound)?;
        let node = self.node(node_id).await?;

        let parent_visible = matches!(
            self.node(orig_parent).await,
            Ok(p) if p.is_dir && (p.id == ROOT_ID || self.full_path_if_visible(p.id).await?.is_some())
        );
        let target_parent = if parent_visible { orig_parent } else { ROOT_ID };

        // 同名冲突 → "name (恢复)"、"name (恢复 2)" ...
        let mut name = node.name.clone();
        let mut k = 0;
        while self.child(target_parent, &name).await?.is_some() {
            k += 1;
            name = if k == 1 {
                format!("{} (恢复)", node.name)
            } else {
                format!("{} (恢复 {k})", node.name)
            };
        }

        let mut tx = self.pool.begin().await?;
        // 先改名/挪位置:此时根仍是 trashed,不受 ready 部分唯一索引约束;
        // 随后整树置 ready 时根已用新名字入索引,不会与占名者冲突
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, modified_at = ? WHERE id = ?")
            .bind(target_parent)
            .bind(&name)
            .bind(now_ts())
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "WITH RECURSIVE sub(id) AS (
               SELECT ? UNION ALL
               SELECT n.id FROM nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE nodes SET status = 'ready' WHERE id IN (SELECT id FROM sub) AND status = 'trashed'",
        )
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM trash WHERE node_id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(self
            .full_path_if_visible(node_id)
            .await?
            .unwrap_or_else(|| format!("/{name}")))
    }

    /// 彻底删除回收站中超期的项,返回引用归零的 message_id
    pub async fn trash_purge_expired(&self, retention_days: i64) -> AppResult<Vec<StoredMessage>> {
        let cutoff = now_ts() - retention_days * 86400;
        let ids: Vec<(i64,)> = sqlx::query_as("SELECT node_id FROM trash WHERE trashed_at <= ?")
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            freed.extend(self.delete_node(id).await?);
        }
        Ok(freed)
    }

    // ---------- 分享 ----------

    /// 节点的完整路径;自身或任一祖先不是 ready(回收站/上传中)时返回 None
    pub async fn full_path_if_visible(&self, id: i64) -> AppResult<Option<String>> {
        let n = self.node(id).await?;
        if n.id == ROOT_ID {
            return Ok(Some("/".into()));
        }
        if n.status != "ready" {
            return Ok(None);
        }
        let mut parts = vec![n.name.clone()];
        let mut cur = n.parent_id;
        while let Some(pid) = cur {
            let p = self.node(pid).await?;
            if p.id == ROOT_ID {
                break;
            }
            if p.status != "ready" {
                return Ok(None);
            }
            parts.push(p.name.clone());
            cur = p.parent_id;
        }
        parts.reverse();
        Ok(Some(format!("/{}", parts.join("/"))))
    }

    pub async fn share_create(
        &self,
        node_id: i64,
        password: Option<&str>,
        expires_at: Option<i64>,
        max_downloads: Option<i64>,
    ) -> AppResult<Share> {
        // 12 位随机 token,碰撞(极小概率)时重试
        for _ in 0..3 {
            let token = random_token(12);
            let r = sqlx::query(
                "INSERT INTO shares (token, node_id, password, expires_at, max_downloads, downloads, created_at)
                 VALUES (?, ?, ?, ?, ?, 0, ?)",
            )
            .bind(&token)
            .bind(node_id)
            .bind(password)
            .bind(expires_at)
            .bind(max_downloads)
            .bind(now_ts())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => {
                    return self
                        .share_by_token(&token)
                        .await?
                        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("分享刚创建即丢失")));
                }
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(AppError::Internal(anyhow::anyhow!("生成分享 token 失败")))
    }

    pub async fn share_by_token(&self, token: &str) -> AppResult<Option<Share>> {
        Ok(
            sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token = ?")
                .bind(token)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn shares_list(&self) -> AppResult<Vec<Share>> {
        Ok(
            sqlx::query_as::<_, Share>("SELECT * FROM shares ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn share_delete(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM shares WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 为一张短期票据原子领取某个文件的下载额度。
    ///
    /// 返回 `true` 表示可以下载。相同票据重复读取同一文件（包括 Range 请求）
    /// 不会再次计数；不同票据或目录分享中的不同文件仍会分别领取额度。
    pub async fn share_claim_download(
        &self,
        share_id: i64,
        ticket_id: &str,
        node_id: i64,
        ticket_expires_at: i64,
    ) -> AppResult<bool> {
        let now = now_ts();
        let mut tx = self.pool.begin().await?;

        // 顺手控制短期领取记录的体积；计数已经写入 shares，不随记录清理回退。
        sqlx::query("DELETE FROM share_download_claims WHERE expires_at < ?")
            .bind(now)
            .execute(&mut *tx)
            .await?;

        let inserted = sqlx::query(
            "INSERT INTO share_download_claims
                (share_id, ticket_id, node_id, expires_at, claimed_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(share_id, ticket_id, node_id) DO NOTHING",
        )
        .bind(share_id)
        .bind(ticket_id)
        .bind(node_id)
        .bind(ticket_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        if !inserted {
            tx.commit().await?;
            return Ok(true);
        }

        let claimed = sqlx::query(
            "UPDATE shares
             SET downloads = downloads + 1
             WHERE id = ?
               AND (max_downloads IS NULL OR downloads < max_downloads)",
        )
        .bind(share_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        if claimed {
            tx.commit().await?;
        } else {
            // 领取记录与计数必须同成同败，不能留下一个未计数却可复用的票据。
            tx.rollback().await?;
        }
        Ok(claimed)
    }

    // ---------- 搜索 / 秒传 ----------

    /// 兼容原有调用的按名称搜索。
    #[cfg(test)]
    pub async fn search(&self, query: &str, limit: usize) -> AppResult<Vec<(Node, String)>> {
        self.search_filtered(
            &SearchFilter {
                query: Some(query.to_string()),
                ..SearchFilter::default()
            },
            limit,
        )
        .await
    }

    /// 在 SQLite 内完成高级筛选、排序和路径拼接，排除回收站与上传中的子树。
    pub async fn search_filtered(
        &self,
        filter: &SearchFilter,
        limit: usize,
    ) -> AppResult<Vec<(Node, String)>> {
        // 只让 SQLite 遍历从 ready 根可达的节点并在库内拼路径，避免节点量大时
        // 每次搜索把整张 nodes 表及所有字符串复制进应用内存。
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "WITH RECURSIVE visible(
                id, parent_id, name, is_dir, size, status,
                created_at, modified_at, sha256, favorite, path
             ) AS (
                SELECT id, parent_id, name, is_dir, size, status,
                       created_at, modified_at, sha256, favorite, ''
                FROM nodes WHERE id = ",
        );
        query.push_bind(ROOT_ID).push(
            " AND status = 'ready'
                UNION ALL
                SELECT n.id, n.parent_id, n.name, n.is_dir, n.size, n.status,
                       n.created_at, n.modified_at, n.sha256, n.favorite,
                       visible.path || '/' || n.name
                FROM nodes n JOIN visible ON n.parent_id = visible.id
                WHERE n.status = 'ready'
             )
             SELECT id, parent_id, name, is_dir, size, status,
                    created_at, modified_at, sha256, favorite, path
             FROM visible
             WHERE id != ",
        );
        query.push_bind(ROOT_ID);
        if let Some(needle) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            query
                .push(" AND instr(lower(name), lower(")
                .push_bind(needle)
                .push(")) > 0");
        }
        if let Some(is_dir) = filter.is_dir {
            query.push(" AND is_dir = ").push_bind(is_dir);
        }
        if !filter.extensions.is_empty() {
            query.push(" AND is_dir = 0 AND (");
            for (index, extension) in filter.extensions.iter().enumerate() {
                if index > 0 {
                    query.push(" OR ");
                }
                query
                    .push("lower(name) GLOB ")
                    .push_bind(format!("*.{}", extension.to_ascii_lowercase()));
            }
            query.push(")");
        }
        if let Some(size) = filter.min_size {
            query.push(" AND is_dir = 0 AND size >= ").push_bind(size);
        }
        if let Some(size) = filter.max_size {
            query.push(" AND is_dir = 0 AND size <= ").push_bind(size);
        }
        if let Some(ts) = filter.modified_after {
            query.push(" AND modified_at >= ").push_bind(ts);
        }
        if let Some(ts) = filter.modified_before {
            query.push(" AND modified_at <= ").push_bind(ts);
        }
        if filter.favorite_only {
            query.push(" AND favorite = 1");
        }
        query.push(" ORDER BY ");
        match filter.sort {
            SearchSort::Name => query.push("name COLLATE NOCASE"),
            SearchSort::Size => query.push("size"),
            SearchSort::Modified => query.push("modified_at"),
            SearchSort::Created => query.push("created_at"),
        };
        query.push(if filter.descending { " DESC" } else { " ASC" });
        query
            .push(", id LIMIT ")
            .push_bind(limit.min(i64::MAX as usize) as i64);

        let rows: Vec<SearchRow> = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let path = row.path;
                (
                    Node {
                        id: row.id,
                        parent_id: row.parent_id,
                        name: row.name,
                        is_dir: row.is_dir,
                        size: row.size,
                        status: row.status,
                        created_at: row.created_at,
                        modified_at: row.modified_at,
                        sha256: row.sha256,
                        favorite: row.favorite,
                    },
                    path,
                )
            })
            .collect())
    }

    /// 秒传:找到内容相同(sha256+size)的既有文件则零流量链接到目标位置,
    /// 覆盖同名旧文件;返回 (新节点 id, 释放的 message_id)。找不到返回 None。
    pub async fn instant_link(
        &self,
        sha256: &str,
        size: i64,
        dst_parent: i64,
        dst_name: &str,
    ) -> AppResult<Option<(i64, Vec<StoredMessage>)>> {
        // 目标本身已经是同一内容时直接成功。除了让接口具备幂等性，也避免把源节点
        // 自己删掉后再从已删除节点复制 chunks，生成“有大小但无分块”的损坏文件。
        if let Some(existing) = self.child(dst_parent, dst_name).await? {
            if existing.is_dir {
                return Err(AppError::IsDirectory);
            }
            if existing.size == size && existing.sha256.as_deref() == Some(sha256) {
                return Ok(Some((existing.id, Vec::new())));
            }
        }
        // 源只需 chunks 有效:ready 即可(回收站子树整树置 trashed,ready ⇒ 可见)
        let src: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM nodes
             WHERE sha256 = ? AND size = ? AND is_dir = 0 AND status = 'ready' LIMIT 1",
        )
        .bind(sha256)
        .bind(size)
        .fetch_optional(&self.pool)
        .await?;
        let Some((src_id,)) = src else {
            return Ok(None);
        };

        if let Some(existing) = self.child(dst_parent, dst_name).await?
            && existing.is_dir
        {
            return Err(AppError::IsDirectory);
        }
        let ts = now_ts();
        let mut tx = self.pool.begin().await?;
        // 覆盖写:删同名旧文件并收集其消息做引用计数
        let old: Option<(i64, bool)> = sqlx::query_as(
            "SELECT id, favorite FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'ready'",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .fetch_optional(&mut *tx)
        .await?;
        let mut candidates = Vec::new();
        let inherited_favorite = old.map(|(_, favorite)| favorite).unwrap_or(false);
        if let Some((old_id, _)) = old {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT storage_target_id, tg_message_id FROM chunks
                 WHERE node_id = ? AND storage_target_id IS NOT NULL",
            )
            .bind(old_id)
            .fetch_all(&mut *tx)
            .await?;
            candidates = rows
                .into_iter()
                .map(|(storage_target_id, message_id)| StoredMessage {
                    storage_target_id,
                    message_id,
                })
                .collect();
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
        let new_id = sqlx::query(
            "INSERT INTO nodes
                (parent_id, name, is_dir, size, status, created_at, modified_at, sha256, favorite)
             VALUES (?, ?, 0, ?, 'ready', ?, ?, ?, ?)",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .bind(size)
        .bind(ts)
        .bind(ts)
        .bind(sha256)
        .bind(inherited_favorite)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO chunks
                (node_id, idx, size, tg_file_id, tg_message_id, storage_target_id,
                 tg_thumbnail_file_id)
             SELECT ?, idx, size, tg_file_id, tg_message_id, storage_target_id,
                    tg_thumbnail_file_id
             FROM chunks WHERE node_id = ?",
        )
        .bind(new_id)
        .bind(src_id)
        .execute(&mut *tx)
        .await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(Some((new_id, freed)))
    }

    // ---------- settings ----------

    /// 一致性快照备份(WAL 下直接拷 .db 文件会缺最新写入,VACUUM INTO 不会)。
    /// 目标路径由服务端自行生成,不接受用户输入。
    pub async fn vacuum_into(&self, dest: &str) -> AppResult<()> {
        let sql = format!("VACUUM INTO '{}'", dest.replace('\'', "''"));
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn setting_get(&self, k: &str) -> AppResult<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT v FROM settings WHERE k = ?")
            .bind(k)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn setting_set(&self, k: &str, v: &str) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO settings (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        )
        .bind(k)
        .bind(v)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn setting_del(&self, k: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM settings WHERE k = ?")
            .bind(k)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 原子批量更新设置，避免运行时仍是旧值而数据库只写入了一半新配置。
    pub async fn settings_apply(
        &self,
        sets: &[(String, String)],
        deletes: &[String],
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        for key in deletes {
            sqlx::query("DELETE FROM settings WHERE k = ?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        for (key, value) in sets {
            sqlx::query(
                "INSERT INTO settings (k, v) VALUES (?, ?)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 取某前缀下的全部设置(键已去掉前缀)
    pub async fn settings_with_prefix(&self, prefix: &str) -> AppResult<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT k, v FROM settings WHERE k LIKE ? || '%'")
                .bind(prefix)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(k, v)| (k.trim_start_matches(prefix).to_string(), v))
            .collect())
    }

    // ---------- 持久化删除队列 ----------

    pub async fn enqueue_delete_message(&self, message: StoredMessage) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO delete_jobs
                (storage_target_id, tg_message_id, attempts, next_retry_at, created_at)
             VALUES (?, ?, 0, 0, ?)
             ON CONFLICT(storage_target_id, tg_message_id) DO NOTHING",
        )
        .bind(message.storage_target_id)
        .bind(message.message_id)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_jobs_due(&self, limit: i64) -> AppResult<Vec<DeleteJob>> {
        // 极端情况下同一消息在入队后又被引用，安全起见取消对应删除任务。
        sqlx::query(
            "DELETE FROM delete_jobs
             WHERE EXISTS (
                 SELECT 1 FROM chunks
                 WHERE chunks.storage_target_id = delete_jobs.storage_target_id
                   AND chunks.tg_message_id = delete_jobs.tg_message_id
             )",
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_as(
            "SELECT id, storage_target_id, tg_message_id, attempts
             FROM delete_jobs
             WHERE next_retry_at <= ?
             ORDER BY next_retry_at, id
             LIMIT ?",
        )
        .bind(now_ts())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn delete_job_done(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM delete_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_job_failed(&self, id: i64, attempts: i64, error: &str) -> AppResult<()> {
        let exponent = attempts.clamp(0, 10) as u32;
        let delay = 5_i64
            .saturating_mul(2_i64.saturating_pow(exponent))
            .min(3600);
        let detail: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE delete_jobs
             SET attempts = ?, next_retry_at = ?, last_error = ?
             WHERE id = ?",
        )
        .bind(attempts + 1)
        .bind(now_ts() + delay)
        .bind(detail)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_delete_count(&self) -> AppResult<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM delete_jobs")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    // ---------- Telegram caption 路径同步队列 ----------

    pub async fn caption_sync_jobs_due(&self, limit: i64) -> AppResult<Vec<CaptionSyncJob>> {
        // 节点/分块已删除，或消息后来被零流量复制为多处引用时，不再同步单一路径。
        sqlx::query(
            "DELETE FROM caption_sync_jobs
             WHERE NOT EXISTS (
                 SELECT 1 FROM chunks c
                 WHERE c.node_id = caption_sync_jobs.node_id
                   AND c.idx = caption_sync_jobs.chunk_idx
                   AND c.storage_target_id = caption_sync_jobs.storage_target_id
                   AND c.tg_message_id = caption_sync_jobs.tg_message_id
             )
             OR 1 != (
                 SELECT COUNT(*) FROM chunks refs
                 WHERE refs.storage_target_id = caption_sync_jobs.storage_target_id
                   AND refs.tg_message_id = caption_sync_jobs.tg_message_id
             )",
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_as(
            "SELECT id, storage_target_id, tg_message_id, node_id, chunk_idx,
                    attempts, generation
             FROM caption_sync_jobs
             WHERE next_retry_at <= ?
             ORDER BY next_retry_at, id
             LIMIT ?",
        )
        .bind(now_ts())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn caption_sync_job_done(&self, id: i64, generation: i64) -> AppResult<bool> {
        let removed = sqlx::query("DELETE FROM caption_sync_jobs WHERE id = ? AND generation = ?")
            .bind(id)
            .bind(generation)
            .execute(&self.pool)
            .await?;
        Ok(removed.rows_affected() > 0)
    }

    pub async fn caption_sync_job_failed(
        &self,
        id: i64,
        generation: i64,
        attempts: i64,
        error: &str,
    ) -> AppResult<()> {
        let exponent = attempts.clamp(0, 10) as u32;
        let delay = 5_i64
            .saturating_mul(2_i64.saturating_pow(exponent))
            .min(3600);
        let detail: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE caption_sync_jobs
             SET attempts = ?, next_retry_at = ?, last_error = ?
             WHERE id = ? AND generation = ?",
        )
        .bind(attempts + 1)
        .bind(now_ts() + delay)
        .bind(detail)
        .bind(id)
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_caption_sync_count(&self) -> AppResult<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM caption_sync_jobs")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// 候选消息中，删除元数据后已无任何分块引用的消息；在同一事务内先写入删除队列。
    async fn freed_messages(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        candidates: Vec<StoredMessage>,
    ) -> AppResult<Vec<StoredMessage>> {
        let uniq: HashSet<StoredMessage> = candidates.into_iter().collect();
        let mut freed = Vec::new();
        for message in uniq {
            let still: Option<(i64,)> = sqlx::query_as(
                "SELECT 1 FROM chunks
                 WHERE storage_target_id = ? AND tg_message_id = ? LIMIT 1",
            )
            .bind(message.storage_target_id)
            .bind(message.message_id)
            .fetch_optional(&mut **tx)
            .await?;
            if still.is_none() {
                sqlx::query(
                    "INSERT INTO delete_jobs
                        (storage_target_id, tg_message_id, attempts, next_retry_at, created_at)
                     VALUES (?, ?, 0, 0, ?)
                     ON CONFLICT(storage_target_id, tg_message_id) DO NOTHING",
                )
                .bind(message.storage_target_id)
                .bind(message.message_id)
                .bind(now_ts())
                .execute(&mut **tx)
                .await?;
                freed.push(message);
            }
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TARGET: i64 = 1;

    fn stored(message_id: i64) -> StoredMessage {
        StoredMessage {
            storage_target_id: TEST_TARGET,
            message_id,
        }
    }

    async fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(path.to_str().unwrap()).await.unwrap();
        let target = db
            .ensure_storage_target("http://mock", "mock-token", "-1001")
            .await
            .unwrap();
        assert_eq!(target.id, TEST_TARGET);
        (db, dir)
    }

    #[tokio::test]
    async fn tree_basic() {
        let (db, _g) = test_db().await;
        let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
        assert!(db.mkdir(ROOT_ID, "docs").await.is_err()); // 重名
        let sub = db.mkdir(docs.id, "2026").await.unwrap();
        let found = db.resolve(&["docs", "2026"]).await.unwrap().unwrap();
        assert_eq!(found.id, sub.id);
        assert!(db.resolve(&["nope"]).await.unwrap().is_none());

        // 移动 + 防环
        db.move_node(sub.id, ROOT_ID, "y2026").await.unwrap();
        assert!(db.resolve(&["y2026"]).await.unwrap().is_some());
        assert!(db.move_node(docs.id, docs.id, "x").await.is_err());
    }

    #[tokio::test]
    async fn migration_removes_blob_thumbnail_cache() {
        let (db, _g) = test_db().await;
        let (tables,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'thumbnails'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(tables, 0);

        let (columns,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('chunks') WHERE name = 'tg_thumbnail_file_id'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(columns, 1);
    }

    #[tokio::test]
    async fn backend_logs_are_newest_first_and_paginated() {
        let (db, _g) = test_db().await;
        db.append_backend_log("info", "路径同步", "第一条")
            .await
            .unwrap();
        db.append_backend_log("warn", "路径同步", "第二条")
            .await
            .unwrap();
        db.append_backend_log("unknown", "", "").await.unwrap();

        let (latest, has_more) = db.backend_logs(2, None).await.unwrap();
        assert_eq!(latest.len(), 2);
        assert!(has_more);
        assert_eq!(latest[0].message, "后台事件");
        assert_eq!(latest[0].level, "info");
        assert_eq!(latest[1].message, "第二条");

        let (older, has_more) = db.backend_logs(2, Some(latest[1].id)).await.unwrap();
        assert!(!has_more);
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].message, "第一条");
    }

    #[tokio::test]
    async fn upload_lifecycle_and_refcount() {
        let (db, _g) = test_db().await;
        // 上传 2 块的文件
        let up = db.create_uploading(ROOT_ID, "a.bin").await.unwrap();
        assert!(db.uploading_child_exists(ROOT_ID, "a.bin").await.unwrap());
        db.add_chunk(up, 0, 10, "f0", 100, TEST_TARGET, Some("thumb-0"))
            .await
            .unwrap();
        db.add_chunk(up, 1, 5, "f1", 101, TEST_TARGET, None)
            .await
            .unwrap();
        // uploading 期间不可见
        assert!(db.resolve(&["a.bin"]).await.unwrap().is_none());
        let freed = db.finalize_upload(up, 15, Some("aabb")).await.unwrap();
        assert!(freed.is_empty());
        let node = db.resolve(&["a.bin"]).await.unwrap().unwrap();
        assert!(!db.uploading_child_exists(ROOT_ID, "a.bin").await.unwrap());
        assert_eq!(node.size, 15);
        assert_eq!(node.sha256.as_deref(), Some("aabb"));

        // 复制共享消息 → 删除原文件不释放消息
        db.copy_file(node.id, ROOT_ID, "b.bin").await.unwrap();
        let copied = db.resolve(&["b.bin"]).await.unwrap().unwrap();
        assert_eq!(
            db.chunks_of(copied.id).await.unwrap()[0]
                .tg_thumbnail_file_id
                .as_deref(),
            Some("thumb-0")
        );
        let freed = db.delete_node(node.id).await.unwrap();
        assert!(freed.is_empty());
        // 删除副本后消息释放
        let copy = db.resolve(&["b.bin"]).await.unwrap().unwrap();
        let mut freed = db.delete_node(copy.id).await.unwrap();
        freed.sort();
        assert_eq!(freed, vec![stored(100), stored(101)]);
    }

    #[tokio::test]
    async fn overwrite_replaces_old() {
        let (db, _g) = test_db().await;
        let v1 = db.create_uploading(ROOT_ID, "f").await.unwrap();
        db.add_chunk(v1, 0, 3, "f0", 200, TEST_TARGET, None)
            .await
            .unwrap();
        db.finalize_upload(v1, 3, None).await.unwrap();

        let v2 = db.create_uploading(ROOT_ID, "f").await.unwrap();
        db.add_chunk(v2, 0, 7, "f1", 201, TEST_TARGET, None)
            .await
            .unwrap();
        let freed = db.finalize_upload(v2, 7, None).await.unwrap();
        assert_eq!(freed, vec![stored(200)]); // 旧版本消息被释放
        let node = db.resolve(&["f"]).await.unwrap().unwrap();
        assert_eq!(node.size, 7);
        assert_eq!(node.id, v2);
    }

    #[tokio::test]
    async fn stale_upload_cleanup() {
        let (db, _g) = test_db().await;
        let up = db.create_uploading(ROOT_ID, "x").await.unwrap();
        db.add_chunk(up, 0, 1, "f", 300, TEST_TARGET, None)
            .await
            .unwrap();
        let freed = db.cleanup_stale_uploads().await.unwrap();
        assert_eq!(freed, vec![stored(300)]);
        assert!(db.node(up).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_uploads_to_same_path_are_rejected() {
        let (db, _g) = test_db().await;
        let (a, b) = tokio::join!(
            db.create_uploading(ROOT_ID, "same.bin"),
            db.create_uploading(ROOT_ID, "same.bin")
        );
        assert_ne!(a.is_ok(), b.is_ok(), "同一路径只能创建一个上传任务");
        let err = if let Err(e) = a { e } else { b.unwrap_err() };
        assert!(matches!(err, AppError::AlreadyExists));
    }

    /// 建一个 ready 文件的辅助
    async fn mk_file(db: &Db, parent: i64, name: &str, msg: i64, sha: &str) -> i64 {
        let up = db.create_uploading(parent, name).await.unwrap();
        db.add_chunk(up, 0, 4, &format!("f{msg}"), msg, TEST_TARGET, None)
            .await
            .unwrap();
        db.finalize_upload(up, 4, Some(sha)).await.unwrap();
        up
    }

    #[tokio::test]
    async fn moving_tree_queues_caption_sync_only_for_unshared_messages() {
        let (db, _g) = test_db().await;
        let folder = db.mkdir(ROOT_ID, "旧目录").await.unwrap();
        let unique = mk_file(&db, folder.id, "独占.bin", 410, "unique").await;
        let shared = mk_file(&db, folder.id, "共享.bin", 411, "shared").await;
        db.copy_file(shared, ROOT_ID, "共享副本.bin").await.unwrap();

        db.move_node(folder.id, ROOT_ID, "新目录").await.unwrap();
        let first = db.caption_sync_jobs_due(10).await.unwrap();
        assert_eq!(first.len(), 1, "共享 Telegram 消息不能绑定到单一路径");
        assert_eq!(first[0].node_id, unique);
        assert_eq!(
            db.full_path_if_visible(unique).await.unwrap().as_deref(),
            Some("/新目录/独占.bin")
        );

        // 同步过程中再次改名时会增加代次；旧 worker 不能误删新任务。
        db.move_node(folder.id, ROOT_ID, "最终目录").await.unwrap();
        db.caption_sync_job_done(first[0].id, first[0].generation)
            .await
            .unwrap();
        let latest = db.caption_sync_jobs_due(10).await.unwrap();
        assert_eq!(latest.len(), 1);
        assert!(latest[0].generation > first[0].generation);
        assert_eq!(
            db.full_path_if_visible(unique).await.unwrap().as_deref(),
            Some("/最终目录/独占.bin")
        );
    }

    #[tokio::test]
    async fn node_usage_counts_directory_tree_without_downloading() {
        let (db, _g) = test_db().await;
        let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
        let sub = db.mkdir(docs.id, "sub").await.unwrap();
        let first = mk_file(&db, docs.id, "a.bin", 450, "a").await;
        mk_file(&db, sub.id, "b.bin", 451, "b").await;

        let usage = db.node_usage(docs.id).await.unwrap();
        assert_eq!(usage.file_count, 2);
        assert_eq!(usage.dir_count, 1);
        assert_eq!(usage.total_size, 8);
        assert_eq!(usage.chunk_count, 2);

        let file_usage = db.node_usage(first).await.unwrap();
        assert_eq!(file_usage.file_count, 1);
        assert_eq!(file_usage.dir_count, 0);
        assert_eq!(file_usage.total_size, 4);
        assert_eq!(file_usage.chunk_count, 1);
    }

    #[tokio::test]
    async fn trash_lifecycle() {
        let (db, _g) = test_db().await;
        let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
        let fid = mk_file(&db, docs.id, "a.txt", 500, "s1").await;
        db.share_create(fid, None, None, None).await.unwrap();

        // 软删除目录:子树不可见,名字可重用,分享被清掉,消息不释放
        db.trash_node(docs.id, "/docs").await.unwrap();
        assert!(db.resolve(&["docs"]).await.unwrap().is_none());
        assert!(db.shares_list().await.unwrap().is_empty());
        db.mkdir(ROOT_ID, "docs").await.unwrap(); // 同名目录可再建
        let (files, _, _) = db.stats().await.unwrap();
        assert_eq!(files, 0, "回收站里的文件不计入统计");

        // 列表 + 恢复(原名被占 → 自动改名)
        let list = db.trash_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].1, "/docs");
        let back = db.trash_restore(list[0].0.id).await.unwrap();
        assert_eq!(back, "/docs (恢复)");
        assert!(
            db.resolve(&["docs (恢复)", "a.txt"])
                .await
                .unwrap()
                .is_some()
        );

        // 再删一次后超期清理:消息引用归零被释放
        db.trash_node(
            db.resolve(&["docs (恢复)"]).await.unwrap().unwrap().id,
            "/docs (恢复)",
        )
        .await
        .unwrap();
        let freed = db.trash_purge_expired(0).await.unwrap();
        assert_eq!(freed, vec![stored(500)]);
        assert!(db.trash_list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn instant_link_hits_and_overwrites() {
        let (db, _g) = test_db().await;
        let src_id = mk_file(&db, ROOT_ID, "src.bin", 600, "deadbeef").await;
        sqlx::query("UPDATE chunks SET tg_thumbnail_file_id = 'instant-thumb' WHERE node_id = ?")
            .bind(src_id)
            .execute(&db.pool)
            .await
            .unwrap();

        // 对已经成功的同一路径重复探测必须是无副作用成功，分块不能被误删。
        let (same_id, freed) = db
            .instant_link("deadbeef", 4, ROOT_ID, "src.bin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(same_id, src_id);
        assert!(freed.is_empty());
        assert_eq!(db.chunks_of(src_id).await.unwrap().len(), 1);

        // 未命中
        assert!(
            db.instant_link("nope", 4, ROOT_ID, "x")
                .await
                .unwrap()
                .is_none()
        );
        // 命中:零流量链接,共享消息
        let (nid, freed) = db
            .instant_link("deadbeef", 4, ROOT_ID, "copy.bin")
            .await
            .unwrap()
            .unwrap();
        assert!(freed.is_empty());
        let n = db.node(nid).await.unwrap();
        assert_eq!(n.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(
            db.chunks_of(nid).await.unwrap()[0]
                .tg_thumbnail_file_id
                .as_deref(),
            Some("instant-thumb")
        );
        // 删源文件不释放消息(副本还引用着)
        let src = db.resolve(&["src.bin"]).await.unwrap().unwrap();
        assert!(db.delete_node(src.id).await.unwrap().is_empty());

        // 秒传覆盖同名文件:旧消息释放
        mk_file(&db, ROOT_ID, "old.bin", 700, "cafe").await;
        let (_, freed) = db
            .instant_link("deadbeef", 4, ROOT_ID, "old.bin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(freed, vec![stored(700)]);
    }

    #[tokio::test]
    async fn search_skips_trashed() {
        let (db, _g) = test_db().await;
        let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
        mk_file(&db, docs.id, "报告.pdf", 800, "s").await;
        mk_file(&db, ROOT_ID, "年度报告.pdf", 801, "s2").await;

        let hits = db.search("报告", 100).await.unwrap();
        let mut paths: Vec<_> = hits.iter().map(|(_, p)| p.clone()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/docs/报告.pdf", "/年度报告.pdf"]);

        db.trash_node(docs.id, "/docs").await.unwrap();
        let hits = db.search("报告", 100).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "/年度报告.pdf");
    }

    #[tokio::test]
    async fn advanced_search_filters_and_favorites() {
        let (db, _g) = test_db().await;
        let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
        let report = mk_file(&db, docs.id, "季度报告.PDF", 820, "pdf").await;
        mk_file(&db, ROOT_ID, "录音.mp3", 821, "audio").await;
        db.set_favorite(report, true).await.unwrap();
        db.set_favorite(docs.id, true).await.unwrap();
        sqlx::query("UPDATE nodes SET modified_at = 2000 WHERE id = ?")
            .bind(report)
            .execute(&db.pool)
            .await
            .unwrap();

        let hits = db
            .search_filtered(
                &SearchFilter {
                    query: Some("报告".into()),
                    is_dir: Some(false),
                    extensions: vec!["pdf".into()],
                    min_size: Some(4),
                    max_size: Some(4),
                    modified_after: Some(1000),
                    modified_before: Some(3000),
                    favorite_only: true,
                    sort: SearchSort::Modified,
                    descending: true,
                },
                100,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "/docs/季度报告.PDF");
        assert!(hits[0].0.favorite);

        let favorites = db
            .search_filtered(
                &SearchFilter {
                    favorite_only: true,
                    ..SearchFilter::default()
                },
                100,
            )
            .await
            .unwrap();
        assert_eq!(favorites.len(), 2, "文件和目录都可以收藏");
    }

    #[tokio::test]
    async fn resumable_upload_session_lifecycle() {
        let (db, _g) = test_db().await;
        let sha = "ab".repeat(32);
        let session = db
            .create_upload_session(ROOT_ID, "large.bin", "/large.bin", 10, &sha, 4, TEST_TARGET)
            .await
            .unwrap();
        assert_eq!(session.part_count(), 3);
        assert_eq!(session.expected_part_size(2), Some(2));
        let same = db
            .create_upload_session(
                ROOT_ID,
                "large.bin",
                "/large.bin",
                10,
                &sha,
                99,
                TEST_TARGET,
            )
            .await
            .unwrap();
        assert_eq!(same.session.id, session.session.id, "相同内容应恢复原会话");

        assert!(matches!(
            db.claim_upload_part(&session.session.id, 0).await.unwrap(),
            UploadPartClaim::Claimed(_)
        ));
        db.finish_upload_part(&session.session.id, 0, 4, "part-0", 900, TEST_TARGET, None)
            .await
            .unwrap();
        assert!(
            db.complete_upload_session(&session.session.id)
                .await
                .is_err()
        );
        assert!(db.cleanup_stale_uploads().await.unwrap().is_empty());

        for (idx, size) in [(1, 4), (2, 2)] {
            db.claim_upload_part(&session.session.id, idx)
                .await
                .unwrap();
            db.finish_upload_part(
                &session.session.id,
                idx,
                size,
                &format!("part-{idx}"),
                900 + idx,
                TEST_TARGET,
                None,
            )
            .await
            .unwrap();
        }
        let node = db
            .complete_upload_session(&session.session.id)
            .await
            .unwrap();
        assert_eq!(node.size, 10);
        assert_eq!(node.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(db.chunks_of(node.id).await.unwrap().len(), 3);
        assert!(db.upload_session_info(&session.session.id).await.is_err());
    }

    #[tokio::test]
    async fn share_roundtrip() {
        let (db, _g) = test_db().await;
        let fid = mk_file(&db, ROOT_ID, "a.txt", 900, "sh").await;
        let s = db
            .share_create(fid, Some("1234"), Some(now_ts() + 100), Some(5))
            .await
            .unwrap();
        assert_eq!(s.token.len(), 12);
        let got = db.share_by_token(&s.token).await.unwrap().unwrap();
        assert_eq!(got.password.as_deref(), Some("1234"));
        assert!(
            db.share_claim_download(got.id, "ticket-a", fid, now_ts() + 100)
                .await
                .unwrap()
        );
        // 同一票据读取同一文件（例如多个 Range）不重复计数。
        assert!(
            db.share_claim_download(got.id, "ticket-a", fid, now_ts() + 100)
                .await
                .unwrap()
        );
        assert_eq!(
            db.share_by_token(&s.token)
                .await
                .unwrap()
                .unwrap()
                .downloads,
            1
        );

        // 并发票据也不能冲破下载上限。
        let limited = db.share_create(fid, None, None, Some(1)).await.unwrap();
        let (a, b) = tokio::join!(
            db.share_claim_download(limited.id, "ticket-1", fid, now_ts() + 100),
            db.share_claim_download(limited.id, "ticket-2", fid, now_ts() + 100),
        );
        let accepted = [a.unwrap(), b.unwrap()]
            .into_iter()
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(accepted, 1);
        assert_eq!(
            db.share_by_token(&limited.token)
                .await
                .unwrap()
                .unwrap()
                .downloads,
            1
        );
        // 彻底删除节点 → 分享级联消失
        db.delete_node(fid).await.unwrap();
        assert!(db.share_by_token(&s.token).await.unwrap().is_none());
    }
}
