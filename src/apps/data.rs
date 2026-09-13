//! 有界的应用私有数据与文件子树分页，所有写入在事务中重新校验应用修订版。
use super::{Db, now_ts, random_token};
use crate::db::Node;
use crate::error::{AppError, AppResult};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{QueryBuilder, Sqlite};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSearch {
    #[serde(default = "root_path")]
    pub under: String,
    #[serde(default)]
    pub q: String,
    #[serde(default = "file_kind")]
    pub kind: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default = "page_limit")]
    pub limit: u16,
    pub cursor: Option<String>,
}
fn root_path() -> String {
    "/".into()
}
fn file_kind() -> String {
    "file".into()
}
fn page_limit() -> u16 {
    200
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchCursor {
    scope: i64,
    query: FileSearch,
    name: String,
    id: i64,
}

#[derive(sqlx::FromRow)]
struct FileRow {
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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DataRecord {
    pub key: String,
    pub value: String,
    pub revision: String,
    pub updated_at: i64,
}
impl DataRecord {
    pub fn json(&self) -> AppResult<Value> {
        Ok(
            serde_json::json!({"key": self.key, "value": serde_json::from_str::<Value>(&self.value).map_err(anyhow::Error::from)?, "revision": self.revision, "updated_at": self.updated_at}),
        )
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DataCursor {
    prefix: String,
    key: String,
    at: i64,
}

fn cursor_error() -> AppError {
    AppError::BadRequest("游标无效或与当前查询不匹配".into())
}
fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> AppResult<T> {
    if raw.len() > 8192 {
        return Err(cursor_error());
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(raw).map_err(|_| cursor_error())?)
        .map_err(|_| cursor_error())
}
fn encode(value: &impl Serialize) -> AppResult<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).map_err(anyhow::Error::from)?))
}

impl Db {
    pub async fn app_search_page(
        &self,
        scope: i64,
        path: &str,
        query: &FileSearch,
    ) -> AppResult<(Vec<(Node, String)>, Option<String>)> {
        let mut identity = query.clone();
        identity.cursor = None;
        let after = query
            .cursor
            .as_deref()
            .map(decode::<SearchCursor>)
            .transpose()?;
        if after.as_ref().is_some_and(|c| {
            c.scope != scope || c.query != identity || c.id <= 0 || c.name.len() > 255
        }) {
            return Err(cursor_error());
        }
        let mut sql = QueryBuilder::<Sqlite>::new("WITH RECURSIVE visible AS (SELECT nodes.*, ");
        sql.push_bind(if path == "/" { "" } else { path }).push(" AS path FROM nodes WHERE id = ").push_bind(scope)
            .push(" AND status = 'ready' UNION ALL SELECT n.*, v.path || '/' || n.name FROM nodes n JOIN visible v ON n.parent_id = v.id WHERE n.status = 'ready') SELECT * FROM visible WHERE id != ").push_bind(scope);
        if query.kind != "all" {
            sql.push(" AND is_dir = ").push_bind(query.kind == "dir");
        }
        if !query.q.is_empty() {
            sql.push(" AND instr(lower(name), lower(")
                .push_bind(&query.q)
                .push(")) > 0");
        }
        if !query.extensions.is_empty() {
            sql.push(" AND (is_dir = 1 OR (");
            for (i, extension) in query.extensions.iter().enumerate() {
                if i > 0 {
                    sql.push(" OR ");
                }
                sql.push("lower(name) GLOB ")
                    .push_bind(format!("*.{extension}"));
            }
            sql.push("))");
        }
        if let Some(c) = &after {
            sql.push(" AND (name COLLATE NOCASE, name COLLATE BINARY, id) > (")
                .push_bind(&c.name)
                .push(", ")
                .push_bind(&c.name)
                .push(", ")
                .push_bind(c.id)
                .push(")");
        }
        sql.push(" ORDER BY name COLLATE NOCASE, name COLLATE BINARY, id LIMIT ")
            .push_bind(i64::from(query.limit) + 1);
        let mut rows: Vec<FileRow> = sql.build_query_as().fetch_all(&self.pool).await?;
        let next = if rows.len() > usize::from(query.limit) {
            rows.pop();
            let last = rows.last().expect("分页上限非零");
            Some(encode(&SearchCursor {
                scope,
                query: identity,
                name: last.name.clone(),
                id: last.id,
            })?)
        } else {
            None
        };
        Ok((
            rows.into_iter()
                .map(|r| {
                    (
                        Node {
                            id: r.id,
                            parent_id: r.parent_id,
                            name: r.name,
                            is_dir: r.is_dir,
                            size: r.size,
                            status: r.status,
                            created_at: r.created_at,
                            modified_at: r.modified_at,
                            sha256: r.sha256,
                            favorite: r.favorite,
                        },
                        r.path,
                    )
                })
                .collect(),
            next,
        ))
    }

