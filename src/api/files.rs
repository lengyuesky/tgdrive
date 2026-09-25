//! 文件操作：列表、上传、重命名、复制与删除。

use super::*;

// ---------- 文件操作 ----------

#[derive(Serialize)]
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) is_dir: bool,
    pub(super) size: i64,
    pub(super) created_at: i64,
    pub(super) modified_at: i64,
    pub(super) sha256: Option<String>,
    pub(super) favorite: bool,
}

impl From<&Node> for Entry {
    fn from(n: &Node) -> Self {
        Entry {
            name: n.name.clone(),
            is_dir: n.is_dir,
            size: n.size,
            created_at: n.created_at,
            modified_at: n.modified_at,
            sha256: n.sha256.clone(),
            favorite: n.favorite,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct PathQuery {
    pub(super) path: String,
    #[serde(default)]
    pub(super) inline: Option<u8>,
}

pub(super) async fn list(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let nodes = state.vfs.list(&q.path).await?;
    let entries: Vec<Entry> = nodes.iter().map(Entry::from).collect();
    Ok(Json(
        serde_json::json!({ "path": q.path, "entries": entries }),
    ))
}

/// 返回文件或目录属性。目录统计只查询 SQLite 元数据，不读取 Telegram 内容。
pub(super) async fn properties_get(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let node = state.vfs.resolve(&q.path).await?;
    let usage = state.vfs.db.node_usage(node.id).await?;
    let segments = crate::vfs::normalize(&q.path)?;
    let path = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };
    Ok(Json(serde_json::json!({
        "properties": {
            "name": node.name,
            "path": path,
            "is_dir": node.is_dir,
            "size": usage.total_size,
            "created_at": node.created_at,
            "modified_at": node.modified_at,
            "sha256": node.sha256,
            "favorite": node.favorite,
            "file_count": usage.file_count,
            "dir_count": usage.dir_count,
            "chunk_count": usage.chunk_count,
        }
    })))
}

#[derive(Deserialize)]
pub(super) struct PathBody {
    pub(super) path: String,
}

pub(super) async fn mkdir(
    State(state): State<AppState>,
    Json(b): Json<PathBody>,
) -> AppResult<Json<serde_json::Value>> {
    let canon = crate::vfs::canonical(&b.path)?;
    let node = state.vfs.mkdir_p(&canon).await?;
    // 逐级创建时更上层目录也可能新增；父目录加"祖先也相关"的前端规则已覆盖所有受影响范围。
    emit_files(&state, vec![crate::vfs::parent_dir(&canon)]);
    Ok(Json(serde_json::json!({ "ok": true, "name": node.name })))
}

/// 单段文件名校验(multipart 里带路径分隔符一律拒绝)
pub(super) fn clean_filename(raw: &str) -> AppResult<String> {
    let segs = crate::vfs::normalize(raw)?;
    match segs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(AppError::InvalidPath(format!("非法文件名: {raw}"))),
    }
}

#[derive(Deserialize)]
pub(super) struct ExistsBody {
    pub(super) dir: String,
    pub(super) names: Vec<String>,
}

/// 上传前批量查重:返回目录下已存在的名字,前端据此询问覆盖还是跳过。
/// 跳过的文件由前端直接不发请求,大文件零流量
pub(super) async fn exists(
    State(state): State<AppState>,
    Json(b): Json<ExistsBody>,
) -> AppResult<Json<serde_json::Value>> {
    // 目录还不存在(如文件夹上传将要新建的子目录)自然没有重名
    let Ok(dir) = state.vfs.resolve(&b.dir).await else {
        return Ok(Json(serde_json::json!({ "existing": [] })));
    };
    if !dir.is_dir {
        return Err(AppError::NotDirectory);
    }
    let mut existing = Vec::new();
    for raw in &b.names {
        // 名字不合法的留给上传时报错,查重阶段跳过
        let Ok(name) = clean_filename(raw) else {
            continue;
        };
        if state.vfs.db.child(dir.id, &name).await?.is_some() {
            existing.push(raw.clone());
        }
    }
    Ok(Json(serde_json::json!({ "existing": existing })))
}

pub(super) async fn upload(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
    mut mp: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    let dir = state.vfs.resolve(&q.path).await?;
    let mut saved = Vec::new();
    let mut pending_thumbnail: Option<Bytes> = None;
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart 解析失败: {e}")))?
    {
        if field.name() == Some("thumbnail") {
            if pending_thumbnail.is_some() {
                return Err(AppError::BadRequest("同一文件收到多个缩略图".into()));
            }
            let declared = field.content_type().unwrap_or("").to_string();
            let mut data = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(bytes)) => {
                        if data.len() + bytes.len() >= crate::telegram::MAX_THUMBNAIL_SIZE {
                            return Err(AppError::BadRequest(format!(
                                "缩略图必须小于 {}KB",
                                crate::telegram::MAX_THUMBNAIL_SIZE / 1024
                            )));
                        }
                        data.extend_from_slice(&bytes);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(AppError::BadRequest(format!("读取缩略图失败: {e}")));
                    }
                }
            }
            validate_thumbnail_upload(&declared, &data)?;
            pending_thumbnail = Some(Bytes::from(data));
            continue;
        }
        let Some(fname) = field.file_name().map(str::to_string) else {
            continue;
        };
        let name = clean_filename(&fname)?;
        let handle = state
            .vfs
            .start_upload(&dir, &name, pending_thumbnail.take())
            .await?;
        loop {
            match field.chunk().await {
                Ok(Some(bytes)) => handle.feed(bytes).await?,
                Ok(None) => break,
                Err(e) => {
                    // handle 随作用域丢弃 → 管线自动回滚
                    return Err(AppError::BadRequest(format!("读取上传流失败: {e}")));
                }
            }
        }
        let node = handle.finish().await?;
        saved.push(Entry::from(&node));
    }
    if saved.is_empty() {
        return Err(AppError::BadRequest("请求中没有文件".into()));
    }
    if pending_thumbnail.is_some() {
        return Err(AppError::BadRequest("缩略图后缺少对应文件".into()));
    }
    Ok(Json(serde_json::json!({ "saved": saved })))
}

