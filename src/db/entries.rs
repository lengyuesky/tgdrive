//! 大目录元数据分页：按互不重叠的索引范围续读，不使用 OFFSET 或复杂 OR。

use super::{Chunk, Db, Node};
use crate::error::{AppError, AppResult};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Name,
    Size,
    Mtime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    All,
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub limit: u16,
    pub sort: Sort,
    pub order: Order,
    pub kind: Kind,
    pub q: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 200,
            sort: Sort::Name,
            order: Order::Asc,
            kind: Kind::All,
            q: String::new(),
        }
    }
}

fn invalid_cursor() -> AppError {
    AppError::BadRequest("游标无效或与当前目录、查询条件不匹配".into())
}

impl Options {
    pub fn validate(&self) -> AppResult<()> {
        if !(1..=500).contains(&self.limit) {
            return Err(AppError::BadRequest("limit 必须在 1 至 500 之间".into()));
        }
        if self.q.len() > 255 || self.q.chars().any(char::is_control) {
            return Err(AppError::BadRequest(
                "q 最多 255 字节且不能包含控制字符".into(),
            ));
        }
        Ok(())
    }

    fn accepts(&self, is_dir: bool) -> bool {
        match self.kind {
            Kind::All => true,
            Kind::Dir => is_dir,
            Kind::File => !is_dir,
        }
    }

    fn columns(&self) -> Vec<&'static str> {
        let mut columns = match self.sort {
            Sort::Name => vec![],
            Sort::Size => vec!["size"],
            Sort::Mtime => vec!["modified_at"],
        };
        columns.extend(["name COLLATE NOCASE", "name COLLATE BINARY", "id"]);
        columns
    }

    fn index(&self) -> &'static str {
        match self.sort {
            Sort::Name => "idx_nodes_entries_name",
            Sort::Size => "idx_nodes_entries_size",
            Sort::Mtime => "idx_nodes_entries_mtime",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    is_dir: bool,
    name: String,
    id: i64,
    value: Option<i64>,
}

impl Key {
    fn from_node(node: &Node, sort: Sort) -> Self {
        Self {
            is_dir: node.is_dir,
            name: node.name.clone(),
            id: node.id,
            value: match sort {
                Sort::Name => None,
                Sort::Size => Some(node.size),
                Sort::Mtime => Some(node.modified_at),
            },
        }
    }

