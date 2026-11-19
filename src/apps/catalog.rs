//! GitHub 目录、精确 Release 下载与不可变发布历史。下载失败保留上一次成功快照。

use super::{PlatformError, PlatformResult, now};
use crate::db::Db;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tgdrive_contract::error_code;
use tgdrive_contract::http::*;
use tgdrive_contract::manifest::*;
use tgdrive_contract::runtime::CatalogResponse;
use tgdrive_contract::validation::{
    parse_catalog_for_host, parse_unique_json, stable_version, valid_repository,
    validate_catalog_transition,
};
use tgdrive_contract::{CATALOG_FORMAT, CONTRACT_VERSION, Timestamp};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

struct CatalogCache {
    response: CatalogResponse,
    last_check: Option<Instant>,
}

pub(crate) struct CatalogClient {
    db: Db,
    repository: String,
    capabilities: Vec<&'static str>,
    client: Client,
    cache: Mutex<CatalogCache>,
    downloads: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    root: PathBuf,
}

impl CatalogClient {
    pub async fn new(
        db: Db,
        repository: String,
        capabilities: Vec<&'static str>,
        root: PathBuf,
    ) -> PlatformResult<Self> {
        if !repository.is_empty() && !valid_repository(&repository) {
            return Err(PlatformError::new(
                error_code::INVALID_CATALOG,
                "应用目录仓库标识无效",
            ));
        }
        tokio::fs::create_dir_all(&root).await?;
        let snapshot: Option<(String, i64)> = sqlx::query_as(
            "SELECT raw_catalog,last_success_at FROM app_catalog_snapshots WHERE repository=?",
        )
        .bind(&repository)
        .fetch_optional(&db.pool)
        .await?;
        let result = snapshot.as_ref().and_then(|(raw, _)| {
            parse_catalog_for_host(raw.as_bytes(), &repository, CONTRACT_VERSION, &capabilities)
                .ok()
        });
        let last_success_at = snapshot
            .as_ref()
            .filter(|_| result.is_some())
            .map(|(_, at)| Timestamp(*at));
        let state = if repository.is_empty() {
            CatalogState::Disabled
        } else if result.is_some() {
            CatalogState::Stale
        } else {
            CatalogState::Unavailable
        };
        let response = CatalogResponse {
            status: CatalogStatus {
                state,
                repository: repository.clone(),
                last_success_at,
                error: None,
            },
            result: result.unwrap_or_else(|| CatalogReadResult {
                catalog: Catalog {
                    format: CATALOG_FORMAT,
                    repository: repository.clone(),
                    entries: Vec::new(),
                    extra: BTreeMap::new(),
                },
                incompatible: Vec::new(),
            }),
        };
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("package-client")
            .build()
            .map_err(|_| PlatformError::new(error_code::INTERNAL_ERROR, "目录客户端初始化失败"))?;
        Ok(Self {
            db,
            repository,
            capabilities,
            client,
            cache: Mutex::new(CatalogCache {
                response,
                last_check: None,
            }),
            downloads: Mutex::new(BTreeMap::new()),
            root,
        })
    }

    pub async fn current(&self) -> CatalogResponse {
        self.cache.lock().await.response.clone()
    }

    /// 刷新锁同时合并并发请求，普通检查和手动刷新分别执行契约最小间隔。
    pub async fn refresh(&self, force: bool) -> PlatformResult<CatalogResponse> {
        let mut cache = self.cache.lock().await;
        if self.repository.is_empty() {
            return Ok(cache.response.clone());
        }
        let minimum = if force {
            CATALOG_MIN_CHECK_SECONDS
        } else {
            CATALOG_CACHE_SECONDS
        };
        if cache
            .last_check
            .is_some_and(|at| at.elapsed() < Duration::from_secs(minimum))
        {
            return Ok(cache.response.clone());
        }
        cache.last_check = Some(Instant::now());
        let result = tokio::time::timeout(Duration::from_secs(CATALOG_TIMEOUT_SECONDS), async {
            let (parsed, raw) = self.fetch_catalog().await?;
            self.persist_history(&parsed, &raw).await?;
            Ok::<_, PlatformError>(parsed)
        })
        .await;
        match result {
            Ok(Ok(parsed)) => {
                let at = now();
                cache.response = CatalogResponse {
                    status: CatalogStatus {
                        state: CatalogState::Ready,
                        repository: self.repository.clone(),
                        last_success_at: Some(at),
                        error: None,
                    },
                    result: parsed,
                };
            }
            error => {
                let message = match error {
                    Err(_) => "目录请求超时",
                    Ok(Err(_)) => "目录读取或校验失败",
                    Ok(Ok(_)) => unreachable!(),
                };
                cache.response.status.state = if cache.response.status.last_success_at.is_some() {
                    CatalogState::Stale
                } else {
                    CatalogState::Unavailable
                };
                cache.response.status.error = Some(message.into());
            }
        }
        Ok(cache.response.clone())
    }

