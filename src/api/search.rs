//! 搜索、收藏与秒传。

use super::*;

// ---------- 搜索 / 秒传 ----------

#[derive(Deserialize, Default)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    extensions: Option<String>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    modified_after: Option<i64>,
    modified_before: Option<i64>,
    #[serde(default)]
    favorite: bool,
    /// 只在该目录子树内搜索；缺省或 `/` 表示整库。
    #[serde(default)]
    pub(crate) under: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    order: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if q.min_size.is_some_and(|size| size < 0) || q.max_size.is_some_and(|size| size < 0) {
        return Err(AppError::BadRequest("文件大小不能为负数".into()));
    }
    if let (Some(min), Some(max)) = (q.min_size, q.max_size)
        && min > max
    {
        return Err(AppError::BadRequest(
            "最小文件大小不能大于最大文件大小".into(),
        ));
    }
    if let (Some(after), Some(before)) = (q.modified_after, q.modified_before)
        && after > before
    {
        return Err(AppError::BadRequest("开始时间不能晚于结束时间".into()));
    }
    let is_dir = match q.kind.as_deref().unwrap_or("all") {
        "all" | "" => None,
        "file" => Some(false),
        "dir" => Some(true),
        _ => return Err(AppError::BadRequest("搜索类型无效".into())),
    };
    let extensions: Vec<String> = q
        .extensions
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect();
    if extensions.len() > 20
        || extensions.iter().any(|ext| {
            ext.len() > 40
                || !ext
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        })
    {
        return Err(AppError::BadRequest("文件后缀筛选格式无效".into()));
    }
    let sort = match q.sort.as_deref().unwrap_or("name") {
        "name" | "" => SearchSort::Name,
        "size" => SearchSort::Size,
        "modified" => SearchSort::Modified,
        "created" => SearchSort::Created,
        _ => return Err(AppError::BadRequest("搜索排序字段无效".into())),
    };
    let descending = match q.order.as_deref().unwrap_or("asc") {
        "asc" | "" => false,
        "desc" => true,
        _ => return Err(AppError::BadRequest("搜索排序方向无效".into())),
    };
    let under = match q.under.as_deref() {
        None => None,
        Some(raw) => {
            let segments = crate::vfs::normalize(raw)?;
            (!segments.is_empty()).then(|| format!("/{}", segments.join("/")))
        }
    };
    let filter = SearchFilter {
        query: q
            .q
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        is_dir,
        extensions,
        min_size: q.min_size,
        max_size: q.max_size,
        modified_after: q.modified_after,
        modified_before: q.modified_before,
        favorite_only: q.favorite,
        under,
        sort,
        descending,
    };
    let hits = state
        .vfs
        .db
        .search_filtered(&filter, q.limit.unwrap_or(500).clamp(1, 2000))
        .await?;
    let results: Vec<serde_json::Value> = hits
        .iter()
        .map(|(n, path)| {
            serde_json::json!({
                "path": path,
                "name": n.name,
                "is_dir": n.is_dir,
                "size": n.size,
                "created_at": n.created_at,
                "modified_at": n.modified_at,
                "favorite": n.favorite,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": results })))
}

#[derive(Deserialize)]
pub(crate) struct FavoriteBody {
    pub(crate) path: String,
    favorite: bool,
}

pub(crate) async fn favorite_set(
    State(state): State<AppState>,
    Json(body): Json<FavoriteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&body.path).await?;
    if node.id == crate::db::ROOT_ID {
        return Err(AppError::BadRequest("不能收藏根目录".into()));
    }
    let updated = state.vfs.db.set_favorite(node.id, body.favorite).await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "favorite": updated.favorite,
    })))
}

#[derive(Deserialize)]
pub(super) struct InstantBody {
    pub(super) dir: String,
    pub(super) name: String,
    pub(super) sha256: String,
    pub(super) size: i64,
}

pub(super) fn normalized_sha256(raw: &str) -> AppResult<String> {
    if raw.len() != 64 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest("sha256 格式不对".into()));
    }
    Ok(raw.to_ascii_lowercase())
}

/// 秒传探测:库里已有同内容文件则零流量落位,否则让前端走真实上传
pub(super) async fn instant(
    State(state): State<AppState>,
    Json(b): Json<InstantBody>,
) -> AppResult<Json<serde_json::Value>> {
    let name = clean_filename(&b.name)?;
    let sha = normalized_sha256(&b.sha256)?;
    if b.size < 0 {
        return Err(AppError::BadRequest("大小无效".into()));
    }
    let dir_path = crate::vfs::canonical(&b.dir)?;
    let dir = state.vfs.resolve(&dir_path).await?;
    if !dir.is_dir {
        return Err(AppError::NotDirectory);
    }
    match state
        .vfs
        .db
        .instant_link(&sha, b.size, dir.id, &name)
        .await?
    {
        Some((id, freed)) => {
            state.vfs.uploader.delete_messages_best_effort(freed);
            let node = state.vfs.db.node(id).await?;
            emit_files(&state, vec![dir_path]);
            Ok(Json(serde_json::json!({
                "linked": true,
                "pending": false,
                "entry": Entry::from(&node),
            })))
        }
        None => {
            let pending = state.vfs.db.uploading_child_exists(dir.id, &name).await?;
            Ok(Json(
                serde_json::json!({ "linked": false, "pending": pending }),
            ))
        }
    }
}
