//! 新应用平台；线上定义只来自契约库，宿主只实现通用存储与生命周期。

pub(crate) mod archive;
#[cfg(test)]
mod archive_conformance_tests;
#[cfg(test)]
mod archive_tests;
mod backend;
#[cfg(test)]
mod backend_management_tests;
pub(crate) mod backend_system;
#[cfg(test)]
mod backend_tests;
#[cfg(test)]
mod budget_tests;
mod cache;
mod catalog;
#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod conformance_tests;
mod cursor;
#[cfg(test)]
mod cursor_plan_tests;
mod error;
mod events;
mod forward;
#[cfg(test)]
mod forward_tests;
mod http;
#[cfg(test)]
mod internal_tests;
mod kv;
#[cfg(all(test, target_os = "linux"))]
mod linux_acceptance_tests;
#[cfg(all(test, target_os = "linux"))]
mod linux_end_to_end_tests;
mod manager;
mod media;
#[cfg(test)]
mod owner_origin_tests;
mod package;
#[cfg(test)]
mod performance_tests;
mod query;
pub(crate) mod restore;
mod runtime;
mod safe_fs;
pub(crate) mod snapshot;
mod sse;
mod store;
#[cfg(test)]
mod tests;
mod uploads;
mod write;
#[cfg(test)]
mod write_conformance_tests;
#[cfg(test)]
mod write_performance;
#[cfg(test)]
mod write_tests;

use crate::api::AppState;
use std::path::PathBuf;
use tokio::sync::{Mutex, OnceCell};

pub(crate) struct AppPlatform {
    pub host: AppState,
    backends: backend::Manager,
    root: PathBuf,
    package_root: PathBuf,
    download_root: PathBuf,
    repository: String,
    lifecycle: Mutex<()>,
    catalog_client: OnceCell<catalog::CatalogClient>,
    cursors: cursor::CursorCodec,
    sessions: runtime::Sessions,
    events: events::EventHub,
    media: media::MediaState,
    uploads: uploads::Uploads,
    cache_indexes:
        Mutex<std::collections::BTreeMap<String, std::sync::Arc<Mutex<cache::CacheIndex>>>>,
    /// 宿主设置里唯一一组应用资源限制，首次使用时从设置表载入。
    shared_limits: OnceCell<runtime::SharedLimits>,
}

