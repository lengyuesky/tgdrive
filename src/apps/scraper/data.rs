//! 预览条件、整组移动与资料提交都在 SQLite 写事务内核对。

use super::{APP_ID, backend, types::*};
use crate::{
    db::{Db, Node, ROOT_ID, now_ts, random_token},
    error::{AppError, AppResult},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, Transaction};
use std::sync::LazyLock;

static GENERATION: LazyLock<String> = LazyLock::new(|| random_token(32));
pub(super) fn generation() -> &'static str {
    &GENERATION
}
pub(super) fn decode<T: serde::de::DeserializeOwned>(text: &str) -> AppResult<T> {
    serde_json::from_str(text).map_err(|e| AppError::Internal(e.into()))
}
pub(super) fn encode<T: serde::Serialize>(value: &T) -> AppResult<String> {
    serde_json::to_string(value).map_err(|e| AppError::Internal(e.into()))
}
fn changed() -> AppError {
    AppError::Conflict("预览后的文件、位置或资料集合已变化，请重新扫描并预览".into())
}

pub(super) async fn node(tx: &mut Transaction<'_, Sqlite>, id: i64) -> AppResult<Node> {
    sqlx::query_as("SELECT * FROM nodes WHERE id=? AND status='ready'")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(changed)
}
pub(super) async fn path(tx: &mut Transaction<'_, Sqlite>, id: i64) -> AppResult<String> {
    let rows:Vec<(i64,Option<i64>,String,bool,String)> = sqlx::query_as("WITH RECURSIVE chain(id,parent_id,name,is_dir,status,depth) AS (SELECT id,parent_id,name,is_dir,status,0 FROM nodes WHERE id=? UNION ALL SELECT n.id,n.parent_id,n.name,n.is_dir,n.status,c.depth+1 FROM nodes n JOIN chain c ON n.id=c.parent_id WHERE c.depth<4096) SELECT id,parent_id,name,is_dir,status FROM chain ORDER BY depth DESC")
        .bind(id).fetch_all(&mut **tx).await?;
    if rows.first().is_none_or(|r| r.0 != ROOT_ID || r.1.is_some())
        || rows.iter().any(|r| r.4 != "ready")
        || rows.iter().take(rows.len().saturating_sub(1)).any(|r| !r.3)
    {
        return Err(changed());
    }
    Ok(format!(
        "/{}",
        rows.iter()
            .skip(1)
            .map(|r| r.2.as_str())
            .collect::<Vec<_>>()
            .join("/")
    ))
}
pub(super) async fn children(
    tx: &mut Transaction<'_, Sqlite>,
    parent: i64,
) -> AppResult<Vec<Node>> {
    Ok(
        sqlx::query_as("SELECT * FROM nodes WHERE parent_id=? AND status='ready' ORDER BY id")
            .bind(parent)
            .fetch_all(&mut **tx)
            .await?,
    )
}
pub(super) async fn watched(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &Directory,
    base: &str,
    extra: &[String],
) -> AppResult<Vec<Snapshot>> {
    let mut values: Vec<_> = children(tx, folder.id)
        .await?
        .into_iter()
        .filter(|n| {
            related(&n.name, base)
                || is_video(n) && stem(&n.name).eq_ignore_ascii_case(base)
                || extra.iter().any(|name| name.eq_ignore_ascii_case(&n.name))
        })
        .map(|n| Snapshot::from_node(&n, joined(&folder.path, &n.name)))
        .collect();
    values.sort_by_key(|s| s.id);
    Ok(values)
}
async fn check_directory(tx: &mut Transaction<'_, Sqlite>, dir: &Directory) -> AppResult<()> {
    if !node(tx, dir.id).await?.is_dir || path(tx, dir.id).await? != dir.path {
        return Err(changed());
    }
    Ok(())
}
pub(super) async fn config_key(tx: &mut Transaction<'_, Sqlite>) -> AppResult<String> {
    let settings: Option<String> =
        sqlx::query_scalar("SELECT settings FROM app_settings WHERE app_id=?")
            .bind(APP_ID)
            .fetch_optional(&mut **tx)
            .await?;
    let settings: Value = decode(settings.as_deref().unwrap_or("{}"))?;
    let url = backend::base_url(
        settings["backend_url"]
            .as_str()
            .unwrap_or(backend::DEFAULT_URL),
    )?
    .to_string();
    let secret: Option<String> =
        sqlx::query_scalar("SELECT secret_generation FROM app_scraper_state WHERE app_id=?")
            .bind(APP_ID)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(crate::engine::upload::to_hex(&Sha256::digest(
        format!("{url}:{}", secret.unwrap_or_default()).as_bytes(),
    )))
}
pub(super) async fn authorize(
    tx: &mut Transaction<'_, Sqlite>,
    revision: &str,
    paths: &[&str],
) -> AppResult<()> {
    let record: Option<(String, bool, String, String)> =
        sqlx::query_as("SELECT revision,enabled,scope_mode,scope_paths FROM apps WHERE id=?")
            .bind(APP_ID)
            .fetch_optional(&mut **tx)
            .await?;
    let (rev, enabled, mode, scopes) = record.ok_or(AppError::AppChanged)?;
    if rev != revision || !enabled {
        return Err(AppError::AppChanged);
    }
    let scopes: Vec<String> = decode(&scopes)?;
    if mode == "none"
        || paths
            .iter()
            .any(|p| !crate::apps::scope::in_scope(&scopes, p))
    {
        return Err(AppError::Forbidden(
            "刮削目录授权已改变，请重新选择目录".into(),
        ));
    }
    Ok(())
}
pub(super) async fn batch(tx: &mut Transaction<'_, Sqlite>, id: &str) -> AppResult<Batch> {
    sqlx::query_as("SELECT * FROM app_scraper_batches WHERE id=? AND app_id=?")
        .bind(id)
        .bind(APP_ID)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}
