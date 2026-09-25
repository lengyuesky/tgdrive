//! 回收站。

use super::*;

// ---------- 回收站 ----------

pub(super) async fn trash_index(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let list = state.vfs.db.trash_list().await?;
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|(n, orig, at)| {
            serde_json::json!({
                "id": n.id,
                "name": n.name,
                "is_dir": n.is_dir,
                "size": n.size,
                "orig_path": orig,
                "trashed_at": at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
pub(super) struct TrashBody {
    #[serde(default)]
    pub(super) ids: Vec<i64>,
    #[serde(default)]
    pub(super) all: bool,
}

/// 只接受确实在回收站里的 id,防止借这个口子删任意节点
pub(super) async fn trash_targets(state: &AppState, b: &TrashBody) -> AppResult<Vec<i64>> {
    let in_trash: Vec<i64> = state
        .vfs
        .db
        .trash_list()
        .await?
        .iter()
        .map(|(n, _, _)| n.id)
        .collect();
    if b.all {
        return Ok(in_trash);
    }
    Ok(b.ids
        .iter()
        .copied()
        .filter(|id| in_trash.contains(id))
        .collect())
}

pub(super) async fn trash_restore(
    State(state): State<AppState>,
    Json(b): Json<TrashBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    let mut changed = Vec::new();
    for id in trash_targets(&state, &b).await? {
        match state.vfs.db.trash_restore(id).await {
            Ok(path) => {
                changed.push(crate::vfs::parent_dir(&path));
                restored.push(serde_json::json!({ "id": id, "path": path }));
            }
            Err(e) => failed.push(serde_json::json!({ "id": id, "error": e.to_string() })),
        }
    }
    if !restored.is_empty() {
        emit_files(&state, changed);
    }
    Ok(Json(
        serde_json::json!({ "restored": restored, "failed": failed }),
    ))
}

pub(super) async fn trash_purge(
    State(state): State<AppState>,
    Json(b): Json<TrashBody>,
) -> AppResult<Json<serde_json::Value>> {
    let mut purged = 0;
    for id in trash_targets(&state, &b).await? {
        let freed = state.vfs.db.delete_node(id).await?;
        state.vfs.uploader.delete_messages_best_effort(freed);
        purged += 1;
    }
    // 彻底删除的是回收站里本就不可见的节点：可见树没有变化，不惊动插件重扫。
    Ok(Json(serde_json::json!({ "purged": purged })))
}
