//! 应用封面缓存：插件生成的小图存进主库旁的独立 SQLite 文件。
//! 封面随时可以重新生成，因此不进数据库备份、不走主库迁移，清空后归还磁盘空间。
use super::{Db, now_ts};
use crate::error::AppResult;
use sqlx::sqlite::{SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use std::path::PathBuf;

/// 每个应用的封面缓存上限；超出后按最近访问时间淘汰到上限的 90%。
pub const COVER_LIMIT_BYTES: i64 = 256 * 1024 * 1024;
pub const COVER_LIMIT_ENTRIES: i64 = 50_000;
/// 读取时最多每天刷新一次访问时间，避免每次浏览都产生写入。
const TOUCH_INTERVAL: i64 = 86_400;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS covers (
    app_id TEXT NOT NULL,
    key TEXT NOT NULL,
    mime TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    meta TEXT,
    updated_at INTEGER NOT NULL,
    accessed_at INTEGER NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY (app_id, key)
);
-- 覆盖索引：占用统计与淘汰只扫索引，不读带封面数据的表页。
CREATE INDEX IF NOT EXISTS idx_covers_lru ON covers(app_id, accessed_at, key, bytes);
PRAGMA user_version = 1;";

#[derive(Clone, Copy, Debug)]
pub struct CoverLimits {
    pub bytes: i64,
    pub entries: i64,
}

impl Default for CoverLimits {
    fn default() -> Self {
        Self {
            bytes: COVER_LIMIT_BYTES,
            entries: COVER_LIMIT_ENTRIES,
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct CoverRecord {
    pub key: String,
    pub mime: String,
    pub meta: Option<String>,
    pub updated_at: i64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverStats {
    pub entries: i64,
    pub bytes: i64,
}

/// 封面库与主库同目录：`tgdrive.db` → `tgdrive-covers.db`。
pub fn covers_path(db_path: &str) -> PathBuf {
    let path = std::path::Path::new(db_path);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tgdrive".into());
    path.with_file_name(format!("{stem}-covers.db"))
}

pub(crate) async fn open_covers(db_path: &str) -> anyhow::Result<SqlitePool> {
    // 增量回收必须在建表前设定；清空封面后据此把空闲页还给文件系统。
    let options = SqliteConnectOptions::new()
        .filename(covers_path(db_path))
        .create_if_missing(true)
        .auto_vacuum(SqliteAutoVacuum::Incremental)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    Ok(pool)
}

fn push_keys(sql: &mut QueryBuilder<Sqlite>, keys: &[String]) {
    sql.push(" AND key IN (");
    let mut list = sql.separated(", ");
    for key in keys {
        list.push_bind(key);
    }
    sql.push(")");
}

impl Db {
    pub async fn cover_get_many(&self, app: &str, keys: &[String]) -> AppResult<Vec<CoverRecord>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT key, mime, meta, updated_at, data FROM covers WHERE app_id = ",
        );
        sql.push_bind(app);
        push_keys(&mut sql, keys);
        let rows: Vec<CoverRecord> = sql.build_query_as().fetch_all(&self.covers).await?;
        if !rows.is_empty() {
            let now = now_ts();
            let mut touch = QueryBuilder::<Sqlite>::new("UPDATE covers SET accessed_at = ");
            touch
                .push_bind(now)
                .push(" WHERE app_id = ")
                .push_bind(app)
                .push(" AND accessed_at < ")
                .push_bind(now - TOUCH_INTERVAL);
            push_keys(&mut touch, keys);
            touch.build().execute(&self.covers).await?;
        }
        Ok(rows)
    }

    /// 写入或覆盖一张封面，返回因超出上限而淘汰的张数；刚写入的封面不会被淘汰。
    pub async fn cover_put(
        &self,
        app: &str,
        key: &str,
        mime: &str,
        data: &[u8],
        meta: Option<&str>,
        limits: CoverLimits,
    ) -> AppResult<u64> {
        let now = now_ts();
        let mut tx = self.covers.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO covers (app_id, key, mime, bytes, meta, updated_at, accessed_at, data)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(app_id, key) DO UPDATE SET mime = excluded.mime, bytes = excluded.bytes,
             meta = excluded.meta, updated_at = excluded.updated_at,
             accessed_at = excluded.accessed_at, data = excluded.data",
        )
        .bind(app)
        .bind(key)
        .bind(mime)
        .bind(data.len() as i64)
        .bind(meta)
        .bind(now)
        .bind(now)
        .bind(data)
        .execute(&mut *tx)
        .await?;
        let (mut entries, mut bytes): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM covers WHERE app_id = ?")
                .bind(app)
                .fetch_one(&mut *tx)
                .await?;
        let mut evicted = 0u64;
        if entries > limits.entries || bytes > limits.bytes {
            let (keep_entries, keep_bytes) = (limits.entries * 9 / 10, limits.bytes * 9 / 10);
            while entries > keep_entries || bytes > keep_bytes {
                let oldest: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT key, bytes FROM covers WHERE app_id = ? AND key != ?
                     ORDER BY accessed_at, key LIMIT 256",
                )
                .bind(app)
                .bind(key)
                .fetch_all(&mut *tx)
                .await?;
                if oldest.is_empty() {
                    break;
                }
                let mut victims = Vec::new();
                for (victim, size) in oldest {
                    if entries <= keep_entries && bytes <= keep_bytes {
                        break;
                    }
                    victims.push(victim);
                    entries -= 1;
                    bytes -= size;
                }
                let mut delete = QueryBuilder::<Sqlite>::new("DELETE FROM covers WHERE app_id = ");
                delete.push_bind(app);
                push_keys(&mut delete, &victims);
                delete.build().execute(&mut *tx).await?;
                evicted += victims.len() as u64;
            }
        }
        tx.commit().await?;
        Ok(evicted)
    }

    pub async fn cover_delete(&self, app: &str, keys: &[String]) -> AppResult<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut sql = QueryBuilder::<Sqlite>::new("DELETE FROM covers WHERE app_id = ");
        sql.push_bind(app);
        push_keys(&mut sql, keys);
        Ok(sql.build().execute(&self.covers).await?.rows_affected())
    }

    pub async fn cover_stats(&self, app: &str) -> AppResult<CoverStats> {
        let (entries, bytes): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM covers WHERE app_id = ?")
                .bind(app)
                .fetch_one(&self.covers)
                .await?;
        Ok(CoverStats { entries, bytes })
    }

    /// 清空应用的全部封面并归还空闲页，返回删除的张数。
    pub async fn cover_clear(&self, app: &str) -> AppResult<u64> {
        let removed = sqlx::query("DELETE FROM covers WHERE app_id = ?")
            .bind(app)
            .execute(&self.covers)
            .await?
            .rows_affected();
        if removed > 0 {
            sqlx::query("PRAGMA incremental_vacuum")
                .execute(&self.covers)
                .await?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (db, dir)
    }

    async fn backdate(db: &Db, key: &str, at: i64) {
        sqlx::query("UPDATE covers SET accessed_at = ? WHERE app_id = 'books' AND key = ?")
            .bind(at)
            .bind(key)
            .execute(&db.covers)
            .await
            .unwrap();
    }

    async fn keys(db: &Db) -> Vec<String> {
        sqlx::query_scalar("SELECT key FROM covers WHERE app_id = 'books' ORDER BY key")
            .fetch_all(&db.covers)
            .await
            .unwrap()
    }

    #[test]
    fn covers_file_sits_next_to_main_database() {
        assert_eq!(
            covers_path("data/tgdrive.db"),
            PathBuf::from("data/tgdrive-covers.db")
        );
        assert_eq!(covers_path("t.db"), PathBuf::from("t-covers.db"));
    }

    #[tokio::test]
    async fn put_evicts_least_recently_viewed_until_ninety_percent() {
        let (db, _dir) = open().await;
        let limits = CoverLimits {
            bytes: 1000,
            entries: 5,
        };
        for (index, key) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let evicted = db
                .cover_put("books", key, "image/jpeg", &[0; 100], None, limits)
                .await
                .unwrap();
            assert_eq!(evicted, 0);
            backdate(&db, key, 100 * (index as i64 + 1)).await;
        }
        // 读取刷新访问时间：a 虽然最早写入，却是最近看过的。
        assert_eq!(
            db.cover_get_many("books", &["a".into()])
                .await
                .unwrap()
                .len(),
            1
        );
        let evicted = db
            .cover_put("books", "f", "image/jpeg", &[0; 100], None, limits)
            .await
            .unwrap();
        assert_eq!(evicted, 2);
        assert_eq!(keys(&db).await, ["a", "d", "e", "f"]);
        // 按字节超限同样淘汰到 90%，刚写入的封面不会被淘汰。
        let evicted = db
            .cover_put("books", "g", "image/jpeg", &[0; 900], None, limits)
            .await
            .unwrap();
        assert_eq!(evicted, 4);
        assert_eq!(keys(&db).await, ["g"]);
        // 其他应用不受影响。
        db.cover_put("comics", "a", "image/png", &[1; 10], Some("{}"), limits)
            .await
            .unwrap();
        assert_eq!(
            db.cover_stats("books").await.unwrap(),
            CoverStats {
                entries: 1,
                bytes: 900
            }
        );
        assert_eq!(db.cover_clear("books").await.unwrap(), 1);
        assert_eq!(db.cover_stats("books").await.unwrap().entries, 0);
        assert_eq!(db.cover_stats("comics").await.unwrap().entries, 1);
    }

    #[tokio::test]
    async fn database_backup_snapshot_excludes_covers() {
        let (db, dir) = open().await;
        db.cover_put(
            "books",
            "a",
            "image/jpeg",
            &[0; 10],
            None,
            CoverLimits::default(),
        )
        .await
        .unwrap();
        let snapshot = dir.path().join("snapshot.db");
        db.vacuum_into(snapshot.to_str().unwrap()).await.unwrap();
        let pool = SqlitePool::connect(&format!("sqlite://{}", snapshot.display()))
            .await
            .unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(tables.iter().any(|name| name == "app_data"));
        assert!(!tables.iter().any(|name| name == "covers"));
        assert!(dir.path().join("t-covers.db").is_file());
    }
}