pub(super) async fn active(tx: &mut Transaction<'_, Sqlite>, id: &str) -> AppResult<Batch> {
    let batch = batch(tx, id).await?;
    if batch.status != "running" || batch.generation != generation() || batch.expires_at <= now_ts()
    {
        return Err(AppError::Conflict(
            "刮削批次已停止或连接中断，请核实结果后手动继续".into(),
        ));
    }
    authorize(tx, &batch.revision, &[&batch.root_path]).await?;
    if config_key(tx).await? != batch.config_key {
        return Err(AppError::Conflict(
            "后端配置已改变，请重新预览未完成项".into(),
        ));
    }
    if path(tx, batch.root_id).await? != batch.root_path {
        return Err(changed());
    }
    Ok(batch)
}
pub(super) async fn check_plan(
    tx: &mut Transaction<'_, Sqlite>,
    batch: &Batch,
    plan: &Plan,
) -> AppResult<()> {
    check_directory(tx, &plan.root).await?;
    check_directory(tx, &plan.folder).await?;
    if plan.root.id != batch.root_id || plan.root.path != batch.root_path {
        return Err(changed());
    }
    authorize(tx, &batch.revision, &[&plan.folder.path, &plan.target_path]).await?;
    if watched(tx, &plan.folder, &plan.stem, &plan.extra_names).await? != plan.watched {
        return Err(changed());
    }
    for file in plan.videos.iter().chain(plan.moving.iter()) {
        let current = node(tx, file.id).await?;
        if Snapshot::from_node(&current, path(tx, file.id).await?) != *file {
            return Err(changed());
        }
    }
    Ok(())
}
pub(super) async fn move_group(
    db: &Db,
    batch_id: &str,
    item_id: &str,
    mut plan: Plan,
) -> AppResult<Plan> {
    db.require_background_enabled()?;
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let batch = active(&mut tx, batch_id).await?;
    adjust_own_moves(&mut tx, batch_id, item_id, &mut plan).await?;
    check_plan(&mut tx, &batch, &plan).await?;
    let mut result: ResultRecord = decode(
        &sqlx::query_scalar::<_, String>(
            "SELECT result FROM app_scraper_items WHERE id=? AND batch_id=?",
        )
        .bind(item_id)
        .bind(batch_id)
        .fetch_one(&mut *tx)
        .await?,
    )?;
    if result.organize.state == "done" {
        tx.commit().await?;
        return Ok(plan);
    }
    let target = if plan.target_path == plan.folder.path {
        plan.folder.id
    } else {
        let name = plan.target_path.rsplit('/').next().ok_or_else(changed)?;
        let existing: Option<Node> =
            sqlx::query_as("SELECT * FROM nodes WHERE parent_id=? AND name=? AND status='ready'")
                .bind(plan.folder.id)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        match existing {
            Some(dir) => {
                if !dir.is_dir {
                    return Err(changed());
                }
                let own: Option<i64> = sqlx::query_scalar(
                    "SELECT node_id FROM app_scraper_directories WHERE batch_id=? AND path=?",
                )
                .bind(batch_id)
                .bind(&plan.target_path)
                .fetch_optional(&mut *tx)
                .await?;
                if Some(dir.id) != plan.target_id && Some(dir.id) != own {
                    return Err(changed());
                }
                let target_dir = Directory {
                    id: dir.id,
                    path: plan.target_path.clone(),
                };
                check_directory(&mut tx, &target_dir).await?;
                if watched(&mut tx, &target_dir, &plan.stem, &plan.extra_names).await?
                    != plan.target_watched
                {
                    return Err(changed());
                }
                dir.id
            }
            None => {
                if plan.target_id.is_some() {
                    return Err(changed());
                }
                let now = now_ts();
                let id=sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,1,0,'ready',?,?)").bind(plan.folder.id).bind(name).bind(now).bind(now).execute(&mut *tx).await?.last_insert_rowid();
                sqlx::query(
                    "INSERT INTO app_scraper_directories(batch_id,path,node_id) VALUES(?,?,?)",
                )
                .bind(batch_id)
                .bind(&plan.target_path)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                id
            }
        }
    };
    if target != plan.folder.id {
        for file in &plan.moving {
            let conflict:i64=sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=? AND status IN ('ready','uploading')").bind(target).bind(&file.name).fetch_one(&mut *tx).await?;
            if conflict != 0 {
                return Err(changed());
            }
        }
        for file in &plan.moving {
            sqlx::query("UPDATE nodes SET parent_id=?,modified_at=? WHERE id=? AND status='ready'")
                .bind(target)
                .bind(now_ts())
                .bind(file.id)
                .execute(&mut *tx)
                .await?;
        }
        for file in &mut plan.videos {
            file.parent_id = Some(target);
            file.path = joined(&plan.target_path, &file.name);
        }
        for artifact in [&mut plan.nfo, &mut plan.poster] {
            if let Some(file) = &mut artifact.existing
                && plan.moving.iter().any(|m| m.id == file.id)
            {
                file.parent_id = Some(target);
                file.path = joined(&plan.target_path, &file.name);
            }
        }
    }
    plan.folder = Directory {
        id: target,
        path: plan.target_path.clone(),
    };
    plan.target_id = Some(target);
    plan.moving.clear();
    plan.watched = watched(&mut tx, &plan.folder, &plan.stem, &plan.extra_names).await?;
    plan.target_watched = plan.watched.clone();
    result.organize.state = "done".into();
    if result.nfo.state == "kept" {
        result.nfo.file = plan.nfo.existing.clone();
    }
    if result.poster.state == "kept" {
        result.poster.file = plan.poster.existing.clone();
    }
    sqlx::query("UPDATE app_scraper_items SET plan=?,result=? WHERE id=? AND batch_id=?")
        .bind(encode(&plan)?)
        .bind(encode(&result)?)
        .bind(item_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(plan)
}

