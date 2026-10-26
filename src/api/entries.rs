//! 新网页专用的有界目录接口；旧列表、分享和 WebDAV 继续使用原条目类型。

use super::{AppState, Entry};
use crate::db::entries::{Kind, Options, Order, Sort};
use crate::error::{AppError, AppResult};
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(super) struct EntriesQuery {
    path: String,
    limit: Option<u16>,
    cursor: Option<String>,
    #[serde(default)]
    sort: Sort,
    #[serde(default)]
    order: Order,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
pub(super) struct DirectoryEntry {
    #[serde(flatten)]
    entry: Entry,
    id: i64,
    content_version: String,
    has_thumbnail: bool,
}

#[derive(Serialize)]
pub(super) struct EntriesResponse {
    path: String,
    entries: Vec<DirectoryEntry>,
    next_cursor: Option<String>,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<i64>,
}

pub(super) async fn entries_get(
    State(state): State<AppState>,
    Query(query): Query<EntriesQuery>,
) -> AppResult<Json<EntriesResponse>> {
    let options = Options {
        limit: query.limit.unwrap_or(200),
        sort: query.sort,
        order: query.order,
        kind: query.kind,
        q: query.q,
    };
    options.validate()?;
    if query.path.len() > 4096 {
        return Err(AppError::BadRequest("path 最多 4096 字节".into()));
    }
    let path = format!("/{}", crate::vfs::normalize(&query.path)?.join("/"));
    // 每一批都重新解析 ready 目录，不以游标里的目录 ID 代替路径权限检查。
    let directory = state.vfs.resolve(&path).await?;
    if !directory.is_dir {
        return Err(AppError::NotDirectory);
    }
    let page = state
        .vfs
        .db
        .entries_page(directory.id, &options, query.cursor.as_deref())
        .await?;
    let entries = page
        .nodes
        .iter()
        .map(|node| DirectoryEntry {
            entry: Entry::from(node),
            id: node.id,
            content_version: node.content_version(),
            has_thumbnail: !node.is_dir && page.thumbnails.contains(&node.id),
        })
        .collect();
    Ok(Json(EntriesResponse {
        path,
        entries,
        has_more: page.next_cursor.is_some(),
        next_cursor: page.next_cursor,
        total: page.total,
    }))
}

#[cfg(test)]
pub(crate) mod tests;
