//! 新应用平台；线上定义只来自契约库，宿主只实现通用存储与生命周期。

mod backend;
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
mod manager;
mod media;
mod package;
#[cfg(test)]
mod performance_tests;
mod query;
pub(crate) mod restore;
mod runtime;
mod safe_fs;
mod sse;
mod store;
#[cfg(test)]
mod tests;

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
    cache_indexes:
        Mutex<std::collections::BTreeMap<String, std::sync::Arc<Mutex<cache::CacheIndex>>>>,
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
            cache_indexes: Mutex::new(std::collections::BTreeMap::new()),
            host,
        }
    }
    fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }
    async fn catalog(&self) -> PlatformResult<&catalog::CatalogClient> {
        self.catalog_client
            .get_or_try_init(|| {
                catalog::CatalogClient::new(
                    self.host.vfs.db.clone(),
                    self.repository.clone(),
                    runtime::capabilities(),
                    self.download_root.clone(),
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
pub(crate) use http::{boundary_guard, maintenance_response, router};

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