pub(super) async fn new_upload(
    db: &Db,
    batch_id: &str,
    item_id: &str,
    plan: &Plan,
    role: &str,
    content: UploadContent<'_>,
    target: i64,
) -> AppResult<crate::db::UploadSessionInfo> {
    let UploadContent { size, sha256: sha } = content;
    db.require_background_enabled()?;
    let artifact = if role == "nfo" {
        &plan.nfo
    } else {
        &plan.poster
    };
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let batch = active(&mut tx, batch_id).await?;
    check_plan(&mut tx, &batch, plan).await?;
    if other_video_uses(
        &artifact.name,
        &plan.stem,
        plan.watched
            .iter()
            .filter(|f| !f.is_dir && video_name(&f.name))
            .map(|f| f.name.as_str()),
    ) {
        return Err(AppError::Conflict("资料共享范围已变化，请重新预览".into()));
    }
    let current: Option<Node> =
        sqlx::query_as("SELECT * FROM nodes WHERE parent_id=? AND name=? AND status='ready'")
            .bind(plan.folder.id)
            .bind(&artifact.name)
            .fetch_optional(&mut *tx)
            .await?;
    let current = current.map(|n| Snapshot::from_node(&n, joined(&plan.folder.path, &n.name)));
    if current != artifact.existing {
        return Err(changed());
    }
    let busy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=? AND status='uploading'",
    )
    .bind(plan.folder.id)
    .bind(&artifact.name)
    .fetch_one(&mut *tx)
    .await?;
    if busy != 0 {
        return Err(AppError::Conflict(
            "目标资料已有未完成上传，请先核实或取消该上传".into(),
        ));
    }
    let now = now_ts();
    let id = random_token(32);
    let node_id=sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,0,0,'uploading',?,?)").bind(plan.folder.id).bind(&artifact.name).bind(now).bind(now).execute(&mut *tx).await?.last_insert_rowid();
    let display = joined(&plan.folder.path, &artifact.name);
    sqlx::query("INSERT INTO upload_sessions(id,node_id,display_path,expected_size,sha256,chunk_size,storage_target_id,created_at,updated_at,overwrite) VALUES(?,?,?,?,?,?,?,?,?,?)")
        .bind(&id).bind(node_id).bind(&display).bind(size).bind(sha).bind(2*1024*1024i64).bind(target).bind(now).bind(now).bind(artifact.action=="replace").execute(&mut *tx).await?;
    let guard = WriteGuard {
        root: plan.root.clone(),
        folder: plan.folder.clone(),
        watched: plan.watched.clone(),
        extra_names: plan.extra_names.clone(),
        stem: plan.stem.clone(),
        expected: artifact.existing.clone(),
        name: artifact.name.clone(),
    };
    sqlx::query("INSERT INTO app_scraper_upload_guards(node_id,batch_id,item_id,role,guard) VALUES(?,?,?,?,?)").bind(node_id).bind(batch_id).bind(item_id).bind(role).bind(encode(&guard)?).execute(&mut *tx).await?;
    let raw: String = sqlx::query_scalar("SELECT result FROM app_scraper_items WHERE id=?")
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
    let mut result: ResultRecord = decode(&raw)?;
    let step = if role == "nfo" {
        &mut result.nfo
    } else {
        &mut result.poster
    };
    step.state = "uploading".into();
    step.upload_id = Some(id.clone());
    step.sha256 = Some(sha.into());
    let shadow: Node = sqlx::query_as("SELECT * FROM nodes WHERE id=?")
        .bind(node_id)
        .fetch_one(&mut *tx)
        .await?;
    step.file = Some(Snapshot::from_node(&shadow, display));
    sqlx::query("UPDATE app_scraper_items SET result=? WHERE id=?")
        .bind(encode(&result)?)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    db.upload_session_info(&id).await
}

