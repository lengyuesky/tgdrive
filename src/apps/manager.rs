//! 应用中心：精确预览、原子安装、显式授权、启停、固定与卸载。

use super::package::{VerifiedPackage, verify_and_materialize};
use super::store::{Installation, installation, installed_app, scope_info};
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use std::path::Path;
use tgdrive_contract::files::{AccessLevel, DirectoryScope};
use tgdrive_contract::http::*;
use tgdrive_contract::runtime::*;
use tgdrive_contract::validation::{check_manifest_compatibility, stable_version, valid_app_id};
use tgdrive_contract::{CONTRACT_VERSION, Timestamp, error_code};

#[derive(serde::Serialize)]
pub(crate) struct BackendManagementInfo {
    pub status: BackendStatus,
    pub limits: BackendResourceLimits,
}

impl AppPlatform {
    pub(crate) async fn backend_management(
        &self,
        id: &str,
    ) -> PlatformResult<BackendManagementInfo> {
        let mut connection = self.host.vfs.db.pool.acquire().await?;
        let app = installation(&mut connection, id).await?;
        let backend = app
            .manifest()?
            .backend
            .ok_or_else(|| PlatformError::new(error_code::CALLER_NOT_ALLOWED, "应用没有后端"))?;
        let mut limits = super::runtime::limits(&app, self.shared_limits().await?.values())?
            .backend
            .expect("已核对后端声明");
        if let Some(run) = self.backends.run(id) {
            limits = run.limits.read().unwrap().clone();
        }
        Ok(BackendManagementInfo {
            status: self.backends.status(id, backend.run_mode),
            limits,
        })
    }
    fn decorate_backend(&self, app: &mut InstalledApp) {
        if let Some(backend) = &app.manifest.backend {
            app.backend = Some(self.backends.status(&app.manifest.id, backend.run_mode));
        }
    }
    /// 保存宿主唯一一组应用资源限制，并立刻对运行中的全部后端生效。
    pub async fn resource_limits_update(
        &self,
        request: AppResourceLimitsUpdateRequest,
    ) -> PlatformResult<AppResourceLimits> {
        let values = super::runtime::SharedLimitValues {
            memory_bytes: request.memory_bytes.0,
            cpu_millis: request.cpu_millis,
        }
        .validate()?;
        let limits = self.shared_limits().await?;
        let _saving = limits.update.lock().await;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        for (key, value) in [
            (
                super::runtime::SHARED_MEMORY_KEY,
                values.memory_bytes.to_string(),
            ),
            (
                super::runtime::SHARED_CPU_KEY,
                values.cpu_millis.to_string(),
            ),
        ] {
            sqlx::query(
                "INSERT INTO settings (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        limits.set(values);
        let report = limits.report();
        self.backends.update_shared_limits(&report);
        Ok(report)
    }
    pub async fn root_selection(&self) -> PlatformResult<RootAppSelection> {
        let (app_id, revision): (Option<String>, String) =
            sqlx::query_as("SELECT app_id,revision FROM app_root_selection WHERE id=1")
                .fetch_one(&self.host.vfs.db.pool)
                .await?;
        Ok(RootAppSelection { app_id, revision })
    }
    pub async fn root_update(
        &self,
        request: RootAppUpdateRequest,
    ) -> PlatformResult<RootAppSelection> {
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (previous, current): (Option<String>, String) =
            sqlx::query_as("SELECT app_id,revision FROM app_root_selection WHERE id=1")
                .fetch_one(&mut *tx)
                .await?;
        if current != request.expected_revision {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "根地址选择已改变",
            ));
        }
        if let Some(id) = &request.app_id {
            let app = installation(&mut tx, id).await?;
            if !app.enabled
                || !app
                    .manifest()?
                    .backend
                    .is_some_and(|backend| backend.external_service)
            {
                return Err(PlatformError::new(
                    error_code::CALLER_NOT_ALLOWED,
                    "根地址需要已启用并声明对外服务的应用后端",
                ));
            }
        }
        let revision = opaque();
        sqlx::query("UPDATE app_root_selection SET app_id=?,revision=? WHERE id=1")
            .bind(&request.app_id)
            .bind(&revision)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if previous != request.app_id {
            for id in previous.iter().chain(request.app_id.iter()) {
                self.events.emit(
                    id,
                    tgdrive_contract::events::event_name::EXTERNAL_SERVICE_CHANGED,
                    &tgdrive_contract::EmptyResponse {},
                )?;
            }
        }
        Ok(RootAppSelection {
            app_id: request.app_id,
            revision,
        })
    }
    pub(crate) async fn backend_logs(&self, id: &str) -> PlatformResult<serde_json::Value> {
        let app = self.installed(id).await?;
        if app.manifest.backend.is_none() {
            return Err(PlatformError::new(
                error_code::CALLER_NOT_ALLOWED,
                "应用没有后端日志",
            ));
        }
        let empty = || serde_json::json!({"stdout":"","stderr":"","truncated":false});
        let root = self.root.join("logs");
        if !root.is_dir() {
            return Ok(empty());
        }
        let directory = match super::safe_fs::Directory::open(&root, false)?.child(id, false) {
            Ok(directory) => directory,
            Err(error) if error.code == error_code::NOT_FOUND => return Ok(empty()),
            Err(error) => return Err(error),
        };
        let mut truncated = false;
        let mut read = |source: &str| -> PlatformResult<String> {
            use std::io::{Read, Seek, SeekFrom};
            let mut output = Vec::new();
            for name in [
                format!("{source}.log"),
                format!("{source}.1.log"),
                format!("{source}.2.log"),
            ] {
                let mut file = match directory.read(&name) {
                    Ok(file) => file,
                    Err(error) if error.code == error_code::NOT_FOUND => continue,
                    Err(error) => return Err(error),
                };
                let left = 64 * 1024 - output.len();
                let size = file.metadata()?.len();
                let length = size.min(left as u64);
                truncated |= length < size;
                file.seek(SeekFrom::End(-(length as i64)))?;
                let mut bytes = Vec::new();
                file.take(length).read_to_end(&mut bytes)?;
                bytes.extend(output);
                output = bytes;
                if output.len() == 64 * 1024 {
                    break;
                }
            }
            Ok(String::from_utf8_lossy(&output).into_owned())
        };
        let stdout = read("stdout")?;
        let stderr = read("stderr")?;
        Ok(serde_json::json!({"stdout":stdout,"stderr":stderr,"truncated":truncated}))
    }
    pub async fn installed(&self, id: &str) -> PlatformResult<InstalledApp> {
        if !valid_app_id(id) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "应用 ID 无效",
            ));
        }
        let mut connection = self.host.vfs.db.pool.acquire().await?;
        let record = installation(&mut connection, id).await?;
        let mut result = installed_app(&mut connection, record).await?;
        self.decorate_backend(&mut result);
        Ok(result)
    }
    pub async fn installed_apps(&self) -> PlatformResult<InstalledAppsResponse> {
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let records: Vec<Installation> =
            sqlx::query_as("SELECT * FROM app_installations ORDER BY pinned DESC,installed_at,id")
                .fetch_all(&mut *tx)
                .await?;
        let mut apps = Vec::with_capacity(records.len());
        for record in records {
            let mut result = installed_app(&mut tx, record).await?;
            self.decorate_backend(&mut result);
            apps.push(result);
        }
        tx.commit().await?;
        Ok(InstalledAppsResponse {
            apps,
            root_app: self.root_selection().await?,
        })
    }
    pub async fn verify_package(&self, path: &Path) -> PlatformResult<VerifiedPackage> {
        let path = path.to_owned();
        let root = self.package_root.clone();
        let verified = tokio::task::spawn_blocking(move || verify_and_materialize(&path, &root))
            .await
            .map_err(|_| PlatformError::new(error_code::INTERNAL_ERROR, "包校验任务失败"))??;
        debug_assert!(verified.directory.starts_with(&self.package_root));
        Ok(verified)
    }
    pub async fn upload_package(&self, path: &Path) -> PlatformResult<PackageUploadReceipt> {
        let _gate = self.lifecycle.lock().await;
        let verified = self.verify_package(path).await?;
        let receipt = PackageUploadReceipt {
            package_id: opaque(),
            manifest: verified.manifest.clone(),
            package: verified.package.clone(),
            expires_at: Timestamp(now().0 + PACKAGE_STAGING_SECONDS as i64 * 1000),
        };
        tokio::fs::create_dir_all(&self.download_root).await?;
        let zip = self
            .download_root
            .join(format!("{}.tgapp", verified.package.sha256));
        if !zip.is_file() {
            tokio::fs::rename(path, &zip).await?;
        }
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        bind_package(&mut tx, &verified).await?;
        sqlx::query(
            "INSERT INTO app_package_uploads(id,manifest,package,expires_at) VALUES(?,?,?,?)",
        )
        .bind(&receipt.package_id)
        .bind(serde_json::to_string(&receipt.manifest)?)
        .bind(serde_json::to_string(&receipt.package)?)
        .bind(receipt.expires_at.0)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM app_package_uploads WHERE expires_at<=?")
            .bind(now().0)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM app_install_previews WHERE expires_at<=?")
            .bind(now().0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(receipt)
    }
    pub async fn preview(&self, request: InstallPreviewRequest) -> PlatformResult<InstallPreview> {
        let _gate = self.lifecycle.lock().await;
        let (path, expected_manifest, expected_package) = match request.source {
            InstallSource::Uploaded { package_id } => {
                let row: Option<(String, String, i64)> = sqlx::query_as(
                    "SELECT manifest,package,expires_at FROM app_package_uploads WHERE id=?",
                )
                .bind(package_id)
                .fetch_optional(&self.host.vfs.db.pool)
                .await?;
                let (manifest, package, expiry) = row.ok_or_else(|| {
                    PlatformError::new(error_code::PACKAGE_CHANGED, "上传包已失效，请重新上传")
                })?;
                if expiry <= now().0 {
                    return Err(PlatformError::new(
                        error_code::PACKAGE_CHANGED,
                        "上传包已过期，请重新上传",
                    ));
                }
                let manifest = serde_json::from_str(&manifest)?;
                let package: InstalledPackage = serde_json::from_str(&package)?;
                (
                    self.download_root.join(format!("{}.tgapp", package.sha256)),
                    manifest,
                    package,
                )
            }
            InstallSource::Catalog {
                app_id,
                version,
                target,
                sha256,
            } => {
                let catalog = self.catalog().await?;
                let current = catalog.current().await;
                let entry = current
                    .result
                    .catalog
                    .entries
                    .iter()
                    .find(|e| e.manifest.id == app_id && e.manifest.version == version)
                    .ok_or_else(|| {
                        PlatformError::new(
                            error_code::PACKAGE_CHANGED,
                            "已选择的目录版本不再可用，请重新预览",
                        )
                    })?;
                let artifact = entry
                    .packages
                    .get(target.as_str())
                    .filter(|artifact| artifact.sha256 == sha256)
                    .ok_or_else(|| {
                        PlatformError::new(error_code::PACKAGE_CHANGED, "已选择的包绑定已改变")
                    })?;
                let path = catalog.download(artifact).await?;
                (
                    path,
                    entry.manifest.clone(),
                    InstalledPackage {
                        target,
                        sha256,
                        size: artifact.size,
                    },
                )
            }
        };
        let verified = self.verify_package(&path).await?;
        if verified.manifest != expected_manifest || verified.package != expected_package {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "实际包与已选择的完整清单或摘要不符",
            ));
        }
        check_manifest_compatibility(
            &verified.manifest,
            CONTRACT_VERSION,
            &super::runtime::capabilities(),
        )?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        bind_package(&mut tx, &verified).await?;
        let previous: Option<Installation> =
            sqlx::query_as("SELECT * FROM app_installations WHERE id=?")
                .bind(&verified.manifest.id)
                .fetch_optional(&mut *tx)
                .await?;
        check_update(previous.as_ref(), &verified)?;
        let old_permissions = previous
            .as_ref()
            .map(Installation::manifest)
            .transpose()?
            .map(|m| m.files)
            .unwrap_or_default();
        let preview = InstallPreview {
            preview_id: opaque(),
            expires_at: Timestamp(now().0 + INSTALL_PREVIEW_SECONDS as i64 * 1000),
            manifest: verified.manifest.clone(),
            package: verified.package,
            previous_state_revision: previous.map(|p| p.state_revision),
            added_file_read: verified.manifest.files.read && !old_permissions.read,
            added_file_write: verified.manifest.files.write && !old_permissions.write,
            starts_backend: verified.manifest.backend.is_some(),
            external_network: verified.manifest.backend.is_some(),
        };
        sqlx::query("INSERT INTO app_install_previews(id,preview,expires_at) VALUES(?,?,?)")
            .bind(&preview.preview_id)
            .bind(serde_json::to_string(&preview)?)
            .bind(preview.expires_at.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(preview)
    }
    pub async fn install(&self, request: InstallRequest) -> PlatformResult<InstalledApp> {
        let _gate = self.lifecycle.lock().await;
        let row: Option<(String, i64)> =
            sqlx::query_as("SELECT preview,expires_at FROM app_install_previews WHERE id=?")
                .bind(&request.preview_id)
                .fetch_optional(&self.host.vfs.db.pool)
                .await?;
        let (preview, expiry) =
            row.ok_or_else(|| PlatformError::new(error_code::PACKAGE_CHANGED, "安装预览已失效"))?;
        if expiry <= now().0 {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "安装预览已过期，请重新预览",
            ));
        }
        let preview: InstallPreview = serde_json::from_str(&preview)?;
        let path = self
            .download_root
            .join(format!("{}.tgapp", preview.package.sha256));
        let verified = self.verify_package(&path).await?;
        if preview.manifest != verified.manifest || preview.package != verified.package {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "已确认的应用包已改变",
            ));
        }
        check_manifest_compatibility(
            &preview.manifest,
            CONTRACT_VERSION,
            &super::runtime::capabilities(),
        )?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<Installation> =
            sqlx::query_as("SELECT * FROM app_installations WHERE id=?")
                .bind(&preview.manifest.id)
                .fetch_optional(&mut *tx)
                .await?;
        if previous.as_ref().map(|p| &p.state_revision) != preview.previous_state_revision.as_ref()
        {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "旧安装状态已改变，请重新预览",
            ));
        }
        check_update(previous.as_ref(), &verified)?;
        bind_package(&mut tx, &verified).await?;
        if let Some(previous) = &previous
            && previous.manifest()?.version == preview.manifest.version
        {
            return installed_app(&mut tx, previous.clone()).await;
        }
        let mut scope = previous
            .as_ref()
            .map(Installation::directory_scope)
            .transpose()?
            .unwrap_or(DirectoryScope::None);
        if !preview.manifest.files.read {
            scope = DirectoryScope::None;
        } else if !preview.manifest.files.write {
            match &mut scope {
                DirectoryScope::Selected { directories } => {
                    for grant in directories {
                        grant.access = AccessLevel::ReadOnly;
                    }
                }
                DirectoryScope::All { access } => *access = AccessLevel::ReadOnly,
                DirectoryScope::None => {}
            }
        }
        self.backends.stop(&preview.manifest.id).await?;
        self.backends.reset_failures(&preview.manifest.id);
        let at = now();
        let generation = opaque();
        let state_revision = opaque();
        let scope_revision = opaque();
        sqlx::query("INSERT INTO app_installations(id,manifest,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET manifest=excluded.manifest,generation=excluded.generation,state_revision=excluded.state_revision,scope=excluded.scope,scope_revision=excluded.scope_revision,target=excluded.target,package_sha256=excluded.package_sha256,package_size=excluded.package_size,updated_at=excluded.updated_at")
            .bind(&preview.manifest.id).bind(serde_json::to_string(&preview.manifest)?).bind(&generation).bind(&state_revision).bind(previous.as_ref().is_none_or(|p|p.enabled)).bind(previous.as_ref().is_some_and(|p|p.pinned)).bind(serde_json::to_string(&scope)?).bind(&scope_revision).bind(preview.package.target.as_str()).bind(&preview.package.sha256).bind(preview.package.size.0 as i64).bind(previous.as_ref().map_or(at.0,|p|p.installed_at)).bind(at.0).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM app_install_previews WHERE id=?")
            .bind(&request.preview_id)
            .execute(&mut *tx)
            .await?;
        let record = installation(&mut tx, &preview.manifest.id).await?;
        let result = installed_app(&mut tx, record).await?;
        tx.commit().await?;
        self.sessions
            .revoke_other_generations(&preview.manifest.id, &result.generation);
        Ok(result)
    }
    pub async fn state_patch(
        &self,
        id: &str,
        request: AppStatePatch,
    ) -> PlatformResult<InstalledApp> {
        let _gate = self.lifecycle.lock().await;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let old = installation(&mut tx, id).await?;
        if old.state_revision != request.expected_state_revision {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "应用状态已改变",
            ));
        }
        let enabled = request.enabled.unwrap_or(old.enabled);
        let pinned = request.pinned.unwrap_or(old.pinned);
        if enabled == old.enabled && pinned == old.pinned {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "启停或固定至少需要一个变化",
            ));
        }
        if enabled != old.enabled {
            self.backends.stop(id).await?;
            self.backends.reset_failures(id);
        }
        let generation = if enabled != old.enabled {
            opaque()
        } else {
            old.generation
        };
        sqlx::query("UPDATE app_installations SET enabled=?,pinned=?,generation=?,state_revision=?,updated_at=? WHERE id=?").bind(enabled).bind(pinned).bind(generation).bind(opaque()).bind(now().0).bind(id).execute(&mut *tx).await?;
        let updated = installation(&mut tx, id).await?;
        let result = installed_app(&mut tx, updated).await?;
        tx.commit().await?;
        if enabled != old.enabled {
            self.sessions
                .revoke_other_generations(id, &result.generation);
        }
        Ok(result)
    }
    pub async fn uninstall(
        &self,
        id: &str,
        state_revision: &str,
        purge: bool,
    ) -> PlatformResult<()> {
        let _gate = self.lifecycle.lock().await;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let old = installation(&mut tx, id).await?;
        if old.state_revision != state_revision {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "应用状态已改变",
            ));
        }
        let was_root: bool =
            sqlx::query_scalar("SELECT COALESCE(app_id=?,0) FROM app_root_selection WHERE id=1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        self.backends.stop(id).await?;
        sqlx::query("DELETE FROM app_installations WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if purge {
            sqlx::query("DELETE FROM app_kv WHERE app_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM app_kv_usage WHERE app_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.sessions.revoke_app(id);
        if was_root {
            self.events.emit(
                id,
                tgdrive_contract::events::event_name::EXTERNAL_SERVICE_CHANGED,
                &tgdrive_contract::EmptyResponse {},
            )?;
        }
        self.clear_app_cache(id).await?;
        super::safe_fs::remove_tree(&self.root.join("logs"), id)?;
        let identity: Option<i64> =
            sqlx::query_scalar("SELECT uid FROM app_runtime_identities WHERE app_id=?")
                .bind(id)
                .fetch_optional(&self.host.vfs.db.pool)
                .await?;
        let owner = identity.map(|uid| {
            if super::backend_system::real_isolation() {
                uid as u32
            } else {
                unsafe { libc::geteuid() }
            }
        });
        super::safe_fs::remove_tree_owner(&self.data_root().join(id), "run", owner)?;
        if purge {
            super::safe_fs::remove_tree_owner(&self.data_root(), id, owner)?;
        }
        let references: i64 =
            sqlx::query_scalar("SELECT count(*) FROM app_installations WHERE package_sha256=?")
                .bind(&old.package_sha256)
                .fetch_one(&self.host.vfs.db.pool)
                .await?;
        if references == 0 {
            super::safe_fs::remove_tree(&self.package_root, &old.package_sha256)?;
        }
        Ok(())
    }
    pub async fn launch(
        &self,
        id: &str,
        request: LaunchPageRequest,
        host_origin: String,
    ) -> PlatformResult<LaunchPageResponse> {
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = installation(&mut tx, id).await?;
        app.check(&app.generation)?;
        let manifest = app.manifest()?;
        let page = manifest.page.ok_or_else(|| {
            PlatformError::new(error_code::CALLER_NOT_ALLOWED, "该应用没有应用页面")
        })?;
        let launch = if let Some(id) = request.open_node_id {
            let file = super::store::reference(
                &mut tx,
                &app,
                &tgdrive_contract::files::NodeReference::Id {
                    id,
                    content_version: None,
                },
            )
            .await?;
            LaunchAction::Open { file }
        } else {
            LaunchAction::Browse
        };
        let session = self.sessions.create(&app, self.host.auth.device_epoch());
        let token = super::runtime::Sessions::token(&self.cursors, &session)?;
        let bootstrap = PageBootstrap {
            contract: CONTRACT_VERSION,
            app: app.identity()?,
            session_token: token,
            expires_at: Timestamp(
                session
                    .expires_at
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
            host_origin: host_origin.clone(),
            api_base: format!("{host_origin}{SELF_API_BASE}"),
            backend_base: manifest
                .backend
                .as_ref()
                .map(|_| format!("{host_origin}{SELF_API_BASE}/backend")),
            ui_nonce: session.id.clone(),
            theme: Theme::Light,
            launch,
        };
        use base64::Engine;
        let fragment = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&bootstrap)?);
        let asset = super::endpoint_path(
            "assets",
            &[
                ("app_id", &app.id),
                ("sha256", &app.package_sha256),
                ("path", &page.entry),
            ],
        );
        let frame_url = format!(
            "{asset}#{}={fragment}",
            tgdrive_contract::ui::BOOTSTRAP_FRAGMENT_KEY
        );
        tx.commit().await?;
        Ok(LaunchPageResponse {
            frame_url,
            bootstrap,
        })
    }
    pub async fn app_scope(&self, id: &str) -> PlatformResult<tgdrive_contract::files::ScopeInfo> {
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let record = installation(&mut tx, id).await?;
        let result = scope_info(&mut tx, &record).await?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn bind_package(
    connection: &mut sqlx::SqliteConnection,
    verified: &VerifiedPackage,
) -> PlatformResult<()> {
    let bindings: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT target,sha256,size,manifest FROM app_package_bindings WHERE app_id=? AND version=?",
    )
    .bind(&verified.manifest.id)
    .bind(&verified.manifest.version)
    .fetch_all(&mut *connection)
    .await?;
    for (target, sha, size, manifest) in bindings {
        let manifest: tgdrive_contract::manifest::Manifest = serde_json::from_str(&manifest)?;
        if manifest != verified.manifest
            || (target == verified.package.target.as_str()
                && (sha != verified.package.sha256 || size != verified.package.size.0 as i64))
        {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "同版本完整清单或目标包不可替换",
            ));
        }
    }
    sqlx::query("INSERT INTO app_package_bindings(app_id,version,target,manifest,sha256,size) VALUES(?,?,?,?,?,?) ON CONFLICT(app_id,version,target) DO NOTHING").bind(&verified.manifest.id).bind(&verified.manifest.version).bind(verified.package.target.as_str()).bind(serde_json::to_string(&verified.manifest)?).bind(&verified.package.sha256).bind(verified.package.size.0 as i64).execute(connection).await?;
    Ok(())
}
fn check_update(previous: Option<&Installation>, verified: &VerifiedPackage) -> PlatformResult<()> {
    if let Some(previous) = previous {
        let old = previous.manifest()?;
        let a = stable_version(&old.version)
            .ok_or_else(|| PlatformError::new(error_code::INVALID_MANIFEST, "既有版本无效"))?;
        let b = stable_version(&verified.manifest.version)
            .ok_or_else(|| PlatformError::new(error_code::INVALID_MANIFEST, "新版本无效"))?;
        if b < a {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "更新版本必须高于已安装版本",
            ));
        }
        if b == a
            && (old != verified.manifest
                || previous.package_sha256 != verified.package.sha256
                || previous.target != verified.package.target.as_str())
        {
            return Err(PlatformError::new(
                error_code::PACKAGE_CHANGED,
                "同版本的安装内容不可更换",
            ));
        }
    }
    Ok(())
}