    async fn fetch_catalog(&self) -> PlatformResult<(CatalogReadResult, String)> {
        let url = format!(
            "https://raw.githubusercontent.com/{}/main/catalog.json",
            self.repository
        );
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(CATALOG_TIMEOUT_SECONDS))
            .send()
            .await
            .map_err(|_| PlatformError::new(error_code::UPSTREAM_UNAVAILABLE, "目录连接失败"))?;
        if !response.status().is_success() {
            return Err(PlatformError::new(
                error_code::UPSTREAM_UNAVAILABLE,
                "目录响应失败",
            ));
        }
        let bytes = bounded_response(response, MAX_CATALOG_BYTES).await?;
        let parsed = parse_catalog_for_host(
            &bytes,
            &self.repository,
            CONTRACT_VERSION,
            &self.capabilities,
        )?;
        let raw = String::from_utf8(bytes)
            .map_err(|_| PlatformError::new(error_code::INVALID_CATALOG, "目录不是 UTF-8"))?;
        Ok((parsed, raw))
    }

    pub(crate) async fn persist_history(
        &self,
        parsed: &CatalogReadResult,
        raw: &str,
    ) -> PlatformResult<()> {
        let raw_catalog: RawCatalog = serde_json::from_value(parse_unique_json(raw.as_bytes())?)?;
        let mut tx = self.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT app_id,version,manifest,packages FROM app_release_history WHERE repository=?",
        )
        .bind(&self.repository)
        .fetch_all(&mut *tx)
        .await?;
        let mut known = BTreeMap::new();
        let mut maxima = BTreeMap::new();
        let mut typed = Catalog {
            format: CATALOG_FORMAT,
            repository: self.repository.clone(),
            entries: Vec::new(),
            extra: BTreeMap::new(),
        };
        for (id, version, manifest, packages) in previous {
            let manifest: serde_json::Value = serde_json::from_str(&manifest)?;
            let packages: BTreeMap<String, PackageArtifact> = serde_json::from_str(&packages)?;
            if let Ok(full) = serde_json::from_value::<Manifest>(manifest.clone())
                && tgdrive_contract::validation::check_manifest_compatibility(
                    &full,
                    CONTRACT_VERSION,
                    &self.capabilities,
                )
                .is_ok()
            {
                typed.entries.push(CatalogEntry {
                    manifest: full,
                    packages: packages.clone(),
                    extra: BTreeMap::new(),
                });
            }
            let v = stable_version(&version).ok_or_else(|| {
                PlatformError::new(error_code::INVALID_CATALOG, "发布历史版本无效")
            })?;
            let maximum = maxima.entry(id.clone()).or_insert(v.clone());
            if v > *maximum {
                *maximum = v;
            }
            known.insert((id, version), (manifest, packages));
        }
        // 共享校验负责当前可完整解析的条目；原始历史补足被跳过的未来版本。
        if typed.entries.len() <= MAX_CATALOG_ENTRIES as usize {
            validate_catalog_transition(&typed, &parsed.catalog)?;
        }
        let mut current_maxima = BTreeMap::new();
        for entry in raw_catalog.entries {
            let header: CatalogCompatibility = serde_json::from_value(entry.manifest.clone())?;
            let version = stable_version(&header.version)
                .ok_or_else(|| PlatformError::new(error_code::INVALID_CATALOG, "目录版本无效"))?;
            let maximum = current_maxima
                .entry(header.id.clone())
                .or_insert(version.clone());
            if version > *maximum {
                *maximum = version;
            }
            let manifest = if let Some(typed) = parsed
                .catalog
                .entries
                .iter()
                .find(|e| e.manifest.id == header.id && e.manifest.version == header.version)
            {
                serde_json::to_value(&typed.manifest)?
            } else {
                entry.manifest
            };
            if let Some((old, packages)) = known.get(&(header.id.clone(), header.version.clone()))
                && (old != &manifest
                    || packages
                        .iter()
                        .any(|(target, artifact)| entry.packages.get(target) != Some(artifact)))
            {
                return Err(PlatformError::new(
                    error_code::PACKAGE_CHANGED,
                    "目录修改了已经发布的清单或包绑定",
                ));
            }
            sqlx::query("INSERT INTO app_release_history(repository,app_id,version,manifest,packages) VALUES(?,?,?,?,?) ON CONFLICT(repository,app_id,version) DO UPDATE SET packages=excluded.packages")
                .bind(&self.repository).bind(&header.id).bind(&header.version).bind(serde_json::to_string(&manifest)?).bind(serde_json::to_string(&entry.packages)?).execute(&mut *tx).await?;
        }
        if current_maxima
            .iter()
            .any(|(id, v)| maxima.get(id).is_some_and(|old| v < old))
        {
            return Err(PlatformError::new(
                error_code::INVALID_CATALOG,
                "目录最高发布版本不能倒退",
            ));
        }
        sqlx::query("INSERT INTO app_catalog_snapshots(repository,raw_catalog,last_success_at) VALUES(?,?,?) ON CONFLICT(repository) DO UPDATE SET raw_catalog=excluded.raw_catalog,last_success_at=excluded.last_success_at")
            .bind(&self.repository).bind(raw).bind(now().0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn download(&self, artifact: &PackageArtifact) -> PlatformResult<PathBuf> {
        let guard = {
            let mut locks = self.downloads.lock().await;
            locks.retain(|_, v| Arc::strong_count(v) > 1);
            locks.entry(artifact.sha256.clone()).or_default().clone()
        };
        let _guard = guard.lock().await;
        let destination = self.root.join(format!("{}.tgapp", artifact.sha256));
        if destination.is_file() && check_artifact(&destination, artifact).await.is_ok() {
            return Ok(destination);
        }
        let result = tokio::time::timeout(
            Duration::from_secs(PACKAGE_DOWNLOAD_SECONDS),
            self.fetch_package(artifact, &destination),
        )
        .await
        .map_err(|_| PlatformError::new(error_code::REQUEST_TIMEOUT, "包下载超时"))?;
        result?;
        Ok(destination)
    }

    async fn fetch_package(
        &self,
        artifact: &PackageArtifact,
        destination: &Path,
    ) -> PlatformResult<()> {
        let mut url = Url::parse(&artifact.url)
            .map_err(|_| PlatformError::new(error_code::INVALID_CATALOG, "包地址格式无效"))?;
        validate_redirect(&url, false)?;
        for hops in 0..=MAX_DOWNLOAD_REDIRECTS {
            let response = self.client.get(url.clone()).send().await.map_err(|_| {
                PlatformError::new(error_code::UPSTREAM_UNAVAILABLE, "包下载连接失败")
            })?;
            if response.status().is_redirection() {
                if hops == MAX_DOWNLOAD_REDIRECTS {
                    return Err(PlatformError::new(
                        error_code::INVALID_PACKAGE,
                        "包重定向次数超过上限",
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        PlatformError::new(error_code::INVALID_PACKAGE, "包重定向缺少地址")
                    })?;
                url = url.join(location).map_err(|_| {
                    PlatformError::new(error_code::INVALID_PACKAGE, "包重定向地址无效")
                })?;
                validate_redirect(&url, true)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(PlatformError::new(
                    error_code::UPSTREAM_UNAVAILABLE,
                    "包下载响应失败",
                ));
            }
            if artifact.size.0 > MAX_PACKAGE_BYTES
                || response
                    .content_length()
                    .is_some_and(|n| n != artifact.size.0)
            {
                return Err(PlatformError::new(
                    error_code::PACKAGE_CHANGED,
                    "包下载长度与目录绑定不符",
                ));
            }
            let staging = tempfile::Builder::new()
                .prefix("download-")
                .tempdir_in(&self.root)?;
            let path = staging.path().join("package.tgapp");
            let mut file = tokio::fs::File::create(&path).await?;
            let mut stream = response.bytes_stream();
            let mut size = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    PlatformError::new(error_code::UPSTREAM_UNAVAILABLE, "包下载中断")
                })?;
                size = size.saturating_add(chunk.len() as u64);
                if size > artifact.size.0 || size > MAX_PACKAGE_BYTES {
                    return Err(PlatformError::new(
                        error_code::PAYLOAD_TOO_LARGE,
                        "包下载实际长度超限",
                    ));
                }
                file.write_all(&chunk).await?;
            }
            file.sync_all().await?;
            drop(file);
            check_artifact(&path, artifact).await?;
            tokio::fs::rename(path, destination).await?;
            return Ok(());
        }
        unreachable!()
    }
}