pub(crate) async fn check_upload_guard(
    db: &Db,
    tx: &mut Transaction<'_, Sqlite>,
    node: &Node,
) -> AppResult<()> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT batch_id,guard FROM app_scraper_upload_guards WHERE node_id=?")
            .bind(node.id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((batch_id, raw)) = row else {
        return Ok(());
    };
    db.require_background_enabled()?;
    let batch = active(tx, &batch_id).await?;
    let guard: WriteGuard = decode(&raw)?;
    check_directory(tx, &guard.root).await?;
    check_directory(tx, &guard.folder).await?;
    authorize(tx, &batch.revision, &[&guard.folder.path]).await?;
    if other_video_uses(
        &guard.name,
        &guard.stem,
        guard
            .watched
            .iter()
            .filter(|f| !f.is_dir && video_name(&f.name))
            .map(|f| f.name.as_str()),
    ) {
        return Err(changed());
    }
    if guard.root.id != batch.root_id
        || guard.root.path != batch.root_path
        || node.parent_id != Some(guard.folder.id)
        || node.name != guard.name
        || watched(tx, &guard.folder, &guard.stem, &guard.extra_names).await? != guard.watched
    {
        return Err(changed());
    }
    let old: Option<Node> =
        sqlx::query_as("SELECT * FROM nodes WHERE parent_id=? AND name=? AND status='ready'")
            .bind(guard.folder.id)
            .bind(&guard.name)
            .fetch_optional(&mut **tx)
            .await?;
    if old.map(|n| Snapshot::from_node(&n, joined(&guard.folder.path, &n.name))) != guard.expected {
        return Err(changed());
    }
    Ok(())
}

