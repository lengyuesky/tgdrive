//! 宿主阅读索引分页：使用持久化格式索引和数据库递归查询，不把全库节点加载进 App。
#[cfg(test)]
#[path = "reading_tests.rs"]
mod tests;

use super::{Db, Node};
use crate::error::{AppError, AppResult};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadingQuery {
    pub roots: Vec<i64>,
    pub kind: String,
    #[serde(default)]
    pub q: String,
    pub format: Option<String>,
    #[serde(default = "default_sort")]
    pub sort: String,
    #[serde(default = "default_limit")]
    pub limit: u16,
    pub cursor: Option<String>,
}
fn default_sort() -> String {
    "title".into()
}
fn default_limit() -> u16 {
    40
}
impl ReadingQuery {
    pub fn validate(&mut self) -> AppResult<()> {
        self.roots.sort_unstable();
        self.roots.dedup();
        if self.roots.is_empty()
            || self.roots.len() > 16
            || self
                .roots
                .iter()
                .any(|id| *id <= 0 || *id > 9_007_199_254_740_991)
            || !["books", "comics"].contains(&self.kind.as_str())
            || !["title", "added"].contains(&self.sort.as_str())
            || !(1..=200).contains(&self.limit)
            || self.q.len() > 255
            || self.q.chars().any(char::is_control)
            || self.format.as_ref().is_some_and(|f| {
                !(if self.kind == "books" {
                    &["txt", "epub", "pdf"][..]
                } else {
                    &["cbz", "zip", "images"][..]
                })
                .contains(&f.as_str())
            })
        {
            return Err(AppError::BadRequest("阅读索引筛选条件无效".into()));
        }
        self.q = self.q.trim().to_string();
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    revision: String,
    query: ReadingQuery,
    name: String,
    created_at: i64,
    id: i64,
}
#[derive(sqlx::FromRow)]
pub struct ReadingRow {
    #[sqlx(flatten)]
    pub node: Node,
    pub path: String,
    pub format: String,
}
pub struct ReadingPage {
    pub rows: Vec<ReadingRow>,
    pub revision: String,
    pub next_cursor: Option<String>,
}
impl Db {
    pub async fn reading_revision(&self) -> AppResult<String> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM reading_tree_revision WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }
    /// roots 已经逐一按稳定节点及当前目录权限核验；事务保证本页和修订来自同一快照。
    pub async fn reading_page(
        &self,
        query: &ReadingQuery,
        roots: &[(i64, String)],
        checked_revision: &str,
    ) -> AppResult<ReadingPage> {
        let mut context = query.clone();
        context.cursor = None;
        let cursor = query
            .cursor
            .as_ref()
            .map(|raw| -> AppResult<Cursor> {
                if raw.len() > 8192 {
                    return Err(AppError::BadRequest("阅读索引游标过长".into()));
                }
                let bytes = URL_SAFE_NO_PAD
                    .decode(raw)
                    .map_err(|_| AppError::BadRequest("阅读索引游标无效".into()))?;
                let c: Cursor = serde_json::from_slice(&bytes)
                    .map_err(|_| AppError::BadRequest("阅读索引游标无效".into()))?;
                if c.version != 1 || c.query != context || c.id <= 0 || c.name.len() > 4096 {
                    return Err(AppError::BadRequest("阅读索引游标与筛选不匹配".into()));
                }
                Ok(c)
            })
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        let revision: String =
            sqlx::query_scalar("SELECT revision FROM reading_tree_revision WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?;
        if revision != checked_revision || cursor.as_ref().is_some_and(|c| c.revision != revision) {
            return Err(AppError::ReadingIndexChanged);
        }
        let mut sql = QueryBuilder::<Sqlite>::new("WITH RECURSIVE roots(id,path) AS (VALUES ");
        for (index, (id, path)) in roots.iter().enumerate() {
            if index > 0 {
                sql.push(",");
            }
            sql.push("(")
                .push_bind(*id)
                .push(",")
                .push_bind(path)
                .push(")");
        }
        sql.push("), tree(id,path) AS (SELECT n.id,r.path FROM roots r JOIN nodes n ON n.id=r.id WHERE n.status='ready' UNION SELECT n.id,CASE WHEN t.path='/' THEN '/'||n.name ELSE t.path||'/'||n.name END FROM nodes n JOIN tree t ON n.parent_id=t.id WHERE n.status='ready'), candidates(id,format) AS (");
        sql.push("SELECT id,format FROM reading_files WHERE format IN (");
        let formats = if query.kind == "books" {
            &["txt", "epub", "pdf"][..]
        } else {
            &["cbz", "zip"][..]
        };
        let mut separated = sql.separated(",");
        for format in formats {
            separated.push_bind(*format);
        }
        sql.push(")");
        if query.kind == "comics" {
            sql.push(
                " UNION SELECT DISTINCT parent_id,'images' FROM reading_files WHERE format='image'",
            );
        }
        sql.push(") SELECT n.*,t.path,c.format FROM candidates c JOIN nodes n ON n.id=c.id JOIN tree t ON t.id=n.id WHERE n.status='ready'");
        if let Some(format) = &query.format {
            sql.push(" AND c.format=").push_bind(format);
        }
        if !query.q.is_empty() {
            sql.push(" AND instr(lower(n.name),lower(")
                .push_bind(&query.q)
                .push("))>0");
        }
        if let Some(cursor) = &cursor {
            if query.sort == "added" {
                sql.push(" AND (n.created_at < ")
                    .push_bind(cursor.created_at)
                    .push(" OR (n.created_at=")
                    .push_bind(cursor.created_at)
                    .push(" AND n.id>")
                    .push_bind(cursor.id)
                    .push("))");
            } else {
                sql.push(" AND (n.name COLLATE NOCASE > ")
                    .push_bind(&cursor.name)
                    .push(" OR (n.name COLLATE NOCASE = ")
                    .push_bind(&cursor.name)
                    .push(" AND (n.name COLLATE BINARY > ")
                    .push_bind(&cursor.name)
                    .push(" OR (n.name COLLATE BINARY = ")
                    .push_bind(&cursor.name)
                    .push(" AND n.id>")
                    .push_bind(cursor.id)
                    .push("))))");
            }
        }
        if query.sort == "added" {
            sql.push(" ORDER BY n.created_at DESC,n.id");
        } else {
            sql.push(" ORDER BY n.name COLLATE NOCASE,n.name COLLATE BINARY,n.id");
        }
        sql.push(" LIMIT ").push_bind(i64::from(query.limit) + 1);
        let mut rows: Vec<ReadingRow> = sql.build_query_as().fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(query.limit);
        rows.truncate(usize::from(query.limit));
        let next_cursor = if more {
            let last = &rows.last().unwrap().node;
            Some(
                URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&Cursor {
                        version: 1,
                        revision: revision.clone(),
                        query: context,
                        name: last.name.clone(),
                        created_at: last.created_at,
                        id: last.id,
                    })
                    .map_err(anyhow::Error::from)?,
                ),
            )
        } else {
            None
        };
        tx.commit().await?;
        Ok(ReadingPage {
            rows,
            revision,
            next_cursor,
        })
    }
}
