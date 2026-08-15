//! SQLite 元数据层:目录树(nodes)与分块映射(chunks)
//!
//! 约定:根目录 id=1;列表/解析只看 status='ready' 的行;
//! 覆盖写通过 uploading 影子行 + finalize 事务原子替换实现。

use crate::error::{AppError, AppResult};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
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
}

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

/// URL 安全随机 token
fn random_token(len: usize) -> String {
    use rand::RngExt as _;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

pub fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        if let Some(dir) = std::path::Path::new(db_path).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Db { pool })
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
        Ok(sqlx::query_as::<_, Chunk>("SELECT * FROM chunks WHERE node_id = ? ORDER BY idx")
            .bind(node_id)
            .fetch_all(&self.pool)
            .await?)
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
    pub async fn move_node(&self, id: i64, new_parent: i64, new_name: &str) -> AppResult<()> {
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
        if let Some(existing) = self.child(new_parent, new_name).await? {
            if existing.id != id {
                return Err(AppError::AlreadyExists);
            }
        }
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, modified_at = ? WHERE id = ?")
            .bind(new_parent)
            .bind(new_name)
            .bind(now_ts())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- 上传生命周期 ----------

    /// 创建 uploading 状态的文件占位节点(允许与同名 ready 文件并存,finalize 时替换)
    pub async fn create_uploading(&self, parent_id: i64, name: &str) -> AppResult<i64> {
        let parent = self.node(parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if let Some(existing) = self.child(parent_id, name).await? {
            if existing.is_dir {
                return Err(AppError::IsDirectory); // 不允许文件覆盖目录
            }
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

    pub async fn add_chunk(
        &self,
        node_id: i64,
        idx: i64,
        size: i64,
        tg_file_id: &str,
        tg_message_id: i64,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO chunks (node_id, idx, size, tg_file_id, tg_message_id)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(node_id)
        .bind(idx)
        .bind(size)
        .bind(tg_file_id)
        .bind(tg_message_id)
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
    ) -> AppResult<Vec<i64>> {
        let (_, freed) = self.finalize_upload_with_node(node_id, size, sha256).await?;
        Ok(freed)
    }

    /// 完成上传并在同一事务内返回最终节点，避免提交后再次查询造成“已成功却报失败”。
    pub async fn finalize_upload_with_node(
        &self,
        node_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> AppResult<(Node, Vec<i64>)> {
        let node = self.node(node_id).await?;
        let parent_id = node.parent_id.ok_or(AppError::NotFound)?;

        let mut tx = self.pool.begin().await?;
        // 旧同名 ready 文件(覆盖写场景)
        let old: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM nodes
             WHERE parent_id = ? AND name = ? AND status = 'ready' AND id != ?",
        )
        .bind(parent_id)
        .bind(&node.name)
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;

        let mut candidates: Vec<i64> = Vec::new();
        if let Some((old_id,)) = old {
            let rows: Vec<(i64,)> =
                sqlx::query_as("SELECT tg_message_id FROM chunks WHERE node_id = ?")
                    .bind(old_id)
                    .fetch_all(&mut *tx)
                    .await?;
            candidates = rows.into_iter().map(|r| r.0).collect();
            sqlx::query("DELETE FROM nodes WHERE id = ?").bind(old_id).execute(&mut *tx).await?;
        }
        sqlx::query(
            "UPDATE nodes SET status = 'ready', size = ?, sha256 = ?, modified_at = ? WHERE id = ?",
        )
        .bind(size)
        .bind(sha256)
        .bind(now_ts())
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
    pub async fn abort_upload(&self, node_id: i64) -> AppResult<Vec<i64>> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(i64,)> = sqlx::query_as("SELECT tg_message_id FROM chunks WHERE node_id = ?")
            .bind(node_id)
            .fetch_all(&mut *tx)
            .await?;
        let candidates: Vec<i64> = rows.into_iter().map(|r| r.0).collect();
        sqlx::query("DELETE FROM nodes WHERE id = ?").bind(node_id).execute(&mut *tx).await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(freed)
    }

    /// 启动时清理所有 uploading 残留,返回可删的 message_id
    pub async fn cleanup_stale_uploads(&self) -> AppResult<Vec<i64>> {
        let ids: Vec<(i64,)> =
            sqlx::query_as("SELECT id FROM nodes WHERE status = 'uploading'")
                .fetch_all(&self.pool)
                .await?;
        let mut freed = Vec::new();
        for (id,) in ids {
            freed.extend(self.abort_upload(id).await?);
        }
        Ok(freed)
    }

    // ---------- 删除/复制 ----------

    /// 递归删除节点;返回引用归零、需要在 TG 删除的 message_id
    pub async fn delete_node(&self, id: i64) -> AppResult<Vec<i64>> {
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

        let mut candidates: Vec<i64> = Vec::new();
        for (nid,) in &node_ids {
            let rows: Vec<(i64,)> =
                sqlx::query_as("SELECT tg_message_id FROM chunks WHERE node_id = ?")
                    .bind(nid)
                    .fetch_all(&mut *tx)
                    .await?;
            candidates.extend(rows.into_iter().map(|r| r.0));
        }
        // 子节点行由 ON DELETE CASCADE 级联;显式删整棵树避免依赖外键深度
        for (nid,) in node_ids.iter().rev() {
            sqlx::query("DELETE FROM nodes WHERE id = ?").bind(nid).execute(&mut *tx).await?;
        }
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(freed)
    }

    /// 复制文件:仅复制元数据行,共享 Telegram 消息(零流量)
    pub async fn copy_file(
        &self,
        src_id: i64,
        dst_parent: i64,
        dst_name: &str,
    ) -> AppResult<i64> {
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
            "INSERT INTO chunks (node_id, idx, size, tg_file_id, tg_message_id)
             SELECT ?, idx, size, tg_file_id, tg_message_id FROM chunks WHERE node_id = ?",
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
        sqlx::query("DELETE FROM trash WHERE node_id = ?").bind(node_id).execute(&mut *tx).await?;
        tx.commit().await?;

        Ok(self.full_path_if_visible(node_id).await?.unwrap_or_else(|| format!("/{name}")))
    }

    /// 彻底删除回收站中超期的项,返回引用归零的 message_id
    pub async fn trash_purge_expired(&self, retention_days: i64) -> AppResult<Vec<i64>> {
        let cutoff = now_ts() - retention_days * 86400;
        let ids: Vec<(i64,)> =
            sqlx::query_as("SELECT node_id FROM trash WHERE trashed_at <= ?")
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
                        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("分享刚创建即丢失")))
                }
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(AppError::Internal(anyhow::anyhow!("生成分享 token 失败")))
    }

    pub async fn share_by_token(&self, token: &str) -> AppResult<Option<Share>> {
        Ok(sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token = ?")
            .bind(token)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn shares_list(&self) -> AppResult<Vec<Share>> {
        Ok(sqlx::query_as::<_, Share>("SELECT * FROM shares ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn share_delete(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM shares WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// 下载计数 +1
    pub async fn share_bump(&self, id: i64) -> AppResult<()> {
        sqlx::query("UPDATE shares SET downloads = downloads + 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- 搜索 / 秒传 ----------

    /// 全库按名称搜索(不区分大小写),排除回收站与上传中的子树;返回 (节点, 完整路径)
    pub async fn search(&self, query: &str, limit: usize) -> AppResult<Vec<(Node, String)>> {
        // 个人网盘节点量级有限,全表进内存过滤,顺带拼路径
        let all: Vec<Node> = sqlx::query_as("SELECT * FROM nodes").fetch_all(&self.pool).await?;
        let by_id: HashMap<i64, &Node> = all.iter().map(|n| (n.id, n)).collect();
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for n in &all {
            if n.id == ROOT_ID || n.status != "ready" || !n.name.to_lowercase().contains(&q) {
                continue;
            }
            let mut parts = vec![n.name.clone()];
            let mut cur = n.parent_id;
            let mut visible = true;
            while let Some(pid) = cur {
                if pid == ROOT_ID {
                    break;
                }
                match by_id.get(&pid) {
                    Some(p) if p.status == "ready" => {
                        parts.push(p.name.clone());
                        cur = p.parent_id;
                    }
                    _ => {
                        visible = false;
                        break;
                    }
                }
            }
            if !visible {
                continue;
            }
            parts.reverse();
            out.push((n.clone(), format!("/{}", parts.join("/"))));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// 秒传:找到内容相同(sha256+size)的既有文件则零流量链接到目标位置,
    /// 覆盖同名旧文件;返回 (新节点 id, 释放的 message_id)。找不到返回 None。
    pub async fn instant_link(
        &self,
        sha256: &str,
        size: i64,
        dst_parent: i64,
        dst_name: &str,
    ) -> AppResult<Option<(i64, Vec<i64>)>> {
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
        let Some((src_id,)) = src else { return Ok(None) };

        if let Some(existing) = self.child(dst_parent, dst_name).await? {
            if existing.is_dir {
                return Err(AppError::IsDirectory);
            }
        }
        let ts = now_ts();
        let mut tx = self.pool.begin().await?;
        // 覆盖写:删同名旧文件并收集其消息做引用计数
        let old: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM nodes WHERE parent_id = ? AND name = ? AND status = 'ready'",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .fetch_optional(&mut *tx)
        .await?;
        let mut candidates = Vec::new();
        if let Some((old_id,)) = old {
            let rows: Vec<(i64,)> =
                sqlx::query_as("SELECT tg_message_id FROM chunks WHERE node_id = ?")
                    .bind(old_id)
                    .fetch_all(&mut *tx)
                    .await?;
            candidates = rows.into_iter().map(|r| r.0).collect();
            sqlx::query("DELETE FROM nodes WHERE id = ?").bind(old_id).execute(&mut *tx).await?;
        }
        let new_id = sqlx::query(
            "INSERT INTO nodes (parent_id, name, is_dir, size, status, created_at, modified_at, sha256)
             VALUES (?, ?, 0, ?, 'ready', ?, ?, ?)",
        )
        .bind(dst_parent)
        .bind(dst_name)
        .bind(size)
        .bind(ts)
        .bind(ts)
        .bind(sha256)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO chunks (node_id, idx, size, tg_file_id, tg_message_id)
             SELECT ?, idx, size, tg_file_id, tg_message_id FROM chunks WHERE node_id = ?",
        )
        .bind(new_id)
        .bind(src_id)
        .execute(&mut *tx)
        .await?;
        let freed = Self::freed_messages(&mut tx, candidates).await?;
        tx.commit().await?;
        Ok(Some((new_id, freed)))
    }

    // ---------- 视频缩略图 ----------

    /// 按文件内容哈希写入缩略图；相同内容的复制与秒传共享同一份缓存。
    pub async fn thumbnail_put(&self, sha256: &str, mime: &str, data: &[u8]) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO thumbnails (sha256, mime, data, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(sha256) DO UPDATE SET
                 mime = excluded.mime,
                 data = excluded.data,
                 created_at = excluded.created_at",
        )
        .bind(sha256)
        .bind(mime)
        .bind(data)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn thumbnail_get(&self, sha256: &str) -> AppResult<Option<(String, Vec<u8>)>> {
        Ok(sqlx::query_as("SELECT mime, data FROM thumbnails WHERE sha256 = ?")
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// 清理已经没有任何文件引用的缩略图缓存，返回删除数量。
    pub async fn cleanup_orphan_thumbnails(&self) -> AppResult<u64> {
        Ok(sqlx::query(
            "DELETE FROM thumbnails
             WHERE NOT EXISTS (
                 SELECT 1 FROM nodes
                 WHERE nodes.sha256 = thumbnails.sha256
             )",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    // ---------- settings ----------

    /// 一致性快照备份(WAL 下直接拷 .db 文件会缺最新写入,VACUUM INTO 不会)。
    /// 目标路径由服务端自行生成,不接受用户输入。
    pub async fn vacuum_into(&self, dest: &str) -> AppResult<()> {
        let sql = format!("VACUUM INTO '{}'", dest.replace('\'', "''"));
        sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&self.pool).await?;
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
        sqlx::query("INSERT INTO settings (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v")
            .bind(k)
            .bind(v)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn setting_del(&self, k: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM settings WHERE k = ?").bind(k).execute(&self.pool).await?;
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

    /// 候选 message_id 中,删除操作后已无任何 chunks 行引用的那些
    async fn freed_messages(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        candidates: Vec<i64>,
    ) -> AppResult<Vec<i64>> {
        let uniq: HashSet<i64> = candidates.into_iter().collect();
        let mut freed = Vec::new();
        for mid in uniq {
            let still: Option<(i64,)> =
                sqlx::query_as("SELECT 1 FROM chunks WHERE tg_message_id = ? LIMIT 1")
                    .bind(mid)
                    .fetch_optional(&mut **tx)
                    .await?;
            if still.is_none() {
                freed.push(mid);
            }
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(path.to_str().unwrap()).await.unwrap();
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
    async fn upload_lifecycle_and_refcount() {
        let (db, _g) = test_db().await;
        // 上传 2 块的文件
        let up = db.create_uploading(ROOT_ID, "a.bin").await.unwrap();
        assert!(db.uploading_child_exists(ROOT_ID, "a.bin").await.unwrap());
        db.add_chunk(up, 0, 10, "f0", 100).await.unwrap();
        db.add_chunk(up, 1, 5, "f1", 101).await.unwrap();
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
        let freed = db.delete_node(node.id).await.unwrap();
        assert!(freed.is_empty());
        // 删除副本后消息释放
        let copy = db.resolve(&["b.bin"]).await.unwrap().unwrap();
        let mut freed = db.delete_node(copy.id).await.unwrap();
        freed.sort();
        assert_eq!(freed, vec![100, 101]);
    }

    #[tokio::test]
    async fn overwrite_replaces_old() {
        let (db, _g) = test_db().await;
        let v1 = db.create_uploading(ROOT_ID, "f").await.unwrap();
        db.add_chunk(v1, 0, 3, "f0", 200).await.unwrap();
        db.finalize_upload(v1, 3, None).await.unwrap();

        let v2 = db.create_uploading(ROOT_ID, "f").await.unwrap();
        db.add_chunk(v2, 0, 7, "f1", 201).await.unwrap();
        let freed = db.finalize_upload(v2, 7, None).await.unwrap();
        assert_eq!(freed, vec![200]); // 旧版本消息被释放
        let node = db.resolve(&["f"]).await.unwrap().unwrap();
        assert_eq!(node.size, 7);
        assert_eq!(node.id, v2);
    }

    #[tokio::test]
    async fn stale_upload_cleanup() {
        let (db, _g) = test_db().await;
        let up = db.create_uploading(ROOT_ID, "x").await.unwrap();
        db.add_chunk(up, 0, 1, "f", 300).await.unwrap();
        let freed = db.cleanup_stale_uploads().await.unwrap();
        assert_eq!(freed, vec![300]);
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
        db.add_chunk(up, 0, 4, &format!("f{msg}"), msg).await.unwrap();
        db.finalize_upload(up, 4, Some(sha)).await.unwrap();
        up
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
        assert!(db.resolve(&["docs (恢复)", "a.txt"]).await.unwrap().is_some());

        // 再删一次后超期清理:消息引用归零被释放
        db.trash_node(db.resolve(&["docs (恢复)"]).await.unwrap().unwrap().id, "/docs (恢复)")
            .await
            .unwrap();
        let freed = db.trash_purge_expired(0).await.unwrap();
        assert_eq!(freed, vec![500]);
        assert!(db.trash_list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn instant_link_hits_and_overwrites() {
        let (db, _g) = test_db().await;
        let src_id = mk_file(&db, ROOT_ID, "src.bin", 600, "deadbeef").await;

        // 对已经成功的同一路径重复探测必须是无副作用成功，分块不能被误删。
        let (same_id, freed) =
            db.instant_link("deadbeef", 4, ROOT_ID, "src.bin").await.unwrap().unwrap();
        assert_eq!(same_id, src_id);
        assert!(freed.is_empty());
        assert_eq!(db.chunks_of(src_id).await.unwrap().len(), 1);

        // 未命中
        assert!(db.instant_link("nope", 4, ROOT_ID, "x").await.unwrap().is_none());
        // 命中:零流量链接,共享消息
        let (nid, freed) =
            db.instant_link("deadbeef", 4, ROOT_ID, "copy.bin").await.unwrap().unwrap();
        assert!(freed.is_empty());
        let n = db.node(nid).await.unwrap();
        assert_eq!(n.sha256.as_deref(), Some("deadbeef"));
        // 删源文件不释放消息(副本还引用着)
        let src = db.resolve(&["src.bin"]).await.unwrap().unwrap();
        assert!(db.delete_node(src.id).await.unwrap().is_empty());

        // 秒传覆盖同名文件:旧消息释放
        mk_file(&db, ROOT_ID, "old.bin", 700, "cafe").await;
        let (_, freed) =
            db.instant_link("deadbeef", 4, ROOT_ID, "old.bin").await.unwrap().unwrap();
        assert_eq!(freed, vec![700]);
    }

    #[tokio::test]
    async fn thumbnail_cache_follows_content_hash() {
        let (db, _g) = test_db().await;
        let data = b"RIFFxxxxWEBPthumbnail";
        db.thumbnail_put("thumb-sha", "image/webp", data).await.unwrap();
        let got = db.thumbnail_get("thumb-sha").await.unwrap().unwrap();
        assert_eq!(got.0, "image/webp");
        assert_eq!(got.1, data);

        let node = mk_file(&db, ROOT_ID, "video.mp4", 750, "thumb-sha").await;
        assert_eq!(db.cleanup_orphan_thumbnails().await.unwrap(), 0);
        db.delete_node(node).await.unwrap();
        assert_eq!(db.cleanup_orphan_thumbnails().await.unwrap(), 1);
        assert!(db.thumbnail_get("thumb-sha").await.unwrap().is_none());
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
    async fn share_roundtrip() {
        let (db, _g) = test_db().await;
        let fid = mk_file(&db, ROOT_ID, "a.txt", 900, "sh").await;
        let s = db.share_create(fid, Some("1234"), Some(now_ts() + 100), Some(5)).await.unwrap();
        assert_eq!(s.token.len(), 12);
        let got = db.share_by_token(&s.token).await.unwrap().unwrap();
        assert_eq!(got.password.as_deref(), Some("1234"));
        db.share_bump(got.id).await.unwrap();
        assert_eq!(db.share_by_token(&s.token).await.unwrap().unwrap().downloads, 1);
        // 彻底删除节点 → 分享级联消失
        db.delete_node(fid).await.unwrap();
        assert!(db.share_by_token(&s.token).await.unwrap().is_none());
    }
}