pub(crate) async fn committed_upload(
    tx: &mut Transaction<'_, Sqlite>,
    node: &Node,
) -> AppResult<()> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT item_id,role,guard FROM app_scraper_upload_guards WHERE node_id=?")
            .bind(node.id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((item_id, role, raw)) = row else {
        return Ok(());
    };
    let guard: WriteGuard = decode(&raw)?;
    let (plan, result): (String, String) =
        sqlx::query_as("SELECT plan,result FROM app_scraper_items WHERE id=?")
            .bind(&item_id)
            .fetch_one(&mut **tx)
            .await?;
    let mut plan: Plan = decode(&plan)?;
    let mut result: ResultRecord = decode(&result)?;
    let file = Snapshot::from_node(node, joined(&guard.folder.path, &node.name));
    let step = if role == "nfo" {
        &mut result.nfo
    } else {
        &mut result.poster
    };
    step.state = "done".into();
    step.file = Some(file.clone());
    let artifact = if role == "nfo" {
        &mut plan.nfo
    } else {
        &mut plan.poster
    };
    artifact.existing = Some(file);
    artifact.action = "keep".into();
    plan.watched = watched(tx, &guard.folder, &plan.stem, &plan.extra_names).await?;
    plan.target_watched = plan.watched.clone();
    sqlx::query("UPDATE app_scraper_items SET plan=?,result=? WHERE id=?")
        .bind(encode(&plan)?)
        .bind(encode(&result)?)
        .bind(&item_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM app_scraper_upload_guards WHERE node_id=?")
        .bind(node.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn recover_interrupted(pool: &sqlx::SqlitePool) -> AppResult<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE app_scraper_items SET status='needs_verification' WHERE status='processing' AND batch_id IN (SELECT id FROM app_scraper_batches WHERE generation!=? OR expires_at<=?)")
        .bind(generation()).bind(now_ts()).execute(&mut *tx).await?;
    sqlx::query("UPDATE app_scraper_batches SET status='interrupted',owner='',expires_at=0,message='连接或宿主已中断，请核实结果后重新扫描' WHERE status IN ('running','stopping') AND (generation!=? OR expires_at<=?)")
        .bind(generation()).bind(now_ts()).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM app_scraper_batches WHERE app_id=? AND status NOT IN ('running','stopping') AND updated_at<?").bind(APP_ID).bind(now_ts()-30*86400).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn invalidate_restored(conn: &mut sqlx::SqliteConnection) -> AppResult<()> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='app_scraper_batches'",
    )
    .fetch_one(&mut *conn)
    .await?;
    if exists == 0 {
        return Ok(());
    }
    sqlx::query("UPDATE app_scraper_batches SET read_only=1")
        .execute(&mut *conn)
        .await?;
    discard_restored_uploads(conn).await?;
    sqlx::query("UPDATE app_scraper_state SET secret_generation=''")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE app_scraper_items SET status='needs_verification' WHERE status='processing'",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE app_scraper_batches SET status='interrupted',owner='',expires_at=0,message='恢复记录仅供核实，请重新扫描' WHERE status IN ('draft','running','stopping','paused')").execute(&mut *conn).await?;
    Ok(())
}

