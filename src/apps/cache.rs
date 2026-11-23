//! 原子二进制小对象缓存。元数据可再生、不进主库，来源每次读取都重新核验。

use super::runtime::CallerSession;
use super::safe_fs::Directory;
use super::store::{self, Installation};
use super::{AppPlatform, PlatformError, PlatformResult, now, opaque};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use tgdrive_contract::data::*;
use tgdrive_contract::http::*;
use tgdrive_contract::validation::{
    parse_unique_json, valid_content_type, valid_kv_key, validate_content_ref,
};
use tgdrive_contract::{ByteCount, error_code, files::*};
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    entry: CacheEntry,
    sources: Vec<ContentRef>,
    file: String,
    last_access: i64,
}
impl Stored {
    fn bytes(&self) -> PlatformResult<u64> {
        Ok(self.entry.size.0 + serde_json::to_vec(self)?.len() as u64)
    }
}
pub(crate) struct CacheIndex {
    directory: Directory,
    entries: BTreeMap<String, Stored>,
    usage: u64,
}
pub(crate) struct CacheRead {
    pub entry: CacheEntry,
    pub sources: Vec<ContentRef>,
    pub bytes: Bytes,
}
fn object_name(key: &str) -> String {
    crate::engine::upload::to_hex(&Sha256::digest(key.as_bytes()))
}
fn metadata_name(key: &str) -> String {
    format!("{}.json", object_name(key))
}

