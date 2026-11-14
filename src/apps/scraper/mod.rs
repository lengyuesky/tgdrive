//! 固定应用的受限手动刮削能力；页面一次显式推进一个步骤，没有后台自动调度器。

mod backend;
pub(crate) mod data;
mod planner;
mod types;

use crate::{
    api::AppState,
    apps::InstalledApp,
    db::{now_ts, random_token},
    error::{AppError, AppResult},
};
use backend::{Backend, Candidate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use types::*;

pub(crate) const APP_ID: &str = "media-scraper";
const LEASE: i64 = 45;

pub(crate) struct Service {
    controls: Mutex<HashMap<String, Arc<Control>>>,
    secret_path: PathBuf,
    configure: tokio::sync::Mutex<()>,
}
struct Control {
    owner: String,
    cancel: CancellationToken,
    executing: AtomicBool,
    last_seen: AtomicI64,
}
impl Service {
    pub(crate) fn new(db_path: &str) -> Self {
        Self {
            controls: Mutex::new(HashMap::new()),
            secret_path: PathBuf::from(format!("{db_path}.scraper-token")),
            configure: tokio::sync::Mutex::new(()),
        }
    }
    pub(crate) fn invalidate(&self) {
        for control in self.controls.lock().unwrap().values() {
            control.cancel.cancel();
        }
    }
    pub(crate) async fn purge_secret(&self) -> AppResult<()> {
        match tokio::fs::remove_file(&self.secret_path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    fn control(&self, handle: &Handle) -> AppResult<Arc<Control>> {
        self.controls
            .lock()
            .unwrap()
            .get(&handle.id)
            .filter(|c| c.owner == handle.owner && !c.cancel.is_cancelled())
            .cloned()
            .ok_or_else(|| AppError::Conflict("原操作页面已停止，请核实结果后重新预览".into()))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Secret {
    generation: String,
    backend_url: String,
    token: String,
}
struct Configuration {
    url: String,
    token: Option<String>,
    key: String,
}

async fn configuration(state: &AppState) -> AppResult<Configuration> {
    let db = &state.vfs.db;
    let mut tx = db.pool.begin().await?;
    let raw: Option<String> =
        sqlx::query_scalar("SELECT settings FROM app_settings WHERE app_id=?")
            .bind(APP_ID)
            .fetch_optional(&mut *tx)
            .await?;
    let settings: Value = data::decode(raw.as_deref().unwrap_or("{}"))?;
    let url = backend::base_url(
        settings["backend_url"]
            .as_str()
            .unwrap_or(backend::DEFAULT_URL),
    )?
    .to_string();
    let generation: Option<String> =
        sqlx::query_scalar("SELECT secret_generation FROM app_scraper_state WHERE app_id=?")
            .bind(APP_ID)
            .fetch_optional(&mut *tx)
            .await?;
    let key = data::config_key(&mut tx).await?;
    tx.commit().await?;
    let token = if let Ok(bytes) = tokio::fs::read(&db.scraper.secret_path).await {
        if bytes.len() <= 16 * 1024 {
            serde_json::from_slice::<Secret>(&bytes)
                .ok()
                .filter(|s| Some(&s.generation) == generation.as_ref() && s.backend_url == url)
                .map(|s| s.token)
        } else {
            None
        }
    } else {
        None
    };
    Ok(Configuration { url, token, key })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    backend_url: String,
    token: Option<String>,
}
async fn configure(state: &AppState, app: &InstalledApp, input: Configure) -> AppResult<Value> {
    let db = &state.vfs.db;
    let _lock = db.scraper.configure.lock().await;
    let url = backend::base_url(&input.backend_url)?.to_string();
    if input
        .token
        .as_ref()
        .is_some_and(|t| t.len() > 4096 || !t.bytes().all(|b| (33..=126).contains(&b)))
    {
        return Err(AppError::BadRequest(
            "Token 需为不含空白的可打印字符，最多 4096 字节；留空可清除".into(),
        ));
    }
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    data::authorize(&mut tx, &app.revision, &[]).await?;
    ensure_no_active(&mut tx).await?;
    if let Some(token) = input.token {
        let generation = if token.is_empty() {
            String::new()
        } else {
            random_token(32)
        };
        if token.is_empty() {
            db.scraper.purge_secret().await?;
        } else {
            let path = db.scraper.secret_path.clone();
            let secret = Secret {
                generation: generation.clone(),
                backend_url: url.clone(),
                token,
            };
            tokio::task::spawn_blocking(move || -> AppResult<()> {
                use std::io::Write as _;
                let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
                std::fs::create_dir_all(parent)?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(&serde_json::to_vec(&secret).map_err(anyhow::Error::from)?)?;
                file.as_file().sync_all()?;
                file.persist(path).map_err(|e| AppError::Io(e.error))?;
                Ok(())
            })
            .await
            .map_err(|e| AppError::Internal(e.into()))??;
        }
        sqlx::query("INSERT INTO app_scraper_state(app_id,secret_generation) VALUES(?,?) ON CONFLICT(app_id) DO UPDATE SET secret_generation=excluded.secret_generation").bind(APP_ID).bind(generation).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO app_settings(app_id,settings) VALUES(?,?) ON CONFLICT(app_id) DO UPDATE SET settings=excluded.settings")
        .bind(APP_ID).bind(data::encode(&json!({"backend_url":url}))?).execute(&mut *tx).await?;
    tx.commit().await?;
    let config = configuration(state).await?;
    Ok(json!({"backend_url":config.url,"token_configured":config.token.is_some()}))
}

pub(crate) async fn ensure_no_active(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> AppResult<()> {
    let active:i64=sqlx::query_scalar("SELECT COUNT(*) FROM app_scraper_batches WHERE app_id=? AND status IN ('running','stopping') AND expires_at>?")
        .bind(APP_ID).bind(now_ts()).fetch_one(&mut **tx).await?;
    if active != 0 {
        return Err(AppError::Conflict(
            "请先暂停正在执行的刮削批次，再修改后端配置".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_settings_patch(patch: &str) -> AppResult<()> {
    let patch: Value = data::decode(patch)?;
    if let Some(url) = patch.get("backend_url") {
        backend::base_url(
            url.as_str()
                .ok_or_else(|| AppError::BadRequest("后端地址需为字符串".into()))?,
        )?;
    }
    Ok(())
}

pub(crate) async fn invalidate_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    purge: bool,
    clear_token: bool,
) -> AppResult<()> {
    if purge {
        sqlx::query("DELETE FROM app_scraper_batches WHERE app_id=?")
            .bind(APP_ID)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM app_scraper_state WHERE app_id=?")
            .bind(APP_ID)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("UPDATE app_scraper_items SET status='needs_verification' WHERE status='processing' AND batch_id IN (SELECT id FROM app_scraper_batches WHERE app_id=?)").bind(APP_ID).execute(&mut **tx).await?;
        sqlx::query("UPDATE app_scraper_batches SET status='interrupted',owner='',expires_at=0,message='应用状态或授权已变化，请核实并重新扫描' WHERE app_id=? AND status IN ('draft','running','stopping','paused')").bind(APP_ID).execute(&mut **tx).await?;
        if clear_token {
            sqlx::query("UPDATE app_scraper_batches SET read_only=1 WHERE app_id=?")
                .bind(APP_ID)
                .execute(&mut **tx)
                .await?;
            data::discard_restored_uploads(tx).await?;
            sqlx::query("UPDATE app_scraper_state SET secret_generation='' WHERE app_id=?")
                .bind(APP_ID)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

async fn create(state: &AppState, app: &InstalledApp, root: Reference) -> AppResult<Value> {
    let db = &state.vfs.db;
    data::recover_interrupted(&db.pool).await?;
    db.require_background_enabled()?;
    let config = configuration(state).await?;
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let dir = data::node(&mut tx, root.id).await?;
    if !dir.is_dir
        || dir.content_version() != root.content_version
        || data::path(&mut tx, root.id).await? != root.path
    {
        return Err(AppError::Conflict("刮削目录已改变，请重新选择".into()));
    }
    data::authorize(&mut tx, &app.revision, &[&root.path]).await?;
    let id = random_token(32);
    let owner = random_token(32);
    let now = now_ts();
    sqlx::query("INSERT INTO app_scraper_batches(id,app_id,revision,config_key,root_id,root_path,owner,generation,status,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,'draft',?,?)")
        .bind(&id).bind(APP_ID).bind(&app.revision).bind(&config.key).bind(root.id).bind(&root.path).bind(&owner).bind(data::generation()).bind(now).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(json!({"id":id,"owner":owner}))
}

async fn prepare(state: &AppState, app: &InstalledApp, input: Prepare) -> AppResult<Value> {
    let db = &state.vfs.db;
    let config = configuration(state).await?;
    let mut tx = db.pool.begin().await?;
    let batch = data::batch(&mut tx, &input.batch.id).await?;
    if batch.owner != input.batch.owner
        || batch.status != "draft"
        || batch.revision != app.revision
        || batch.config_key != config.key
    {
        return Err(AppError::Conflict(
            "预览会话或后端已改变，请重新预览".into(),
        ));
    }
    tx.commit().await?;
    let backend = Backend::new(state, &config.url, config.token)?;
    let plan = planner::build(state, app, &batch, &input, &backend).await;
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = data::batch(&mut tx, &batch.id).await?;
    if current.owner != input.batch.owner
        || current.status != "draft"
        || data::config_key(&mut tx).await? != batch.config_key
    {
        return Err(AppError::Conflict(
            "预览会话或配置已改变，请重新预览".into(),
        ));
    }
    data::authorize(&mut tx, &app.revision, &[&batch.root_path]).await?;
    let key = input
        .files
        .first()
        .map(|f| {
            format!(
                "{}:{}",
                f.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("/"),
                stem(f.path.rsplit('/').next().unwrap_or("")).to_ascii_lowercase()
            )
        })
        .unwrap_or_else(|| random_token(16));
    let old: Option<String> =
        sqlx::query_scalar("SELECT id FROM app_scraper_items WHERE batch_id=? AND group_key=?")
            .bind(&batch.id)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    let id = old.unwrap_or_else(|| random_token(32));
    let position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position),-1)+1 FROM app_scraper_items WHERE batch_id=?",
    )
    .bind(&batch.id)
    .fetch_one(&mut *tx)
    .await?;
    let response = match plan {
        Ok(mut plan) => {
            data::check_plan(&mut tx, &batch, &plan).await?;
            if let (Some(candidate), Some(movie)) = (&plan.candidate, &plan.movie) {
                sqlx::query("INSERT INTO app_scraper_sources(batch_id,provider,source_id,movie) VALUES(?,?,?,?) ON CONFLICT(batch_id,provider,source_id) DO NOTHING")
                    .bind(&batch.id).bind(&candidate.provider).bind(&candidate.id).bind(data::encode(movie)?).execute(&mut *tx).await?;
                let saved:String=sqlx::query_scalar("SELECT movie FROM app_scraper_sources WHERE batch_id=? AND provider=? AND source_id=?").bind(&batch.id).bind(&candidate.provider).bind(&candidate.id).fetch_one(&mut *tx).await?;
                let movie: backend::Movie = data::decode(&saved)?;
                if plan.nfo.action != "keep" {
                    movie.nfo()?;
                }
                plan.movie = Some(movie);
            }
            let result = ResultRecord {
                organize: Step {
                    state: "pending".into(),
                    ..Default::default()
                },
                nfo: Step {
                    state: if plan.nfo.action == "keep" {
                        if plan.nfo.existing.is_some() {
                            "kept"
                        } else {
                            "skipped"
                        }
                    } else {
                        "pending"
                    }
                    .into(),
                    file: plan.nfo.existing.clone(),
                    ..Default::default()
                },
                poster: Step {
                    state: if plan.poster.action == "keep" {
                        if plan.poster.existing.is_some() {
                            "kept"
                        } else {
                            "skipped"
                        }
                    } else {
                        "pending"
                    }
                    .into(),
                    file: plan.poster.existing.clone(),
                    ..Default::default()
                },
                reason: if plan.needs_candidate {
                    "请选择影视资料候选".into()
                } else {
                    String::new()
                },
            };
            sqlx::query("INSERT INTO app_scraper_items(id,batch_id,group_key,position,files,plan,result,status) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(batch_id,group_key) DO UPDATE SET files=excluded.files,plan=excluded.plan,result=excluded.result,status=excluded.status")
                .bind(&id).bind(&batch.id).bind(&key).bind(position).bind(data::encode(&plan.videos)?).bind(data::encode(&plan)?).bind(data::encode(&result)?).bind(if plan.needs_candidate{"needs_user"}else{"pending"}).execute(&mut *tx).await?;
            data::plan_value(&id, &plan)
        }
        Err(error) => {
            if matches!(
                error,
                AppError::ScraperBackend(_) | AppError::Forbidden(_) | AppError::AppChanged
            ) {
                return Err(error);
            }
            let message = error.public_parts().0;
            let files:Vec<Value>=input.files.iter().map(|f|json!({"id":f.id,"path":f.path,"name":f.path.rsplit('/').next().unwrap_or(""),"content_version":f.content_version})).collect();
            sqlx::query("INSERT INTO app_scraper_items(id,batch_id,group_key,position,files,result,status) VALUES(?,?,?,?,?,?,'needs_user') ON CONFLICT(batch_id,group_key) DO UPDATE SET files=excluded.files,plan='null',result=excluded.result,status='needs_user'")
                .bind(&id).bind(&batch.id).bind(&key).bind(position).bind(data::encode(&files)?).bind(data::encode(&ResultRecord{reason:message.clone(),..Default::default()})?).execute(&mut *tx).await?;
            json!({"id":id,"ready":false,"needs_candidate":false,"error":message,"files":files})
        }
    };
    sqlx::query("UPDATE app_scraper_batches SET updated_at=? WHERE id=?")
        .bind(now_ts())
        .bind(&batch.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(response)
}

async fn begin(state: &AppState, app: &InstalledApp, handle: Handle) -> AppResult<Value> {
    let db = &state.vfs.db;
    let _lock = db.scraper.configure.lock().await;
    data::recover_interrupted(&db.pool).await?;
    db.require_background_enabled()?;
    if db
        .scraper
        .controls
        .lock()
        .unwrap()
        .values()
        .any(|c| c.executing.load(Ordering::Acquire))
    {
        return Err(AppError::Conflict(
            "上一批仍在停止或核实，请稍后再启动".into(),
        ));
    }
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let batch = data::batch(&mut tx, &handle.id).await?;
    if batch.owner != handle.owner
        || batch.status != "draft"
        || batch.revision != app.revision
        || batch.config_key != data::config_key(&mut tx).await?
    {
        return Err(AppError::Conflict("批次预览已失效，请重新预览".into()));
    }
    data::authorize(&mut tx, &app.revision, &[&batch.root_path]).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_scraper_items WHERE batch_id=? AND status='pending'",
    )
    .bind(&handle.id)
    .fetch_one(&mut *tx)
    .await?;
    if count == 0 {
        return Err(AppError::BadRequest(
            "没有可以执行的视频，请完成候选选择或重新预览".into(),
        ));
    }
    let conflicting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT json_extract(plan,'$.target_path') FROM app_scraper_items WHERE batch_id=? AND status='pending' AND json_type(plan,'$.candidate')='object' GROUP BY json_extract(plan,'$.target_path') HAVING COUNT(DISTINCT json_array(json_extract(plan,'$.candidate.provider'),json_extract(plan,'$.candidate.id')))>1)").bind(&handle.id).fetch_one(&mut *tx).await?;
    if conflicting != 0 {
        return Err(AppError::Conflict(
            "同番号视频需使用同一份选定资料，请统一候选后重新预览".into(),
        ));
    }
    let raws: Vec<String> = sqlx::query_scalar(
        "SELECT plan FROM app_scraper_items WHERE batch_id=? AND status='pending'",
    )
    .bind(&handle.id)
    .fetch_all(&mut *tx)
    .await?;
    let plans: Vec<Plan> = raws
        .iter()
        .map(|raw| data::decode(raw))
        .collect::<AppResult<_>>()?;
    for plan in &plans {
        for artifact in [&plan.nfo, &plan.poster] {
            if artifact.action != "keep"
                && other_video_uses(
                    &artifact.name,
                    &plan.stem,
                    plans
                        .iter()
                        .filter(|p| p.target_path == plan.target_path)
                        .flat_map(|p| p.videos.iter().map(|f| f.name.as_str())),
                )
            {
                return Err(AppError::Conflict(
                    "所选组的资料命名相互影响，请取消受影响资料更新或手动确定归属后重新预览".into(),
                ));
            }
        }
    }
    let other:i64=sqlx::query_scalar("SELECT COUNT(*) FROM app_scraper_batches WHERE app_id=? AND status IN ('running','stopping')").bind(APP_ID).fetch_one(&mut *tx).await?;
    if other != 0 {
        return Err(AppError::Conflict(
            "已有刮削批次执行中，请在操作记录中查看并等待其结束".into(),
        ));
    }
    sqlx::query("UPDATE app_scraper_batches SET status='running',generation=?,expires_at=?,updated_at=? WHERE id=?").bind(data::generation()).bind(now_ts()+LEASE).bind(now_ts()).bind(&handle.id).execute(&mut *tx).await?;
    tx.commit().await?;
    db.scraper.controls.lock().unwrap().insert(
        handle.id.clone(),
        Arc::new(Control {
            owner: handle.owner.clone(),
            cancel: CancellationToken::new(),
            executing: AtomicBool::new(false),
            last_seen: AtomicI64::new(now_ts()),
        }),
    );
    Ok(json!({"ok":true}))
}

async fn heartbeat(state: &AppState, handle: Handle) -> AppResult<Value> {
    let db = &state.vfs.db;
    let control = db.scraper.control(&handle)?;
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    data::active(&mut tx, &handle.id).await?;
    control.last_seen.store(now_ts(), Ordering::Release);
    sqlx::query("UPDATE app_scraper_batches SET expires_at=?,updated_at=? WHERE id=? AND owner=? AND status='running'").bind(now_ts()+LEASE).bind(now_ts()).bind(&handle.id).bind(&handle.owner).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(json!({"ok":true}))
}

async fn upload(
    state: &AppState,
    batch: &str,
    item: &str,
    plan: &Plan,
    role: &str,
    bytes: Vec<u8>,
) -> AppResult<()> {
    use crate::engine::upload::ResumablePartStart;
    use sha2::{Digest, Sha256};
    let target = state.vfs.tg.next_upload()?;
    let sha = crate::engine::upload::to_hex(&Sha256::digest(&bytes));
    let session = data::new_upload(
        &state.vfs.db,
        batch,
        item,
        plan,
        role,
        UploadContent {
            size: bytes.len() as i64,
            sha256: &sha,
        },
        target.storage_target_id,
    )
    .await?;
    match state
        .vfs
        .uploader
        .start_resumable_part(&session.session.id, 0)
        .await?
    {
        ResumablePartStart::Complete => {}
        ResumablePartStart::Upload(handle) => {
            handle.feed(bytes::Bytes::from(bytes)).await?;
            handle.finish().await?;
        }
    }
    state
        .vfs
        .db
        .complete_upload_session(&session.session.id)
        .await?;
    state.vfs.uploader.process_pending_deletes();
    Ok(())
}

async fn phase(state: &AppState, item: &Item, plan: Plan) -> AppResult<()> {
    let result: ResultRecord = data::decode(&item.result)?;
    if result.organize.state != "done" {
        data::move_group(&state.vfs.db, &item.batch_id, &item.id, plan).await?;
        return Ok(());
    }
    if !matches!(result.nfo.state.as_str(), "done" | "kept" | "skipped") {
        let candidate = plan
            .candidate
            .as_ref()
            .ok_or_else(|| AppError::BadRequest("缺少已选定的影片资料".into()))?;
        let saved: String = sqlx::query_scalar(
            "SELECT movie FROM app_scraper_sources WHERE batch_id=? AND provider=? AND source_id=?",
        )
        .bind(&item.batch_id)
        .bind(&candidate.provider)
        .bind(&candidate.id)
        .fetch_optional(&state.vfs.db.pool)
        .await?
        .ok_or_else(|| AppError::BadRequest("影片资料记录已失效，请重新预览".into()))?;
        let movie: backend::Movie = data::decode(&saved)?;
        upload(state, &item.batch_id, &item.id, &plan, "nfo", movie.nfo()?).await?;
        return Ok(());
    }
    if !matches!(result.poster.state.as_str(), "done" | "kept" | "skipped") {
        let candidate = plan
            .candidate
            .as_ref()
            .ok_or_else(|| AppError::BadRequest("缺少已选定的海报资料".into()))?;
        let config = configuration(state).await?;
        let backend = Backend::new(state, &config.url, config.token)?;
        let bytes = backend.poster(candidate, false).await?;
        upload(state, &item.batch_id, &item.id, &plan, "poster", bytes).await?;
    }
    Ok(())
}

async fn next(state: &AppState, handle: Handle) -> AppResult<Value> {
    let db = &state.vfs.db;
    let control = db.scraper.control(&handle)?;
    if control.executing.swap(true, Ordering::AcqRel) {
        return Err(AppError::Conflict("当前步骤仍在执行，请等待结果".into()));
    }
    struct InFlight(Arc<Control>);
    impl Drop for InFlight {
        fn drop(&mut self) {
            self.0.executing.store(false, Ordering::Release);
        }
    }
    let _flight = InFlight(control.clone());
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    data::active(&mut tx, &handle.id).await?;
    let item:Option<Item>=sqlx::query_as("SELECT * FROM app_scraper_items WHERE batch_id=? AND status='pending' ORDER BY position LIMIT 1").bind(&handle.id).fetch_optional(&mut *tx).await?;
    let Some(item) = item else {
        sqlx::query("UPDATE app_scraper_batches SET status='completed',expires_at=0,updated_at=? WHERE id=?").bind(now_ts()).bind(&handle.id).execute(&mut *tx).await?;
        tx.commit().await?;
        control.cancel.cancel();
        return Ok(json!({"done":true}));
    };
    sqlx::query("UPDATE app_scraper_items SET status='processing' WHERE id=?")
        .bind(&item.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let plan: Plan = data::decode(&item.plan)?;
    let deadline = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if now_ts() - control.last_seen.load(Ordering::Acquire) >= LEASE {
                control.cancel.cancel();
                return;
            }
        }
    };
    let outcome = tokio::select! {
        biased;
        _=control.cancel.cancelled()=>Err(AppError::Conflict("操作已停止，请核实已提交结果".into())),
        _=deadline=>Err(AppError::Conflict("操作页面连接已中断，请核实结果".into())),
        result=tokio::time::timeout(Duration::from_secs(130),phase(state,&item,plan))=>result.unwrap_or_else(|_|Err(AppError::ScraperBackend("当前步骤超时，批次已暂停".into()))),
    };
    let cancelled = control.cancel.is_cancelled();
    if outcome.as_ref().is_err_and(|e| {
        cancelled
            || matches!(
                e,
                AppError::ScraperBackend(_)
                    | AppError::Telegram(_)
                    | AppError::TelegramRejected(_)
                    | AppError::TelegramUncertain(_)
                    | AppError::UploadInterrupted
            )
    }) {
        sqlx::query(
            "UPDATE app_scraper_batches SET status='stopping' WHERE id=? AND status='running'",
        )
        .bind(&handle.id)
        .execute(&db.pool)
        .await?;
    }
    if outcome.is_err() {
        cancel_item_uploads(state, &item.id).await?;
    }
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let raw: String = sqlx::query_scalar("SELECT result FROM app_scraper_items WHERE id=?")
        .bind(&item.id)
        .fetch_one(&mut *tx)
        .await?;
    let mut result: ResultRecord = data::decode(&raw)?;
    for step in [&mut result.nfo, &mut result.poster] {
        if step.state == "uploading"
            && let Some(id) = &step.upload_id
            && db
                .upload_receipt(id)
                .await?
                .is_some_and(|receipt| receipt.status == "cancelled")
        {
            step.state = "cancelled".into();
            step.file = None;
        }
    }
    let mut paused = false;
    let status = if let Err(error) = outcome {
        result.reason = error.public_parts().0;
        if cancelled || matches!(error, AppError::TelegramUncertain(_)) {
            "needs_verification"
        } else if matches!(
            error,
            AppError::ScraperBackend(_)
                | AppError::Telegram(_)
                | AppError::TelegramRejected(_)
                | AppError::UploadInterrupted
        ) {
            paused = true;
            "pending"
        } else {
            "needs_user"
        }
    } else if result.organize.state == "done"
        && matches!(result.nfo.state.as_str(), "done" | "kept" | "skipped")
        && matches!(result.poster.state.as_str(), "done" | "kept" | "skipped")
    {
        "done"
    } else {
        "pending"
    };
    sqlx::query("UPDATE app_scraper_items SET status=?,result=? WHERE id=?")
        .bind(status)
        .bind(data::encode(&result)?)
        .bind(&item.id)
        .execute(&mut *tx)
        .await?;
    if cancelled || paused || status == "needs_verification" {
        paused = true;
        sqlx::query("UPDATE app_scraper_batches SET status=?,expires_at=0,message=?,updated_at=? WHERE id=?").bind(if cancelled{"cancelled"}else{"paused"}).bind(&result.reason).bind(now_ts()).bind(&handle.id).execute(&mut *tx).await?;
        control.cancel.cancel();
    }
    tx.commit().await?;
    crate::events::emit(&state.events, crate::events::HostEvent::files());
    Ok(json!({"done":false,"paused":paused,"item_id":item.id,"status":status,"result":result}))
}

async fn cancel_item_uploads(state: &AppState, item_id: &str) -> AppResult<()> {
    let rows:Vec<(i64,Option<String>)>=sqlx::query_as("SELECT g.node_id,s.id FROM app_scraper_upload_guards g JOIN nodes n ON n.id=g.node_id LEFT JOIN upload_sessions s ON s.node_id=g.node_id WHERE g.item_id=? AND n.status='uploading'").bind(item_id).fetch_all(&state.vfs.db.pool).await?;
    for (node_id, id) in rows {
        if let Some(id) = id {
            state.vfs.uploader.begin_cancel_resumable_session(&id).await;
            let cancel = state.vfs.db.cancel_upload_session(&id).await;
            state.vfs.uploader.finish_cancel_resumable_session(&id);
            if let Err(error) = cancel
                && !matches!(error, AppError::NotFound)
            {
                return Err(error);
            }
        } else {
            state.vfs.db.abort_upload(node_id).await?;
        }
        state.vfs.uploader.process_pending_deletes();
    }
    Ok(())
}

async fn stop(state: &AppState, handle: Handle) -> AppResult<Value> {
    let db = &state.vfs.db;
    let control = db
        .scraper
        .controls
        .lock()
        .unwrap()
        .get(&handle.id)
        .filter(|c| c.owner == handle.owner)
        .cloned();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let batch = data::batch(&mut tx, &handle.id).await?;
    if batch.owner != handle.owner {
        return Err(AppError::Forbidden("批次不属于当前操作页面".into()));
    }
    if !matches!(batch.status.as_str(), "draft" | "running" | "stopping") {
        tx.commit().await?;
        return Ok(json!({"stopped":true,"status":batch.status}));
    }
    sqlx::query("UPDATE app_scraper_batches SET status='stopping',message='正在停止并核实在途步骤',updated_at=? WHERE id=?").bind(now_ts()).bind(&handle.id).execute(&mut *tx).await?;
    tx.commit().await?;
    if let Some(control) = &control {
        control.cancel.cancel();
    }
    let stopped = control
        .as_ref()
        .is_none_or(|c| !c.executing.load(Ordering::Acquire));
    if stopped {
        // HTTP 请求可在处理期间断开；显式停止负责收尾已失去处理者的步骤。
        let items: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM app_scraper_items WHERE batch_id=? AND status='processing'",
        )
        .bind(&handle.id)
        .fetch_all(&db.pool)
        .await?;
        for id in &items {
            cancel_item_uploads(state, id).await?;
        }
        let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE app_scraper_items SET status='needs_verification' WHERE batch_id=? AND status='processing'")
            .bind(&handle.id).execute(&mut *tx).await?;
        let message = if items.is_empty() {
            "后续操作已停止，已完成结果保留"
        } else {
            "后续操作已停止，已完成结果保留；中断步骤需核实"
        };
        sqlx::query("UPDATE app_scraper_batches SET status='cancelled',expires_at=0,message=?,updated_at=? WHERE id=?")
            .bind(message).bind(now_ts()).bind(&handle.id).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(json!({"stopped":stopped,"status":if stopped{"cancelled"}else{"stopping"}}))
}

pub(super) async fn records(state: &AppState, params: Value) -> AppResult<Value> {
    data::recover_interrupted(&state.vfs.db.pool).await?;
    let (before, before_id) = if let Some(cursor) = params.get("cursor").and_then(Value::as_str) {
        let (time, id) = cursor
            .split_once(':')
            .ok_or_else(|| AppError::BadRequest("记录游标无效".into()))?;
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(AppError::BadRequest("记录游标无效".into()));
        }
        (
            time.parse::<i64>()
                .map_err(|_| AppError::BadRequest("记录游标无效".into()))?,
            id.to_string(),
        )
    } else {
        (i64::MAX, String::new())
    };
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 100) as i64;
    let rows:Vec<Batch>=sqlx::query_as("SELECT * FROM app_scraper_batches WHERE app_id=? AND (created_at<? OR (created_at=? AND id<?)) ORDER BY created_at DESC,id DESC LIMIT ?").bind(APP_ID).bind(before).bind(before).bind(before_id).bind(limit+1).fetch_all(&state.vfs.db.pool).await?;
    let has_more = rows.len() > limit as usize;
    let mut entries = Vec::new();
    for row in rows.iter().take(limit as usize) {
        let (total,done):(i64,i64)=sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(status='done'),0) FROM app_scraper_items WHERE batch_id=?").bind(&row.id).fetch_one(&state.vfs.db.pool).await?;
        entries.push(json!({"id":row.id,"root_path":row.root_path,"status":row.status,"message":row.message,"read_only":row.read_only,"created_at":row.created_at,"updated_at":row.updated_at,"total":total,"completed":done}));
    }
    Ok(
        json!({"records":entries,"has_more":has_more,"next_cursor":if has_more{rows.get(limit as usize-1).map(|r|format!("{}:{}",r.created_at,r.id))}else{None}}),
    )
}

async fn record(state: &AppState, params: Value) -> AppResult<Value> {
    let id = params["id"]
        .as_str()
        .ok_or_else(|| AppError::BadRequest("缺少批次 ID".into()))?;
    let after = params.get("after").and_then(Value::as_i64).unwrap_or(-1);
    let mut tx = state.vfs.db.pool.begin().await?;
    let batch = data::batch(&mut tx, id).await?;
    let rows: Vec<RecordRow> = sqlx::query_as("SELECT id,position,files,result,status,json_extract(plan,'$.number') AS number,json_extract(plan,'$.target_path') AS target_path FROM app_scraper_items WHERE batch_id=? AND position>? ORDER BY position LIMIT 101").bind(id).bind(after).fetch_all(&mut *tx).await?;
    let entries = rows.iter().take(100).map(|row| -> AppResult<Value> {
        Ok(json!({"id":row.id,"position":row.position,"files":data::decode::<Value>(&row.files)?,"result":data::decode::<Value>(&row.result)?,"status":row.status,"number":row.number,"target_path":row.target_path}))
    }).collect::<AppResult<Vec<_>>>()?;
    tx.commit().await?;
    Ok(
        json!({"id":batch.id,"root_id":batch.root_id,"root_path":batch.root_path,"status":batch.status,"message":batch.message,"read_only":batch.read_only,"items":entries,"has_more":rows.len()>100,"next_after":rows.get(99).map(|r|r.position)}),
    )
}

async fn verify_record(state: &AppState, params: Value) -> AppResult<Value> {
    let id = params["id"]
        .as_str()
        .ok_or_else(|| AppError::BadRequest("缺少批次 ID".into()))?;
    let rows: Vec<Item> = sqlx::query_as(
        "SELECT * FROM app_scraper_items WHERE batch_id=? AND status='needs_verification'",
    )
    .bind(id)
    .fetch_all(&state.vfs.db.pool)
    .await?;
    let mut tx = state.vfs.db.pool.begin().await?;
    let batch = data::batch(&mut tx, id).await?;
    if batch.read_only {
        return Err(AppError::Conflict(
            "恢复记录仅供查看，请重新扫描目录".into(),
        ));
    }
    if matches!(batch.status.as_str(), "running" | "stopping") {
        return Err(AppError::Conflict(
            "请等待当前操作到达停止状态，再核实结果".into(),
        ));
    }
    tx.commit().await?;
    for item in rows {
        cancel_item_uploads(state, &item.id).await?;
        let fresh: String = sqlx::query_scalar("SELECT result FROM app_scraper_items WHERE id=?")
            .bind(&item.id)
            .fetch_one(&state.vfs.db.pool)
            .await?;
        let mut result: ResultRecord = data::decode(&fresh)?;
        for step in [&mut result.nfo, &mut result.poster] {
            if step.state == "uploading" {
                step.state = "cancelled".into();
                step.file = None;
            }
        }
        result.reason = "已核实宿主提交记录；请重新扫描并预览未完成项".into();
        sqlx::query("UPDATE app_scraper_items SET status='needs_user',result=? WHERE id=?")
            .bind(data::encode(&result)?)
            .bind(&item.id)
            .execute(&state.vfs.db.pool)
            .await?;
    }
    record(state, json!({"id":id})).await
}

pub(super) async fn invoke(
    state: &AppState,
    app: &InstalledApp,
    method: &str,
    params: Value,
) -> AppResult<Value> {
    if app.manifest.id != APP_ID || app.manifest.api_version < 2 {
        return Err(AppError::Forbidden("影视刮削能力仅供指定应用使用".into()));
    }
    match method {
        "scraper.config.get" => {
            let cfg = configuration(state).await?;
            Ok(json!({"backend_url":cfg.url,"token_configured":cfg.token.is_some()}))
        }
        "scraper.config.set" => configure(state, app, crate::apps::files::parse(params)?).await,
        "scraper.batch.create" => create(state, app, crate::apps::files::parse(params)?).await,
        "scraper.preview" => prepare(state, app, crate::apps::files::parse(params)?).await,
        "scraper.batch.begin" => begin(state, app, crate::apps::files::parse(params)?).await,
        "scraper.heartbeat" => heartbeat(state, crate::apps::files::parse(params)?).await,
        "scraper.next" => next(state, crate::apps::files::parse(params)?).await,
        "scraper.stop" => stop(state, crate::apps::files::parse(params)?).await,
        "scraper.records" => records(state, params).await,
        "scraper.record" => record(state, params).await,
        "scraper.verify" => verify_record(state, params).await,
        "scraper.providers" | "scraper.search" | "scraper.poster" => {
            let config = configuration(state).await?;
            let backend = Backend::new(state, &config.url, config.token)?;
            match method {
                "scraper.providers" => Ok(json!({"providers":backend.providers().await?})),
                "scraper.search" => {
                    let number = params["number"]
                        .as_str()
                        .ok_or_else(|| AppError::BadRequest("缺少番号".into()))?;
                    Ok(json!({"candidates":backend.search(number).await?}))
                }
                _ => {
                    let candidate: Candidate = crate::apps::files::parse(params)?;
                    Ok(backend::poster_value(
                        backend.poster(&candidate, true).await?,
                    ))
                }
            }
        }
        _ => Err(AppError::BadRequest("不支持的影视刮削能力".into())),
    }
}