/// 恢复只丢弃尚未提交的资料影子；不撤回 ready 文件或删除备份外的 Telegram 附件。
pub(super) async fn discard_restored_uploads(conn: &mut sqlx::SqliteConnection) -> AppResult<()> {
    for role in ["nfo", "poster"] {
        let state_path = format!("$.{role}.state");
        sqlx::query("UPDATE app_scraper_items SET result=json_set(result,?,'cancelled',?,NULL) WHERE json_extract(result,?)='uploading'")
            .bind(&state_path).bind(format!("$.{role}.file")).bind(&state_path).execute(&mut *conn).await?;
    }
    sqlx::query("DELETE FROM nodes WHERE status='uploading' AND id IN(SELECT node_id FROM app_scraper_upload_guards)")
        .execute(&mut *conn).await?;
    Ok(())
}

pub(super) fn plan_value(item_id: &str, plan: &Plan) -> Value {
    json!({"id":item_id,"files":plan.videos,"target_path":plan.target_path,"moving":plan.moving,"nfo":plan.nfo,"poster":plan.poster,"candidate":plan.candidate,"needs_candidate":plan.needs_candidate,"ready":!plan.needs_candidate})
}

/// 只接纳同一批次已确认移动的相同节点及内容，外部新增或修改仍使预览失效。
async fn adjust_own_moves(
    tx: &mut Transaction<'_, Sqlite>,
    batch_id: &str,
    item_id: &str,
    plan: &mut Plan,
) -> AppResult<()> {
    if plan.folder.path == plan.target_path {
        return Ok(());
    }
    let raws:Vec<String>=sqlx::query_scalar("SELECT plan FROM app_scraper_items WHERE batch_id=? AND id!=? AND json_extract(result,'$.organize.state')='done' AND json_extract(plan,'$.folder.path')=?")
        .bind(batch_id).bind(item_id).bind(&plan.target_path).fetch_all(&mut **tx).await?;
    let mut known = std::collections::HashMap::new();
    for raw in raws {
        let prior: Plan = decode(&raw)?;
        for file in prior.watched {
            if let Ok(current) = node(tx, file.id).await
                && Snapshot::from_node(&current, path(tx, file.id).await?) == file
            {
                known.insert(file.id, file);
            }
        }
    }
    if known.is_empty() {
        return Ok(());
    }
    let current = watched(tx, &plan.folder, &plan.stem, &plan.extra_names).await?;
    let mut moved = std::collections::HashSet::new();
    for expected in &plan.watched {
        if current.iter().any(|file| file == expected) {
            continue;
        }
        if let Some(actual) = known.get(&expected.id)
            && actual.content_version == expected.content_version
            && actual.name == expected.name
            && actual.size == expected.size
            && actual.is_dir == expected.is_dir
        {
            moved.insert(expected.id);
            continue;
        }
        return Err(changed());
    }
    let retained: Vec<_> = plan
        .watched
        .iter()
        .filter(|file| !moved.contains(&file.id))
        .cloned()
        .collect();
    if retained != current {
        return Err(changed());
    }
    plan.watched = current;
    plan.moving.retain(|file| !moved.contains(&file.id));
    for artifact in [&mut plan.nfo, &mut plan.poster] {
        if let Some(existing) = &mut artifact.existing
            && moved.contains(&existing.id)
        {
            *existing = known[&existing.id].clone();
        }
    }
    for file in known.into_values() {
        if (related(&file.name, &plan.stem)
            || plan
                .extra_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&file.name)))
            && !plan.target_watched.iter().any(|old| old.id == file.id)
        {
            plan.target_watched.push(file);
        }
    }
    plan.target_watched.sort_by_key(|file| file.id);
    Ok(())
}