#[derive(Deserialize)]
pub(super) struct FromTo {
    pub(super) from: String,
    pub(super) to: String,
}

pub(super) async fn rename(
    State(state): State<AppState>,
    Json(b): Json<FromTo>,
) -> AppResult<Json<serde_json::Value>> {
    let (from, to) = (
        crate::vfs::canonical(&b.from)?,
        crate::vfs::canonical(&b.to)?,
    );
    state.vfs.rename(&from, &to).await?;
    emit_files(
        &state,
        vec![crate::vfs::parent_dir(&from), crate::vfs::parent_dir(&to)],
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(super) async fn copy_entry(
    State(state): State<AppState>,
    Json(b): Json<FromTo>,
) -> AppResult<Json<serde_json::Value>> {
    let to = crate::vfs::canonical(&b.to)?;
    state.vfs.copy(&b.from, &to).await?;
    emit_files(&state, vec![crate::vfs::parent_dir(&to)]);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct DeleteBody {
    pub(super) paths: Vec<String>,
    /// 默认仍移入回收站，兼容旧版前端及现有 API 调用方。
    #[serde(default)]
    pub(super) permanent: bool,
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Json(b): Json<DeleteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut failed = Vec::new();
    let mut deleted = 0;
    let mut changed = Vec::new();
    for p in &b.paths {
        let result = if b.permanent {
            state
                .vfs
                .delete_permanently(p)
                .await
                .map(|freed| state.vfs.uploader.delete_messages_best_effort(freed))
        } else {
            state.vfs.delete(p).await
        };
        match result {
            Ok(()) => {
                deleted += 1;
                if let Ok(canon) = crate::vfs::canonical(p) {
                    changed.push(crate::vfs::parent_dir(&canon));
                }
            }
            Err(e) => failed.push(serde_json::json!({ "path": p, "error": e.to_string() })),
        }
    }
    if deleted > 0 {
        emit_files(&state, changed);
    }
    Ok(Json(
        serde_json::json!({ "deleted": deleted, "failed": failed }),
    ))
}

pub(super) async fn info(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let (files, dirs, total) = state.vfs.db.stats().await?;
    let c = state.rt.current();
    Ok(Json(serde_json::json!({
        "username": state.auth.username(),
        "api_base": c.api_base,
        "official_api": c.is_official_api(),
        "chunk_size": c.chunk_size,
        "configured": c.is_configured(),
        "files": files,
        "dirs": dirs,
        "total_size": total,
    })))
}
