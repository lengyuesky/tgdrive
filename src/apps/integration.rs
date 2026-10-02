//! 宿主控制的文件入口、授权、应用详情与恢复接口。
use super::*;

#[derive(Deserialize)]
pub(super) struct Authorization {
    path: String,
    expected_revision: String,
}

pub(super) async fn authorize(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<Authorization>,
) -> AppResult<Json<InstalledApp>> {
    let _guard = INSTALL_LOCK.lock().await;
    let app = installed(&state, &id).await?;
    if app.revision != input.expected_revision {
        return Err(AppError::AppChanged);
    }
    app.manifest.require("files.read")?;
    if input.path.len() > 4096 {
        return Err(AppError::BadRequest("目录路径过长".into()));
    }
    let path = crate::vfs::canonical(&input.path)?;
    if !state.vfs.resolve(&path).await?.is_dir {
        return Err(AppError::NotDirectory);
    }
    if app.scope_mode != "all" {
        let mut paths = app.scope.clone();
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() > 32 {
            return Err(AppError::BadRequest("目录授权最多 32 项".into()));
        }
        paths.sort();
        state
            .vfs
            .db
            .set_app_scope_mode(
                &id,
                "selected",
                &serde_json::to_string(&paths).map_err(anyhow::Error::from)?,
                &app.revision,
            )
            .await?;
        crate::events::emit(
            &state.events,
            crate::events::HostEvent::app("app.scope", &id, None),
        );
    }
    Ok(Json(installed(&state, &id).await?))
}

pub(super) async fn open(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(reference): Json<files::Reference>,
) -> AppResult<Json<Value>> {
    let app = installed(&state, &id).await?;
    if !app.enabled {
        return Err(AppError::AppChanged);
    }
    app.manifest.require("files.read")?;
    let (node, path) = files::resolve(&state, &reference).await?;
    let info = app
        .manifest
        .integration
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("此应用未声明文件入口".into()))?;
    let ext = node
        .name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    if if node.is_dir {
        !info.directories
    } else {
        !info.file_types.contains(&ext)
    } {
        return Err(AppError::BadRequest("此应用不支持打开这个文件类型".into()));
    }
    scope::check(&state, &app, &path).await?;
    Ok(Json(files::metadata(&node, &path)))
}

pub(super) async fn health(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    installed(&state, &id).await?;
    Ok(Json(state.vfs.db.app_health(&id).await?))
}

#[derive(Deserialize)]
pub(super) struct Report {
    revision: String,
    phase: String,
    #[serde(default)]
    message: String,
}
pub(super) async fn report(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(report): Json<Report>,
) -> AppResult<Json<Value>> {
    if !["ready", "failed", "loading"].contains(&report.phase.as_str())
        || report.message.len() > 1200
    {
        return Err(AppError::BadRequest("诊断状态无效".into()));
    }
    state
        .vfs
        .db
        .report_app_health(&id, &report.revision, &report.phase, &report.message)
        .await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub(super) struct Recovery {
    expected_revision: String,
}
pub(super) async fn recover(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<Recovery>,
) -> AppResult<Json<InstalledApp>> {
    let _guard = INSTALL_LOCK.lock().await;
    let _cache_guard = CACHE_LOCK.lock().await;
    let bytes = state.vfs.db.app_recovery_package(&id).await?;
    let root = cache_root(&state);
    let package = blocking(move || {
        let package = AppPackage::parse(bytes)?;
        package.materialize(&root)?;
        Ok(package)
    })
    .await?;
    if package.manifest.id != id {
        return Err(AppError::BadRequest("恢复包身份不匹配".into()));
    }
    state
        .vfs
        .db
        .recover_app(&id, &input.expected_revision, &package.digest)
        .await?;
    prune_sibling_digests(&cache_root(&state).join(&id), &package.digest).await;
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::app("app.scope", &id, None),
    );
    Ok(Json(installed(&state, &id).await?))
}

pub(super) async fn screenshot(
    State(state): State<AppState>,
    Path((id, index)): Path<(String, usize)>,
    Query(query): Query<CatalogPackageQuery>,
) -> AppResult<Response> {
    let (_, _, bytes) = state
        .apps_catalog
        .get_package(&id, query.digest.as_deref())
        .await?;
    let package = blocking(move || AppPackage::parse(bytes)).await?;
    let path = package
        .manifest
        .integration
        .as_ref()
        .and_then(|i| i.screenshots.get(index))
        .ok_or(AppError::NotFound)?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        package.files.get(path).ok_or(AppError::NotFound)?.clone(),
    )
        .into_response())
}

pub(super) async fn lifecycle(
    state: &AppState,
    app: &InstalledApp,
    params: Value,
) -> AppResult<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Migration {
        action: String,
        version: u32,
        owner: String,
        #[serde(default)]
        message: String,
    }
    let input: Migration = files::parse(params)?;
    let info = app
        .manifest
        .integration
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("应用未声明数据版本".into()))?;
    if input.version != info.data_schema
        || input.owner.is_empty()
        || input.owner.len() > 80
        || !input
            .owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || input.message.len() > 1200
    {
        return Err(AppError::BadRequest("迁移参数与应用声明不匹配".into()));
    }
    state
        .vfs
        .db
        .app_migration(
            &app.manifest.id,
            &app.revision,
            &input.action,
            input.version,
            &input.owner,
            &input.message,
        )
        .await
}
