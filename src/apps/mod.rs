//! 应用中心：独立插件包、安装生命周期与权限受限的页面运行环境。

pub mod catalog;
mod files;
mod package;
mod runtime;
mod storage;

pub use catalog::CatalogManager;

use crate::api::AppState;
use crate::db::apps::{AppInstall, AppRecord};
use crate::error::{AppError, AppResult};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use package::{AppPackage, Manifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::Mutex;

static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());
static CACHE_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Serialize)]
pub struct InstalledApp {
    pub manifest: Manifest,
    pub digest: String,
    pub revision: String,
    pub enabled: bool,
    pub pinned: bool,
    pub source: String,
    pub installed_at: i64,
    pub updated_at: i64,
}

impl InstalledApp {
    fn from_record(record: AppRecord) -> AppResult<Self> {
        Ok(Self {
            manifest: serde_json::from_str(&record.manifest)
                .map_err(|e| AppError::Internal(e.into()))?,
            digest: record.digest,
            revision: record.revision,
            enabled: record.enabled,
            pinned: record.pinned,
            source: record.source,
            installed_at: record.installed_at,
            updated_at: record.updated_at,
        })
    }
}

pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/apps", get(index))
        .route(
            "/apps/inspect",
            post(inspect).layer(DefaultBodyLimit::max(package::MAX_PACKAGE_SIZE)),
        )
        .route(
            "/apps/install",
            post(install).layer(DefaultBodyLimit::max(package::MAX_PACKAGE_SIZE)),
        )
        .route("/apps/catalog/refresh", post(catalog_refresh))
        .route("/apps/catalog/{id}/package", get(catalog_download))
        .route("/apps/catalog/{id}/install", post(catalog_install))
        .route("/apps/{id}", get(detail).delete(uninstall))
        .route("/apps/{id}/state", post(set_state))
        .route(
            "/apps/{id}/settings",
            get(settings_get).patch(settings_patch),
        )
        .route("/apps/{id}/launch", post(runtime::launch))
        .route("/apps/{id}/rpc", post(runtime::rpc))
        .layer(middleware::from_fn(no_store))
}

pub fn public_router() -> Router<AppState> {
    Router::new()
        .route(
            "/apps/runtime/{id}/{revision}/{*resource}",
            get(runtime::asset),
        )
        .route("/apps/media/{ticket}", get(runtime::media))
}

async fn no_store(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
}

async fn installed(state: &AppState, id: &str) -> AppResult<InstalledApp> {
    InstalledApp::from_record(state.vfs.db.app(id).await?.ok_or(AppError::NotFound)?)
}

fn cache_root(state: &AppState) -> PathBuf {
    std::path::Path::new(&state.rt.env.db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("apps")
}

async fn cache_dir(state: &AppState, app: &InstalledApp) -> AppResult<PathBuf> {
    let root = cache_root(state);
    let target = root.join(&app.manifest.id).join(&app.digest);
    if tokio::fs::try_exists(target.join(".complete")).await? {
        return Ok(target);
    }
    let _guard = CACHE_LOCK.lock().await;
    if tokio::fs::try_exists(target.join(".complete")).await? {
        return Ok(target);
    }
    // 数据库恢复到新机器后，第一次打开应用时从备份中的安装包重建缓存。
    let archive = state
        .vfs
        .db
        .app_package(&app.manifest.id, &app.digest)
        .await?;
    let digest = app.digest.clone();
    blocking(move || {
        let package = AppPackage::parse(archive)?;
        if package.digest != digest {
            return Err(AppError::Internal(anyhow::anyhow!(
                "数据库中的应用安装包校验失败"
            )));
        }
        package.materialize(&root)
    })
    .await
}

async fn index(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let records = state.vfs.db.apps().await?;
    let (catalog_status, available) = state.apps_catalog.available_apps(&records).await;
    let installed: Vec<_> = records
        .into_iter()
        .map(InstalledApp::from_record)
        .collect::<AppResult<_>>()?;
    Ok(Json(json!({
        "installed": installed,
        "available": available,
        "catalog": catalog_status,
        "api_version": package::API_VERSION,
        "supported_api_versions": [1, 2],
        "host_version": env!("CARGO_PKG_VERSION"),
    })))
}

#[derive(Deserialize)]
struct RefreshOptions {
    #[serde(default)]
    force: bool,
}

async fn catalog_refresh(
    State(state): State<AppState>,
    Json(options): Json<RefreshOptions>,
) -> AppResult<Json<Value>> {
    state.apps_catalog.refresh(options.force).await;
    index(State(state)).await
}

async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<InstalledApp>> {
    Ok(Json(installed(&state, &id).await?))
}

async fn preview(state: &AppState, package: &AppPackage) -> AppResult<Value> {
    let existing = state
        .vfs
        .db
        .app(&package.manifest.id)
        .await?
        .map(InstalledApp::from_record)
        .transpose()?;
    let added_permissions: Vec<_> = package
        .manifest
        .permissions
        .iter()
        .filter(|permission| {
            existing
                .as_ref()
                .is_none_or(|app| !app.manifest.permissions.contains(permission))
        })
        .cloned()
        .collect();
    Ok(
        json!({"manifest": package.manifest, "digest": package.digest,
        "existing": existing, "added_permissions": added_permissions}),
    )
}

async fn inspect(State(state): State<AppState>, bytes: Bytes) -> AppResult<Json<Value>> {
    let package = blocking(move || AppPackage::parse(bytes.to_vec())).await?;
    Ok(Json(preview(&state, &package).await?))
}

#[derive(Deserialize)]
struct InstallOptions {
    digest: String,
    expected_revision: Option<String>,
}

async fn install_package(
    state: &AppState,
    package: AppPackage,
    options: InstallOptions,
    source: &str,
) -> AppResult<InstalledApp> {
    if package.digest != options.digest {
        return Err(AppError::Conflict(
            "插件包已经变化，请重新查看安装信息".into(),
        ));
    }
    let _guard = INSTALL_LOCK.lock().await;
    let id = package.manifest.id.clone();
    let root = cache_root(state);
    let package = {
        let _cache_guard = CACHE_LOCK.lock().await;
        blocking(move || {
            package.materialize(&root)?;
            Ok(package)
        })
        .await?
    };
    let manifest =
        serde_json::to_string(&package.manifest).map_err(|e| AppError::Internal(e.into()))?;
    state
        .vfs
        .db
        .install_app(AppInstall {
            id: &id,
            manifest: &manifest,
            version: &package.manifest.version,
            digest: &package.digest,
            package: &package.archive,
            source,
            expected_revision: options.expected_revision.as_deref(),
        })
        .await?;
    installed(state, &id).await
}

async fn install(
    State(state): State<AppState>,
    Query(options): Query<InstallOptions>,
    bytes: Bytes,
) -> AppResult<Json<InstalledApp>> {
    let package = blocking(move || AppPackage::parse(bytes.to_vec())).await?;
    Ok(Json(
        install_package(&state, package, options, "import").await?,
    ))
}

#[derive(Default, Deserialize)]
struct CatalogPackageQuery {
    digest: Option<String>,
}

async fn catalog_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CatalogPackageQuery>,
) -> AppResult<Response> {
    let (manifest, _digest, archive) = state
        .apps_catalog
        .get_package(&id, query.digest.as_deref())
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}-{}.tgapp\"",
                    manifest.id, manifest.version
                ),
            ),
        ],
        archive,
    )
        .into_response())
}

