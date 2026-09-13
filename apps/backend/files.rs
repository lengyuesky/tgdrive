//! API v2 的文件元数据能力；节点引用不能绕过当前可见性检查。
use crate::db::apps::FileSearch;
use crate::db::entries::{Kind, Options, Order, Sort};
use crate::{
    api::AppState,
    db::Node,
    error::{AppError, AppResult},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) fn version(node: &Node) -> String {
    let source = format!(
        "{}:{}:{}:{}",
        node.id,
        node.is_dir,
        node.size,
        node.sha256
            .clone()
            .unwrap_or_else(|| node.created_at.to_string())
    );
    crate::engine::upload::to_hex(&Sha256::digest(source.as_bytes()))
}
pub(super) fn metadata(node: &Node, path: &str) -> Value {
    json!({"id":node.id,"path":path,"name":node.name,"is_dir":node.is_dir,"size":node.size,
        "created_at":node.created_at,"modified_at":node.modified_at,"favorite":node.favorite,"content_version":version(node)})
}
pub(super) fn parse<T: serde::de::DeserializeOwned>(value: Value) -> AppResult<T> {
    serde_json::from_value(value).map_err(|_| AppError::BadRequest("应用参数无效".into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {
    pub id: Option<i64>,
    pub path: Option<String>,
    pub content_version: Option<String>,
}
pub(super) async fn resolve(state: &AppState, reference: &Reference) -> AppResult<(Node, String)> {
    let (node, path) = match (&reference.id, &reference.path) {
        (Some(id), None) if *id > 0 && *id <= 9_007_199_254_740_991 => {
            let path = state
                .vfs
                .db
                .full_path_if_visible(*id)
                .await?
                .ok_or(AppError::NotFound)?;
            (state.vfs.db.node(*id).await?, path)
        }
        (None, Some(path)) if path.len() <= 4096 => {
            let path = format!("/{}", crate::vfs::normalize(path)?.join("/"));
            (state.vfs.resolve(&path).await?, path)
        }
        _ => return Err(AppError::BadRequest("需提供唯一的节点 ID 或路径".into())),
    };
    if reference
        .content_version
        .as_ref()
        .is_some_and(|v| *v != version(&node))
    {
        return Err(AppError::Conflict("文件内容已改变，请重新打开".into()));
    }
    Ok((node, path))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    path: String,
    cursor: Option<String>,
    limit: Option<u16>,
}

pub(super) async fn invoke(state: &AppState, method: &str, params: Value) -> AppResult<Value> {
    match method {
        "files.stat" => {
            let (node, path) = resolve(state, &parse(params)?).await?;
            Ok(metadata(&node, &path))
        }
        "files.list" => {
            let q: ListQuery = parse(params)?;
            let (node, path) = resolve(
                state,
                &Reference {
                    id: None,
                    path: Some(q.path),
                    content_version: None,
                },
            )
            .await?;
            if !node.is_dir {
                return Err(AppError::NotDirectory);
            }
            let options = Options {
                limit: q.limit.unwrap_or(200),
                sort: Sort::Name,
                order: Order::Asc,
                kind: Kind::All,
                q: String::new(),
            };
            let page = state
                .vfs
                .db
                .entries_page(node.id, &options, q.cursor.as_deref())
                .await?;
            let entries: Vec<_> = page
                .nodes
                .iter()
                .map(|n| metadata(n, &format!("{}/{}", path.trim_end_matches('/'), n.name)))
                .collect();
            Ok(
                json!({"path":path,"entries":entries,"next_cursor":page.next_cursor,"has_more":page.next_cursor.is_some(),"total":page.total}),
            )
        }
        "files.searchPage" => {
            let mut q: FileSearch = parse(params)?;
            if !(1..=500).contains(&q.limit)
                || q.q.len() > 255
                || q.q.chars().any(char::is_control)
                || !["all", "file", "dir"].contains(&q.kind.as_str())
                || q.extensions.len() > 20
            {
                return Err(AppError::BadRequest("搜索筛选条件无效".into()));
            }
            for ext in &mut q.extensions {
                *ext = ext.trim_start_matches('.').to_ascii_lowercase();
                if ext.is_empty()
                    || ext.len() > 40
                    || !ext
                        .bytes()
                        .all(|v| v.is_ascii_alphanumeric() || b"._+-".contains(&v))
                {
                    return Err(AppError::BadRequest("后缀筛选无效".into()));
                }
            }
            q.extensions.sort();
            q.extensions.dedup();
            let (node, path) = resolve(
                state,
                &Reference {
                    id: None,
                    path: Some(q.under.clone()),
                    content_version: None,
                },
            )
            .await?;
            if !node.is_dir {
                return Err(AppError::NotDirectory);
            }
            q.under = path.clone();
            let (nodes, next) = state.vfs.db.app_search_page(node.id, &path, &q).await?;
            let results: Vec<_> = nodes
                .iter()
                .map(|(node, path)| metadata(node, path))
                .collect();
            Ok(json!({"results":results,"next_cursor":next,"has_more":next.is_some()}))
        }
        _ => Err(AppError::BadRequest("不支持的文件能力".into())),
    }
}