    pub async fn app_data_get(&self, app: &str, key: &str) -> AppResult<Option<DataRecord>> {
        Ok(sqlx::query_as(
            "SELECT key, value, revision, updated_at FROM app_data WHERE app_id = ? AND key = ?",
        )
        .bind(app)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn app_data_list(
        &self,
        app: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: u16,
    ) -> AppResult<Value> {
        let after = cursor.map(decode::<DataCursor>).transpose()?;
        if after.as_ref().is_some_and(|c| c.prefix != prefix) {
            return Err(cursor_error());
        }
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT key, value, revision, updated_at FROM app_data WHERE app_id = ",
        );
        sql.push_bind(app)
            .push(" AND substr(key, 1, ")
            .push_bind(prefix.len() as i64)
            .push(") = ")
            .push_bind(prefix);
        if let Some(c) = &after {
            sql.push(" AND (updated_at < ")
                .push_bind(c.at)
                .push(" OR (updated_at = ")
                .push_bind(c.at)
                .push(" AND key > ")
                .push_bind(&c.key)
                .push("))");
        }
        sql.push(" ORDER BY updated_at DESC, key LIMIT ")
            .push_bind(i64::from(limit) + 1);
        let mut rows: Vec<DataRecord> = sql.build_query_as().fetch_all(&self.pool).await?;
        let next = if rows.len() > usize::from(limit) {
            rows.pop();
            let last = rows.last().expect("分页上限非零");
            Some(encode(&DataCursor {
                prefix: prefix.into(),
                key: last.key.clone(),
                at: last.updated_at,
            })?)
        } else {
            None
        };
        let records = rows
            .iter()
            .map(DataRecord::json)
            .collect::<AppResult<Vec<_>>>()?;
        Ok(serde_json::json!({"records":records, "next_cursor":next, "has_more":next.is_some()}))
    }

    pub async fn app_data_write(
        &self,
        app: &str,
        app_revision: &str,
        key: &str,
        value: Option<&str>,
        expected: Option<&str>,
    ) -> AppResult<Option<DataRecord>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<(String, bool)> =
            sqlx::query_as("SELECT revision, enabled FROM apps WHERE id = ?")
                .bind(app)
                .fetch_optional(&mut *tx)
                .await?;
        if !active.is_some_and(|(revision, enabled)| enabled && revision == app_revision) {
            return Err(AppError::Conflict(
                "应用已停用、卸载或更新，请重新打开".into(),
            ));
        }
        let old: Option<(String, i64)> = sqlx::query_as("SELECT revision, length(CAST(value AS BLOB)) FROM app_data WHERE app_id = ? AND key = ?").bind(app).bind(key).fetch_optional(&mut *tx).await?;
        if old.as_ref().map(|v| v.0.as_str()) != expected {
            return Err(AppError::AppDataConflict);
        }
        let result = if let Some(value) = value {
            let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(length(CAST(value AS BLOB)) + length(key)), 0) FROM app_data WHERE app_id = ?").bind(app).fetch_one(&mut *tx).await?;
            let extra = if old.is_none() { key.len() as i64 } else { 0 };
            if (old.is_none() && count >= 10000)
                || bytes - old.as_ref().map_or(0, |v| v.1) + value.len() as i64 + extra
                    > 8 * 1024 * 1024
            {
                return Err(AppError::BadRequest(
                    "应用数据已达到 10000 条或 8 MiB 上限，请清理旧书签和记录".into(),
                ));
            }
            let record = DataRecord {
                key: key.into(),
                value: value.into(),
                revision: random_token(32),
                updated_at: now_ts(),
            };
            sqlx::query("INSERT INTO app_data(app_id,key,value,revision,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(app_id,key) DO UPDATE SET value=excluded.value, revision=excluded.revision, updated_at=excluded.updated_at")
                .bind(app).bind(key).bind(value).bind(&record.revision).bind(record.updated_at).execute(&mut *tx).await?;
            Some(record)
        } else {
            sqlx::query("DELETE FROM app_data WHERE app_id = ? AND key = ?")
                .bind(app)
                .bind(key)
                .execute(&mut *tx)
                .await?;
            None
        };
        tx.commit().await?;
        Ok(result)
    }
}