impl CacheIndex {
    fn load(directory: Directory) -> PlatformResult<Self> {
        let mut entries = BTreeMap::new();
        let mut usage = 0u64;
        for name in directory.names()? {
            if !name.ends_with(".json") {
                continue;
            }
            let result = (|| {
                let bytes = directory.read_bytes(&name, MAX_JSON_REQUEST_BYTES)?;
                let value = parse_unique_json(&bytes)?;
                let stored: Stored = serde_json::from_value(value)?;
                tgdrive_contract::validation::validate_cache_entry_header(&stored.entry)?;
                if !valid_kv_key(&stored.entry.key)
                    || metadata_name(&stored.entry.key) != name
                    || stored.entry.size.0 > MAX_CACHE_OBJECT_BYTES
                    || stored.sources.len() as u64 > MAX_CACHE_SOURCES
                    || stored.file
                        != format!(
                            "{}-{}.bin",
                            object_name(&stored.entry.key),
                            stored.entry.revision
                        )
                {
                    return Err(PlatformError::new(
                        error_code::INVALID_REQUEST,
                        "可再生缓存元数据无效",
                    ));
                }
                let file = directory.read(&stored.file)?;
                if file.metadata()?.len() != stored.entry.size.0 {
                    return Err(PlatformError::new(
                        error_code::NOT_FOUND,
                        "缓存对象长度不符",
                    ));
                }
                Ok(stored)
            })();
            if let Ok(stored) = result {
                usage = usage.saturating_add(stored.bytes()?);
                entries.insert(stored.entry.key.clone(), stored);
            } else {
                directory.remove(&name)?;
            }
        }
        // 原子覆盖或崩溃留下的孤儿二进制可以直接删除，绝不当成应用业务数据。
        let retained = entries
            .values()
            .map(|stored| stored.file.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for name in directory.names()? {
            if (name.ends_with(".bin") && !retained.contains(name.as_str()))
                || name.starts_with("tmp-")
            {
                directory.remove(&name)?;
            }
        }
        Ok(Self {
            directory,
            entries,
            usage,
        })
    }
    pub(crate) fn invalidate(&mut self) {
        self.entries.clear();
        self.usage = 0;
    }
    fn remove(&mut self, key: &str) -> PlatformResult<()> {
        if let Some(stored) = self.entries.remove(key) {
            self.usage = self.usage.saturating_sub(stored.bytes()?);
            self.directory.remove(&metadata_name(key))?;
            self.directory.remove(&stored.file)?;
        }
        Ok(())
    }
}

impl AppPlatform {
    async fn cache_index(&self, app_id: &str) -> PlatformResult<Arc<Mutex<CacheIndex>>> {
        if !tgdrive_contract::validation::valid_app_id(app_id) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "应用 ID 无效",
            ));
        }
        let mut indices = self.cache_indexes.lock().await;
        if let Some(index) = indices.get(app_id) {
            return Ok(index.clone());
        }
        let root = self.data_root();
        let id = app_id.to_owned();
        let index = tokio::task::spawn_blocking(move || {
            let root = Directory::open(&root, true)?;
            let app = root.child(&id, true)?;
            CacheIndex::load(app.child("cache", true)?.child("objects", true)?)
        })
        .await
        .map_err(|_| PlatformError::new(error_code::INTERNAL_ERROR, "缓存索引任务失败"))??;
        let index = Arc::new(Mutex::new(index));
        indices.insert(app_id.into(), index.clone());
        Ok(index)
    }
    pub(crate) async fn sources_current(
        connection: &mut sqlx::SqliteConnection,
        app: &Installation,
        sources: &[ContentRef],
    ) -> PlatformResult<bool> {
        for source in sources {
            let result = store::reference(
                connection,
                app,
                &NodeReference::Id {
                    id: source.id.clone(),
                    content_version: Some(source.content_version.clone()),
                },
            )
            .await;
            match result {
                Ok(node) if node.kind == NodeKind::File => {}
                Ok(_) => return Ok(false),
                Err(error) if error.code == error_code::INTERNAL_ERROR => return Err(error),
                Err(_) => return Ok(false),
            }
        }
        Ok(true)
    }
    pub async fn cache_put(
        &self,
        session: &Arc<CallerSession>,
        key: &str,
        content_type: String,
        metadata: CachePutMetadata,
        bytes: Bytes,
    ) -> PlatformResult<CacheEntry> {
        if !valid_kv_key(key) || !valid_content_type(&content_type) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "缓存键或 MIME 无效",
            ));
        }
        if bytes.len() as u64 > MAX_CACHE_OBJECT_BYTES
            || serde_json::to_vec(&metadata.metadata)?.len() as u64 > MAX_CACHE_METADATA_BYTES
        {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "缓存对象或元数据超过上限",
            ));
        }
        if metadata.sources.len() as u64 > MAX_CACHE_SOURCES {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "缓存来源超过上限",
            ));
        }
        for source in &metadata.sources {
            validate_content_ref(source)?;
        }
        let index = self.cache_index(&session.app_id).await?;
        let mut index = index.lock().await;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        for source in &metadata.sources {
            let node = store::reference(
                &mut tx,
                &app,
                &NodeReference::Id {
                    id: source.id.clone(),
                    content_version: None,
                },
            )
            .await?;
            if node.kind != NodeKind::File {
                return Err(PlatformError::new(
                    error_code::IS_DIRECTORY,
                    "缓存来源必须是文件",
                ));
            }
            if node.content_version.as_ref() != Some(&source.content_version) {
                return Err(PlatformError::new(
                    error_code::CONTENT_CHANGED,
                    "缓存来源内容版本已改变",
                ));
            }
        }
        let limits = super::runtime::limits(&app, self.shared_limits().await?.values())?;
        let private_usage = if limits.backend.is_some() {
            let uid: Option<i64> =
                sqlx::query_scalar("SELECT uid FROM app_runtime_identities WHERE app_id=?")
                    .bind(&app.id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let uid = if super::backend_system::real_isolation() {
                uid.unwrap_or(0) as u32
            } else {
                unsafe { libc::geteuid() }
            };
            super::backend_system::disk_usage(
                &self.data_root().join(&app.id),
                super::backend_system::Identity { uid, gid: uid },
            )?
            .1
            .saturating_sub(index.usage)
        } else {
            0
        };
        let available = limits.cache_byte_limit.0.saturating_sub(private_usage);
        let revision = opaque();
        let at = now();
        let entry = CacheEntry {
            key: key.into(),
            content_type,
            size: ByteCount(bytes.len() as u64),
            revision: revision.clone(),
            metadata: metadata.metadata,
            updated_at: at,
        };
        tgdrive_contract::validation::validate_cache_entry_header(&entry)?;
        let stored = Stored {
            entry: entry.clone(),
            sources: metadata.sources,
            file: format!("{}-{revision}.bin", object_name(key)),
            last_access: at.0,
        };
        let length = stored.bytes()?;
        if length > available {
            return Err(PlatformError::new(
                error_code::QUOTA_EXCEEDED,
                "缓存对象超过应用总配额",
            ));
        }
        index.directory.write_atomic(&stored.file, &bytes)?;
        let encoded = serde_json::to_vec(&stored)?;
        index
            .directory
            .write_atomic(&metadata_name(key), &encoded)?;
        if let Some(old) = index.entries.insert(key.into(), stored) {
            index.usage = index.usage.saturating_sub(old.bytes()?);
            index.directory.remove(&old.file)?;
        }
        index.usage += length;
        if index.usage > available {
            let target = (limits.cache_byte_limit.0 / 100 * CACHE_EVICTION_TARGET_PERCENT)
                .saturating_sub(private_usage);
            let mut victims = index
                .entries
                .iter()
                .filter(|(value, _)| *value != key)
                .map(|(key, stored)| (stored.last_access, key.clone()))
                .collect::<Vec<_>>();
            victims.sort();
            for (_, key) in victims {
                if index.usage <= target {
                    break;
                }
                index.remove(&key)?;
            }
        }
        tx.commit().await?;
        self.bump_cache_epoch();
        Ok(entry)
    }
    pub async fn cache_read(
        &self,
        session: &Arc<CallerSession>,
        key: &str,
        body: bool,
        revision: Option<&str>,
    ) -> PlatformResult<CacheRead> {
        if !valid_kv_key(key) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "缓存键无效",
            ));
        }
        let index = self.cache_index(&session.app_id).await?;
        let mut index = index.lock().await;
        let stored = index
            .entries
            .get(key)
            .cloned()
            .filter(|stored| revision.is_none_or(|revision| revision == stored.entry.revision))
            .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "缓存未命中"))?;
        let mut tx = self.host.vfs.db.pool.begin().await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        if !Self::sources_current(&mut tx, &app, &stored.sources).await? {
            index.remove(key)?;
            return Err(PlatformError::new(error_code::NOT_FOUND, "缓存未命中"));
        }
        let file = match index.directory.read(&stored.file) {
            Ok(file) => file,
            Err(error) if error.code == error_code::NOT_FOUND => {
                index.remove(key)?;
                return Err(PlatformError::new(error_code::NOT_FOUND, "缓存未命中"));
            }
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() != stored.entry.size.0 {
            index.remove(key)?;
            return Err(PlatformError::new(error_code::NOT_FOUND, "缓存未命中"));
        }
        let bytes = if body {
            let mut bytes = Vec::new();
            use std::io::Read;
            file.take(MAX_CACHE_OBJECT_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != stored.entry.size.0 {
                return Err(PlatformError::new(error_code::NOT_FOUND, "缓存未命中"));
            }
            Bytes::from(bytes)
        } else {
            Bytes::new()
        };
        if let Some(entry) = index.entries.get_mut(key) {
            entry.last_access = now().0;
        }
        tx.commit().await?;
        Ok(CacheRead {
            entry: stored.entry,
            sources: stored.sources,
            bytes,
        })
    }
    pub async fn cache_delete(
        &self,
        session: &Arc<CallerSession>,
        key: &str,
    ) -> PlatformResult<()> {
        if !valid_kv_key(key) {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "缓存键无效",
            ));
        }
        let index = self.cache_index(&session.app_id).await?;
        let mut index = index.lock().await;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, &session.app_id).await?;
        app.check(&session.generation)?;
        index.remove(key)?;
        tx.commit().await?;
        self.bump_cache_epoch();
        Ok(())
    }
    pub(crate) async fn cache_usage(&self, app_id: &str) -> PlatformResult<u64> {
        let index = self.cache_index(app_id).await?;
        let usage = index.lock().await.usage;
        Ok(usage)
    }
    pub(crate) fn bump_cache_epoch(&self) {
        let epoch = self
            .host
            .vfs
            .db
            .app_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        self.host.vfs.db.app_changes.send_replace(epoch);
    }
}