async fn bounded_response(response: reqwest::Response, maximum: u64) -> PlatformResult<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > maximum) {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "远端正文超过上限",
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|_| PlatformError::new(error_code::UPSTREAM_UNAVAILABLE, "远端读取中断"))?;
        if bytes.len() as u64 + chunk.len() as u64 > maximum {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "远端正文超过上限",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) async fn check_artifact(path: &Path, artifact: &PackageArtifact) -> PlatformResult<()> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    if file.metadata().await?.len() != artifact.size.0 {
        return Err(PlatformError::new(
            error_code::PACKAGE_CHANGED,
            "包长度与目录绑定不符",
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    if crate::engine::upload::to_hex(&digest.finalize()) != artifact.sha256 {
        return Err(PlatformError::new(
            error_code::PACKAGE_CHANGED,
            "包摘要与目录绑定不符",
        ));
    }
    Ok(())
}

fn validate_redirect(url: &Url, temporary: bool) -> PlatformResult<()> {
    let allowed_host = matches!(
        url.host_str(),
        Some(
            "github.com" | "release-assets.githubusercontent.com" | "objects.githubusercontent.com"
        )
    );
    let allowed_parameter = |key: &str| {
        matches!(
            key,
            "sv" | "sr"
                | "spr"
                | "sp"
                | "se"
                | "st"
                | "sig"
                | "skt"
                | "ske"
                | "sks"
                | "skv"
                | "skoid"
                | "sktid"
                | "rsct"
                | "rscd"
                | "rscc"
                | "response-content-disposition"
                | "response-content-type"
                | "X-Amz-Algorithm"
                | "X-Amz-Credential"
                | "X-Amz-Date"
                | "X-Amz-Expires"
                | "X-Amz-SignedHeaders"
                | "X-Amz-Signature"
                | "X-Amz-Security-Token"
                | "jwt"
        )
    };
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !allowed_host
        || (!temporary && url.query().is_some())
        || url.query_pairs().any(|(key, _)| !allowed_parameter(&key))
    {
        return Err(PlatformError::new(
            error_code::INVALID_PACKAGE,
            "包重定向越过允许的 GitHub Release 下载边界",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirects_never_become_arbitrary_network_access() {
        for url in [
            "http://github.com/x",
            "https://github.com:444/x",
            "https://user@github.com/x",
            "https://example.invalid/x",
            "https://github.com/x#secret",
            "https://objects.githubusercontent.com/x?next=https://other.invalid",
        ] {
            assert!(validate_redirect(&Url::parse(url).unwrap(), true).is_err());
        }
        assert!(
            validate_redirect(
                &Url::parse("https://release-assets.githubusercontent.com/x?sv=1&sig=test")
                    .unwrap(),
                true
            )
            .is_ok()
        );
    }
}