impl AppPlatform {
    pub fn new(host: AppState) -> Self {
        Self::with_repository(host, crate::config::app_catalog_repository())
    }
    fn with_repository(host: AppState, repository: String) -> Self {
        let database = PathBuf::from(&host.rt.env.db_path);
        let database = if database.is_absolute() {
            database
        } else {
            std::env::current_dir()
                .expect("宿主工作目录可读取")
                .join(database)
        };
        let parent = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let root = parent.join("apps-v1");
        Self {
            backends: backend::Manager::new(),
            package_root: root.join("packages"),
            download_root: root.join("downloads"),
            root,
            repository,
            lifecycle: Mutex::new(()),
            catalog_client: OnceCell::new(),
            cursors: cursor::CursorCodec::new(
                host.vfs.db.app_cursor_key.clone(),
                host.vfs.db.app_cursor_keys.clone(),
            ),
            sessions: runtime::Sessions::new(),
            events: events::EventHub::new(),
            media: media::MediaState::new(),
            uploads: uploads::Uploads::new(),
            cache_indexes: Mutex::new(std::collections::BTreeMap::new()),
            shared_limits: OnceCell::new(),
            host,
        }
    }
    /// 全部应用后端合计共用的内存与 CPU 上限；首次读取时载入并写入根 cgroup。
    pub(crate) async fn shared_limits(&self) -> PlatformResult<&runtime::SharedLimits> {
        self.shared_limits
            .get_or_try_init(|| async {
                let rows: Vec<(String, String)> =
                    sqlx::query_as("SELECT k,v FROM settings WHERE k IN (?,?)")
                        .bind(runtime::SHARED_MEMORY_KEY)
                        .bind(runtime::SHARED_CPU_KEY)
                        .fetch_all(&self.host.vfs.db.pool)
                        .await?;
                let stored = |key: &str| rows.iter().find(|row| row.0 == key).map(|row| &row.1);
                let default = runtime::SharedLimitValues::default();
                // 设置表里的值无法解析或越界时回到默认值，不让应用因此无法启动。
                let values = runtime::SharedLimitValues {
                    memory_bytes: stored(runtime::SHARED_MEMORY_KEY)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(default.memory_bytes),
                    cpu_millis: stored(runtime::SHARED_CPU_KEY)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(default.cpu_millis),
                }
                .validate()
                .unwrap_or(default);
                Ok(runtime::SharedLimits::new(values))
            })
            .await
    }
    pub(crate) async fn shutdown(&self) -> PlatformResult<()> {
        self.sessions.revoke_all();
        self.backends.stop_all().await?;
        self.prune_uploads().await?;
        Ok(())
    }
    fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }
    async fn backend_database_usage(&self, id: &str) -> PlatformResult<u64> {
        let uid: Option<i64> =
            sqlx::query_scalar("SELECT uid FROM app_runtime_identities WHERE app_id=?")
                .bind(id)
                .fetch_optional(&self.host.vfs.db.pool)
                .await?;
        let uid = if backend_system::real_isolation() {
            uid.unwrap_or(0) as u32
        } else {
            unsafe { libc::geteuid() }
        };
        backend_system::database_usage(
            &self.data_root().join(id),
            backend_system::Identity { uid, gid: uid },
        )
    }
    async fn catalog(&self) -> PlatformResult<&catalog::CatalogClient> {
        self.catalog_client
            .get_or_try_init(|| {
                catalog::CatalogClient::new(
                    self.host.vfs.db.clone(),
                    self.repository.clone(),
                    runtime::capabilities(),
                    self.download_root.clone(),
                    self.host.vfs.tg.proxy_client(),
                )
            })
            .await
    }
    async fn clear_app_cache(&self, id: &str) -> PlatformResult<()> {
        if !tgdrive_contract::validation::valid_app_id(id) {
            return Err(PlatformError::new(
                tgdrive_contract::error_code::INVALID_REQUEST,
                "应用 ID 无效",
            ));
        }
        self.backends.stop(id).await?;
        let owner: Option<i64> =
            sqlx::query_scalar("SELECT uid FROM app_runtime_identities WHERE app_id=?")
                .bind(id)
                .fetch_optional(&self.host.vfs.db.pool)
                .await?;
        let owner = owner.map(|uid| {
            if backend_system::real_isolation() {
                uid as u32
            } else {
                unsafe { libc::geteuid() }
            }
        });
        let mut indices = self.cache_indexes.lock().await;
        let index = indices.remove(id);
        let mut guard = match index.as_ref() {
            Some(index) => Some(index.lock().await),
            None => None,
        };
        if let Some(guard) = &mut guard {
            guard.invalidate();
        }
        let directory = self.data_root().join(id);
        safe_fs::remove_tree_owner(&directory, "cache", owner)?;
        safe_fs::remove_tree_owner(&directory, "tmp", owner)?;
        self.bump_cache_epoch();
        Ok(())
    }
}

pub(crate) use error::{PlatformError, PlatformResult};
pub(crate) use http::{boundary_guard, maintenance_response, router_with_platform};

fn endpoint_path(name: &str, parameters: &[(&str, &str)]) -> String {
    let mut path = tgdrive_contract::http::ENDPOINTS
        .iter()
        .find(|endpoint| endpoint.name == name)
        .expect("接口必须存在于契约注册表")
        .path
        .to_owned();
    for (key, value) in parameters {
        path = path.replace(&format!("{{{key}}}"), value);
    }
    path
}

fn opaque() -> String {
    crate::auth::random_secret()
}

fn now() -> tgdrive_contract::Timestamp {
    tgdrive_contract::Timestamp(crate::db::now_ts().saturating_mul(1000))
}