async fn catalog_install(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(options): Json<InstallOptions>,
) -> AppResult<Json<InstalledApp>> {
    // 绝不持锁等待网络：下载与摘要验证在获取安装锁之前完成
    let (_manifest, digest, archive) = state
        .apps_catalog
        .get_package(&id, Some(&options.digest))
        .await?;
    if digest != options.digest {
        return Err(AppError::Conflict(
            "插件包已经变化，请重新查看安装信息".into(),
        ));
    }
    let package = blocking(move || AppPackage::parse(archive)).await?;
    if package.digest != options.digest {
        return Err(AppError::Conflict(
            "插件包已经变化，请重新查看安装信息".into(),
        ));
    }
    Ok(Json(
        install_package(&state, package, options, "catalog").await?,
    ))
}

#[derive(Deserialize)]
struct StatePatch {
    enabled: Option<bool>,
    pinned: Option<bool>,
}

async fn set_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<StatePatch>,
) -> AppResult<Json<InstalledApp>> {
    let _guard = INSTALL_LOCK.lock().await;
    state
        .vfs
        .db
        .set_app_state(&id, patch.enabled, patch.pinned)
        .await?;
    Ok(Json(installed(&state, &id).await?))
}

#[derive(Default, Deserialize)]
struct UninstallOptions {
    #[serde(default)]
    purge_data: bool,
}

async fn uninstall(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(options): Query<UninstallOptions>,
) -> AppResult<Json<Value>> {
    let _guard = INSTALL_LOCK.lock().await;
    let app = installed(&state, &id).await?;
    state.vfs.db.uninstall_app(&id, options.purge_data).await?;
    let _cache_guard = CACHE_LOCK.lock().await;
    let path = cache_root(&state).join(&app.manifest.id);
    if let Err(error) = tokio::fs::remove_dir_all(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(app_id = %id, %error, "应用已卸载，运行缓存稍后可手动清理");
    }
    Ok(Json(json!({"ok": true})))
}

async fn read_settings(state: &AppState, app: &InstalledApp) -> AppResult<Value> {
    let saved = state.vfs.db.app_settings(&app.manifest.id).await?;
    Ok(Value::Object(app.manifest.settings_values(&saved)?))
}

async fn save_settings(state: &AppState, app: &InstalledApp, values: Value) -> AppResult<Value> {
    let patch = app.manifest.validate_patch(values)?;
    let encoded = serde_json::to_string(&patch).map_err(|e| AppError::Internal(e.into()))?;
    state
        .vfs
        .db
        .patch_app_settings(&app.manifest.id, &app.revision, &encoded)
        .await?;
    read_settings(state, app).await
}

async fn settings_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        read_settings(&state, &installed(&state, &id).await?).await?,
    ))
}

async fn settings_patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(values): Json<Value>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        save_settings(&state, &installed(&state, &id).await?, values).await?,
    ))
}

#[cfg(test)]
mod tests;