    fn bind(&self, query: &mut QueryBuilder<Sqlite>, index: usize, columns: usize) {
        if index == columns - 1 {
            query.push_bind(self.id);
        } else if columns == 4 && index == 0 {
            query.push_bind(self.value.expect("游标排序值已经验证"));
        } else {
            query.push_bind(self.name.clone());
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    directory_id: i64,
    options: Options,
    last: Key,
}

impl Cursor {
    fn decode(raw: &str, directory_id: i64, options: &Options) -> AppResult<Self> {
        if raw.is_empty() || raw.len() > 4096 {
            return Err(invalid_cursor());
        }
        let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| invalid_cursor())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
        let key = &cursor.last;
        let valid_name = crate::vfs::normalize(&key.name)
            .is_ok_and(|parts| parts.len() == 1 && parts[0] == key.name);
        if cursor.version != 1
            || cursor.directory_id <= 0
            || cursor.directory_id != directory_id
            || cursor.options != *options
            || key.id <= 0
            || !valid_name
            || !options.accepts(key.is_dir)
            || match options.sort {
                Sort::Name => key.value.is_some(),
                Sort::Size => key.value.is_none_or(|value| value < 0),
                Sort::Mtime => key.value.is_none(),
            }
        {
            return Err(invalid_cursor());
        }
        Ok(cursor)
    }

    fn encode(directory_id: i64, options: &Options, last: &Node) -> AppResult<String> {
        let cursor = Self {
            version: 1,
            directory_id,
            options: options.clone(),
            last: Key::from_node(last, options.sort),
        };
        let bytes = serde_json::to_vec(&cursor).map_err(anyhow::Error::from)?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
}

pub struct Page {
    pub nodes: Vec<Node>,
    pub thumbnails: HashSet<i64>,
    pub next_cursor: Option<String>,
    pub total: Option<i64>,
}

fn filter(query: &mut QueryBuilder<Sqlite>, options: &Options) {
    if !options.q.is_empty() {
        // SQLite lower 只折叠 ASCII；instr 按字面匹配，不把 % 和 _ 当通配符。
        query
            .push(" AND instr(lower(name), lower(")
            .push_bind(options.q.clone())
            .push(")) > 0");
    }
}

/// 每个范围固定前缀、只对下一个键作范围查找；从最深尾键逐段回退。
/// 等值前缀不参与 ORDER BY，避免重复 name 的不同排序规则触发临时排序。
fn range_query(
    directory_id: i64,
    is_dir: bool,
    options: &Options,
    after: Option<(&Key, usize)>,
    limit: usize,
) -> QueryBuilder<Sqlite> {
    let columns = options.columns();
    let mut query = QueryBuilder::new("SELECT * FROM nodes INDEXED BY ");
    query
        .push(options.index())
        .push(" WHERE status = 'ready' AND parent_id = ")
        .push_bind(directory_id)
        .push(" AND is_dir = ")
        .push_bind(is_dir);
    let start = if let Some((key, range)) = after {
        for (index, column) in columns.iter().enumerate().take(range + 1) {
            query.push(" AND ").push(*column).push(if index < range {
                " = "
            } else if options.order == Order::Asc {
                " > "
            } else {
                " < "
            });
            key.bind(&mut query, index, columns.len());
        }
        range
    } else {
        0
    };
    filter(&mut query, options);
    query.push(" ORDER BY ");
    for (index, column) in columns.iter().enumerate().skip(start) {
        if index > start {
            query.push(", ");
        }
        query.push(*column).push(if options.order == Order::Asc {
            " ASC"
        } else {
            " DESC"
        });
    }
    query.push(" LIMIT ").push_bind(limit as i64);
    query
}

impl Db {
    /// 调用者必须先按当前鉴权和规范路径解析目录；游标本身不授予访问权限。
    pub async fn entries_page(
        &self,
        directory_id: i64,
        options: &Options,
        cursor: Option<&str>,
    ) -> AppResult<Page> {
        options.validate()?;
        let cursor = cursor
            .map(|raw| Cursor::decode(raw, directory_id, options))
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        let total = if cursor.is_none() {
            let mut count = QueryBuilder::<Sqlite>::new(
                "SELECT COUNT(*) FROM nodes WHERE status = 'ready' AND parent_id = ",
            );
            count.push_bind(directory_id);
            if options.kind != Kind::All {
                count
                    .push(" AND is_dir = ")
                    .push_bind(options.kind == Kind::Dir);
            }
            filter(&mut count, options);
            Some(
                count
                    .build_query_scalar::<i64>()
                    .fetch_one(&mut *tx)
                    .await?,
            )
        } else {
            None
        };
        let target = usize::from(options.limit) + 1;
        let mut nodes = Vec::with_capacity(target);
        for is_dir in [true, false] {
            if !options.accepts(is_dir) || cursor.as_ref().is_some_and(|c| !c.last.is_dir && is_dir)
            {
                continue;
            }
            let after = cursor.as_ref().filter(|c| c.last.is_dir == is_dir);
            if let Some(cursor) = after {
                for range in (0..options.columns().len()).rev() {
                    if nodes.len() == target {
                        break;
                    }
                    let mut query = range_query(
                        directory_id,
                        is_dir,
                        options,
                        Some((&cursor.last, range)),
                        target - nodes.len(),
                    );
                    nodes.extend(query.build_query_as::<Node>().fetch_all(&mut *tx).await?);
                }
            } else if nodes.len() < target {
                let mut query =
                    range_query(directory_id, is_dir, options, None, target - nodes.len());
                nodes.extend(query.build_query_as::<Node>().fetch_all(&mut *tx).await?);
            }
        }
        let has_more = nodes.len() == target;
        if has_more {
            nodes.pop();
        }
        let next_cursor = if has_more {
            Some(Cursor::encode(
                directory_id,
                options,
                nodes.last().expect("limit 非零"),
            )?)
        } else {
            None
        };
        let mut thumbnails = HashSet::new();
        if !nodes.is_empty() {
            let mut query = QueryBuilder::<Sqlite>::new(
                "SELECT node_id FROM chunks WHERE idx = 0 AND tg_thumbnail_file_id IS NOT NULL AND node_id IN (",
            );
            let mut ids = query.separated(", ");
            for node in &nodes {
                ids.push_bind(node.id);
            }
            ids.push_unseparated(")");
            thumbnails.extend(
                query
                    .build_query_scalar::<i64>()
                    .fetch_all(&mut *tx)
                    .await?,
            );
        }
        tx.commit().await?;
        Ok(Page {
            nodes,
            thumbnails,
            next_cursor,
            total,
        })
    }

    /// 使用 chunks(node_id, idx) 唯一索引读取首块，不加载其余分块。
    pub async fn first_chunk(&self, node_id: i64) -> AppResult<Option<Chunk>> {
        Ok(
            sqlx::query_as("SELECT * FROM chunks WHERE node_id = ? AND idx = 0")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests;
