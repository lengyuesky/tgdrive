//! 应用中心目录：安全读取 GitHub 仓库根目录索引，校验、缓存并按精确摘要安装插件包。

use crate::apps::package::{API_VERSION, AppPackage, MAX_PACKAGE_SIZE, Manifest, valid_id};
use crate::error::{AppError, AppResult};
use crate::proxy::ProxyConfig;
use futures_util::StreamExt;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

pub const MAX_CATALOG_SIZE: usize = 1024 * 1024; // 1 MiB
pub const MAX_ENTRIES: usize = 512;
pub const MAX_REDIRECT_HOPS: usize = 5;
pub const CATALOG_CACHE_TTL: Duration = Duration::from_secs(300); // 5 分钟
pub const MIN_CHECK_INTERVAL: Duration = Duration::from_secs(30); // 至少 30 秒检查间隔
pub const MAX_PACKAGE_CACHE_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB LRU 缓存

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSourceType {
    Github,
    Local,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogSourceConfig {
    Github { repo: String },
    Local { path: PathBuf },
    Disabled,
}

impl CatalogSourceConfig {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var_os("TGDRIVE_APP_CATALOG_DIR").map(PathBuf::from),
            std::env::var("TGDRIVE_APP_CATALOG_REMOTE_ENABLED")
                .ok()
                .as_deref(),
            std::env::var("TGDRIVE_APP_GITHUB_REPO").ok().as_deref(),
            || {
                ["app-catalog", "apps/catalog"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|p| p.is_dir())
            },
        )
    }

    fn from_values(
        directory: Option<PathBuf>,
        remote_enabled: Option<&str>,
        repository: Option<&str>,
        local_fallback: impl FnOnce() -> Option<PathBuf>,
    ) -> Self {
        if let Some(path) = directory.filter(|path| {
            !path.as_os_str().is_empty() && !path.to_str().is_some_and(|s| s.trim().is_empty())
        }) {
            return Self::Local { path };
        }
        let remote_enabled = !remote_enabled.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        });
        if !remote_enabled {
            return local_fallback().map_or(Self::Disabled, |path| Self::Local { path });
        }
        let repo = repository.unwrap_or("lengyuesky/tgdrive-app");
        if is_valid_repo(repo) {
            Self::Github { repo: repo.into() }
        } else {
            tracing::warn!("TGDRIVE_APP_GITHUB_REPO 格式无效，应用中心目录已禁用");
            Self::Disabled
        }
    }
}

pub fn is_valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    repo.len() <= 100
        && [owner, name].iter().all(|part| {
            !part.is_empty()
                && !matches!(*part, "." | "..")
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPayload {
    pub schema_version: u32,
    pub repository: String,
    pub entries: Vec<CatalogEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub manifest: Manifest,
    pub sha256: String,
    pub size: usize,
    pub url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalogPayload {
    schema_version: u32,
    repository: String,
    entries: Vec<RawCatalogEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalogEntry {
    manifest: serde_json::Value,
    sha256: String,
    size: usize,
    url: String,
}

#[derive(Deserialize)]
struct CatalogCompatibility {
    id: String,
    version: String,
    api_version: u32,
    min_host_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogStatus {
    pub source: CatalogSourceType,
    pub repository: Option<String>,
    pub state: String, // "ready" | "stale" | "unavailable" | "disabled"
    pub last_success_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogApp {
    pub manifest: Manifest,
    pub digest: String,
    pub size: usize,
    pub source: CatalogSourceType,
    pub source_url: Option<String>,
    pub update_available: bool,
}

fn stable_version(value: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(value).ok()?;
    (value.len() <= 80 && version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn valid_package_asset(asset: &str) -> bool {
    asset
        .strip_suffix(".tgapp")
        .and_then(|name| name.rsplit_once('-'))
        .is_some_and(|(id, version)| valid_id(id) && stable_version(version).is_some())
}

pub fn validate_redirect_url(url: &Url, expected_repo: &str) -> AppResult<()> {
    if !is_valid_repo(expected_repo) || url.scheme() != "https" {
        return Err(AppError::BadRequest(
            "目录与安装包仅支持合法 GitHub 仓库的 HTTPS 链接".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::BadRequest("链接禁止包含用户信息或凭据".into()));
    }
    if url.port().is_some_and(|port| port != 443) || url.fragment().is_some() {
        return Err(AppError::BadRequest(
            "链接仅支持 443 端口且不能含片段".into(),
        ));
    }
    if url.query().is_some() {
        return Err(AppError::BadRequest("链接禁止包含查询参数".into()));
    }
    if url.host_str() != Some("raw.githubusercontent.com") {
        return Err(AppError::BadRequest("拒绝访问不受信任的下载主机".into()));
    }
    let prefix = format!("/{expected_repo}/main/");
    let Some(relative_path) = url.path().strip_prefix(&prefix) else {
        return Err(AppError::BadRequest(
            "GitHub 下载路径必须属于指定仓库的主分支".into(),
        ));
    };
    let valid = if relative_path == "catalog.json" {
        true
    } else if let Some(asset) = relative_path.strip_prefix("apps/") {
        valid_package_asset(asset)
    } else {
        false
    };
    if !valid {
        return Err(AppError::BadRequest("GitHub 下载路径格式无效".into()));
    }
    Ok(())
}

fn validate_initial_url(url: &Url, expected_repo: &str) -> AppResult<()> {
    validate_redirect_url(url, expected_repo)
}

#[async_trait::async_trait]
pub trait CatalogTransport: Send + Sync {
    async fn fetch(
        &self,
        url: &str,
        expected_repo: &str,
        proxy: &ProxyConfig,
        timeout: Duration,
        max_bytes: usize,
    ) -> AppResult<Vec<u8>>;
}

pub struct HttpCatalogTransport;

#[async_trait::async_trait]
impl CatalogTransport for HttpCatalogTransport {
    async fn fetch(
        &self,
        initial_url: &str,
        expected_repo: &str,
        proxy: &ProxyConfig,
        timeout: Duration,
        max_bytes: usize,
    ) -> AppResult<Vec<u8>> {
        let client = proxy
            .http_builder()?
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .build()
            .map_err(|_| AppError::BadRequest("构建目录 HTTP 客户端失败".into()))?;
        fetch_http(initial_url, expected_repo, timeout, max_bytes, |url| {
            client.get(url).send()
        })
        .await
    }
}

// 注入的仅是单跳 HTTP 请求；生产与离线回归共用重定向、流限量和总超时校验。
async fn fetch_http<F, Fut>(
    initial_url: &str,
    expected_repo: &str,
    timeout: Duration,
    max_bytes: usize,
    mut send: F,
) -> AppResult<Vec<u8>>
where
    F: FnMut(Url) -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut current_url =
        Url::parse(initial_url).map_err(|_| AppError::BadRequest("初始下载地址格式无效".into()))?;
    validate_initial_url(&current_url, expected_repo)?;
    tokio::time::timeout(timeout, async {
        for hop in 0..=MAX_REDIRECT_HOPS {
            let response = send(current_url.clone()).await.map_err(|error| {
                // 不输出 reqwest 错误链：其中可能含代理凭据或 CDN 签名。
                AppError::BadRequest(
                    if error.is_timeout() {
                        "目录或安装包请求超时"
                    } else {
                        "目录或安装包网络请求失败"
                    }
                    .into(),
                )
            })?;
            let status = response.status();
            if status.is_redirection() {
                if hop == MAX_REDIRECT_HOPS {
                    return Err(AppError::BadRequest("重定向次数超过 5 次上限".into()));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| AppError::BadRequest("重定向缺少有效 Location 标头".into()))?;
                let next_url = current_url
                    .join(location)
                    .map_err(|_| AppError::BadRequest("重定向地址格式无效".into()))?;
                validate_redirect_url(&next_url, expected_repo)?;
                current_url = next_url;
                continue;
            }
            if !status.is_success() {
                return Err(AppError::BadRequest(format!(
                    "GitHub 下载返回错误状态码：{}",
                    status.as_u16()
                )));
            }
            if response
                .content_length()
                .is_some_and(|length| length > max_bytes as u64)
            {
                return Err(AppError::BadRequest("下载响应超过大小上限".into()));
            }
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| AppError::BadRequest("读取下载响应失败".into()))?;
                if chunk.len() > max_bytes.saturating_sub(body.len()) {
                    return Err(AppError::BadRequest("下载响应超过大小上限".into()));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(body);
        }
        unreachable!("重定向上限在发起下一跳之前检查")
    })
    .await
    .map_err(|_| AppError::BadRequest("目录或安装包请求总超时".into()))?
}

pub struct CatalogSnapshot {
    pub status: CatalogStatus,
    pub entries: Vec<CatalogEntry>,
    pub last_network_attempt: Option<Instant>,
}

pub struct CatalogManager {
    cache_root: PathBuf,
    config: CatalogSourceConfig,
    transport: Arc<dyn CatalogTransport>,
    proxy_provider: Arc<dyn Fn() -> ProxyConfig + Send + Sync>,
    snapshot: RwLock<CatalogSnapshot>,
    refresh_lock: Mutex<()>,
    package_cache_lock: Arc<Mutex<()>>,
}

fn sanitize_repo(repo: &str) -> String {
    // 仓库名不允许波浪号，先转义下划线，避免名称与目录分隔标记碰撞。
    repo.replace('_', "~").replace('/', "__")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn catalog_is_fresh(last_success_at: Option<i64>) -> bool {
    last_success_at
        .and_then(|timestamp| now_millis().checked_sub(timestamp))
        .is_some_and(|age| (0..CATALOG_CACHE_TTL.as_millis() as i64).contains(&age))
}

fn read_bounded_file(path: &Path, max_bytes: usize) -> AppResult<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(AppError::BadRequest(
            "缓存或本地包不是普通文件，或超过大小上限".into(),
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(AppError::BadRequest(
            "文件类型或大小在读取前发生变化".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(AppError::BadRequest("文件超过大小上限".into()));
    }
    Ok(bytes)
}

fn remove_temporary_files(directory: &Path, prefix: &str) -> AppResult<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
            && !entry.file_type()?.is_dir()
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn persist_file(directory: &Path, name: &str, prefix: &str, bytes: &[u8]) -> AppResult<()> {
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join(name))
        .map_err(|error| AppError::Io(error.error))?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

impl CatalogManager {
    pub fn new(
        cache_root: impl Into<PathBuf>,
        config: CatalogSourceConfig,
        transport: Arc<dyn CatalogTransport>,
        proxy_provider: Arc<dyn Fn() -> ProxyConfig + Send + Sync>,
    ) -> Self {
        let cache_root = cache_root.into();
        let config = match config {
            CatalogSourceConfig::Github { ref repo } if !is_valid_repo(repo) => {
                CatalogSourceConfig::Disabled
            }
            other => other,
        };
        let (initial_status, initial_entries) = Self::init_snapshot(&cache_root, &config);
        Self {
            cache_root,
            config,
            transport,
            proxy_provider,
            snapshot: RwLock::new(CatalogSnapshot {
                status: initial_status,
                entries: initial_entries,
                last_network_attempt: None,
            }),
            refresh_lock: Mutex::new(()),
            package_cache_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn from_env(cache_root: impl Into<PathBuf>, rt: Arc<crate::settings::Runtime>) -> Self {
        let config = CatalogSourceConfig::from_env();
        let transport = Arc::new(HttpCatalogTransport);
        let proxy_provider = Arc::new(move || rt.current().proxy.clone());
        Self::new(cache_root, config, transport, proxy_provider)
    }

    #[allow(dead_code)]
    pub fn disabled() -> Self {
        Self::new(
            PathBuf::from("/tmp"),
            CatalogSourceConfig::Disabled,
            Arc::new(HttpCatalogTransport),
            Arc::new(ProxyConfig::default),
        )
    }

    fn init_snapshot(
        cache_root: &Path,
        config: &CatalogSourceConfig,
    ) -> (CatalogStatus, Vec<CatalogEntry>) {
        match config {
            CatalogSourceConfig::Disabled => (
                CatalogStatus {
                    source: CatalogSourceType::Disabled,
                    repository: None,
                    state: "disabled".into(),
                    last_success_at: None,
                    error: None,
                },
                Vec::new(),
            ),
            CatalogSourceConfig::Local { .. } => (
                CatalogStatus {
                    source: CatalogSourceType::Local,
                    repository: None,
                    state: "ready".into(),
                    last_success_at: Some(now_millis()),
                    error: None,
                },
                Vec::new(),
            ),
            CatalogSourceConfig::Github { repo } => {
                let repo_dir = cache_root.join(sanitize_repo(repo));
                if let Err(error) = remove_temporary_files(&repo_dir, ".catalog.tmp.")
                    .and_then(|_| Self::prune_package_cache(&repo_dir.join("packages"), 0))
                {
                    tracing::warn!(%error, "清理应用目录缓存失败");
                }
                let catalog_path = repo_dir.join("catalog.json");
                if catalog_path.exists() {
                    match read_bounded_file(&catalog_path, MAX_CATALOG_SIZE) {
                        Ok(bytes) => match Self::parse_catalog_bytes(&bytes, repo) {
                            Ok(payload) => {
                                let mtime = std::fs::metadata(&catalog_path)
                                    .and_then(|m| m.modified())
                                    .ok()
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                return (
                                    CatalogStatus {
                                        source: CatalogSourceType::Github,
                                        repository: Some(repo.clone()),
                                        state: if catalog_is_fresh(Some(mtime)) {
                                            "ready"
                                        } else {
                                            "stale"
                                        }
                                        .into(),
                                        last_success_at: Some(mtime),
                                        error: None,
                                    },
                                    payload.entries,
                                );
                            }
                            Err(err) => {
                                tracing::warn!(%err, "持久缓存目录损坏或为旧协议版本，将在联网时重新获取");
                                let _ = std::fs::remove_file(&catalog_path);
                            }
                        },
                        Err(err) => {
                            tracing::warn!(%err, "无法读取持久目录缓存");
                            let _ = std::fs::remove_file(&catalog_path);
                        }
                    }
                }
                (
                    CatalogStatus {
                        source: CatalogSourceType::Github,
                        repository: Some(repo.clone()),
                        state: "unavailable".into(),
                        last_success_at: None,
                        error: None,
                    },
                    Vec::new(),
                )
            }
        }
    }

    pub fn parse_catalog_bytes(bytes: &[u8], expected_repo: &str) -> AppResult<CatalogPayload> {
        if bytes.is_empty() || bytes.len() > MAX_CATALOG_SIZE {
            return Err(AppError::BadRequest("目录文件为空或超过 1 MiB".into()));
        }
        // Serde 的结构体也接受位置数组；先约束协议的对象形态，再严格解析以保留重复字段检查。
        let object_shapes = {
            let value: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|_| AppError::BadRequest("目录 JSON 或字段格式无效".into()))?;
            value.is_object()
                && value
                    .get("entries")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|entries| {
                        entries.iter().all(|entry| {
                            entry.is_object()
                                && entry
                                    .get("manifest")
                                    .is_some_and(serde_json::Value::is_object)
                        })
                    })
        };
        if !object_shapes {
            return Err(AppError::BadRequest(
                "目录、条目和清单必须使用 JSON 对象".into(),
            ));
        }
        let payload: RawCatalogPayload = serde_json::from_slice(bytes)
            .map_err(|_| AppError::BadRequest("目录 JSON 或字段格式无效".into()))?;
        if payload.schema_version != 2
            || !is_valid_repo(expected_repo)
            || payload.repository != expected_repo
        {
            return Err(AppError::BadRequest("目录协议版本或仓库名不符".into()));
        }
        if payload.entries.len() > MAX_ENTRIES {
            return Err(AppError::BadRequest("目录条目数量超过 512 上限".into()));
        }
        let host = semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("主程序版本合法");
        let mut seen = std::collections::HashSet::new();
        let mut validated_entries = Vec::with_capacity(payload.entries.len());
        for entry in payload.entries {
            let header: CatalogCompatibility = serde_json::from_value(entry.manifest.clone())
                .map_err(|_| AppError::BadRequest("目录清单的身份或兼容性字段格式无效".into()))?;
            if !valid_id(&header.id)
                || stable_version(&header.version).is_none()
                || header.api_version == 0
                || header.min_host_version.len() > 80
            {
                return Err(AppError::BadRequest(
                    "目录清单 ID、稳定版本或接口版本无效".into(),
                ));
            }
            if !seen.insert(header.id.clone()) {
                return Err(AppError::BadRequest("目录包含重复的应用记录".into()));
            }
            let minimum = semver::Version::parse(&header.min_host_version)
                .map_err(|_| AppError::BadRequest("目录清单的最低宿主版本无效".into()))?;
            if !valid_digest(&entry.sha256) || entry.size == 0 || entry.size > MAX_PACKAGE_SIZE {
                return Err(AppError::BadRequest("目录安装包摘要或大小无效".into()));
            }
            let expected_url = format!(
                "https://raw.githubusercontent.com/{expected_repo}/main/apps/{}-{}.tgapp",
                header.id, header.version
            );
            if entry.url != expected_url {
                return Err(AppError::BadRequest(
                    "条目下载地址与仓库、分支或包名不符".into(),
                ));
            }
            // 仅明确声明未来接口或更高宿主版本的条目可以跳过；它们绝不参与下载或安装。
            // 身份、摘要、大小元数据仍须合法，兼容条目继续采用严格完整清单。
            if header.api_version > API_VERSION || minimum > host {
                continue;
            }
            if !entry
                .manifest
                .get("permissions")
                .is_some_and(serde_json::Value::is_array)
            {
                return Err(AppError::BadRequest("目录清单必须完整声明权限列表".into()));
            }
            if entry
                .manifest
                .get("settings")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|settings| settings.iter().any(|field| !field.is_object()))
            {
                return Err(AppError::BadRequest(
                    "目录清单必须完整声明设置列表，且各项为 JSON 对象".into(),
                ));
            }
            let manifest: Manifest = serde_json::from_value(entry.manifest)
                .map_err(|_| AppError::BadRequest("目录应用清单含未知字段或字段格式无效".into()))?;
            manifest
                .validate()
                .map_err(|_| AppError::BadRequest("目录应用清单、权限或设置无效".into()))?;
            validated_entries.push(CatalogEntry {
                manifest,
                sha256: entry.sha256,
                size: entry.size,
                url: entry.url,
            });
        }
        Ok(CatalogPayload {
            schema_version: payload.schema_version,
            repository: payload.repository,
            entries: validated_entries,
        })
    }

    async fn validate_history(&self, payload: CatalogPayload) -> AppResult<CatalogPayload> {
        let snapshot = self.snapshot.read().await;
        let old_by_id: HashMap<&str, &CatalogEntry> = snapshot
            .entries
            .iter()
            .map(|e| (e.manifest.id.as_str(), e))
            .collect();

        for new_entry in &payload.entries {
            if let Some(old_entry) = old_by_id.get(new_entry.manifest.id.as_str()) {
                let old_ver = semver::Version::parse(&old_entry.manifest.version)
                    .map_err(|_| AppError::BadRequest("快照中的插件版本格式无效".into()))?;
                let new_ver = semver::Version::parse(&new_entry.manifest.version)
                    .map_err(|_| AppError::BadRequest("新目录中的插件版本格式无效".into()))?;

                if new_ver < old_ver {
                    return Err(AppError::BadRequest(format!(
                        "远程目录应用 {} 版本倒退（{} -> {}）",
                        new_entry.manifest.id, old_ver, new_ver
                    )));
                }
                if new_ver == old_ver
                    && (new_entry.sha256 != old_entry.sha256
                        || new_entry.size != old_entry.size
                        || new_entry.manifest != old_entry.manifest)
                {
                    return Err(AppError::BadRequest(format!(
                        "远程目录更改了已知应用 {} 版本 {} 的摘要或元数据",
                        new_entry.manifest.id, old_ver
                    )));
                }
            }
        }
        Ok(payload)
    }

    #[allow(dead_code)]
    pub async fn status(&self) -> CatalogStatus {
        self.snapshot.read().await.status.clone()
    }

    pub async fn refresh(&self, force: bool) {
        match &self.config {
            CatalogSourceConfig::Disabled => {}
            CatalogSourceConfig::Local { .. } => {
                let mut snap = self.snapshot.write().await;
                snap.status.state = "ready".into();
                snap.status.last_success_at = Some(now_millis());
                snap.status.error = None;
            }
            CatalogSourceConfig::Github { repo } => {
                if !force {
                    let snap = self.snapshot.read().await;
                    if snap.status.state == "ready" && catalog_is_fresh(snap.status.last_success_at)
                    {
                        return;
                    }
                }

                let _guard = self.refresh_lock.lock().await;

                // 获取锁后双重检查
                {
                    let snap = self.snapshot.read().await;
                    if !force
                        && snap.status.state == "ready"
                        && catalog_is_fresh(snap.status.last_success_at)
                    {
                        return;
                    }
                    if let Some(last_attempt) = snap.last_network_attempt
                        && last_attempt.elapsed() < MIN_CHECK_INTERVAL
                    {
                        // 间隔小于 30 秒，遵循防刷约束
                        return;
                    }
                }

                {
                    let mut snap = self.snapshot.write().await;
                    snap.last_network_attempt = Some(Instant::now());
                }

                let catalog_url =
                    format!("https://raw.githubusercontent.com/{repo}/main/catalog.json");
                let proxy = (self.proxy_provider)();
                let fetch_result = self
                    .transport
                    .fetch(
                        &catalog_url,
                        repo,
                        &proxy,
                        Duration::from_secs(10),
                        MAX_CATALOG_SIZE,
                    )
                    .await;

                let result = async {
                    let bytes = fetch_result?;
                    let payload = Self::parse_catalog_bytes(&bytes, repo)?;
                    Ok::<_, AppError>((bytes, self.validate_history(payload).await?))
                }
                .await;
                match result {
                    Ok((bytes, payload)) => {
                        let repo_dir = self.cache_root.join(sanitize_repo(repo));
                        if let Err(error) = Self::persist_catalog(&repo_dir, &bytes).await {
                            tracing::warn!(%error, "保存应用目录持久缓存失败");
                        }
                        let mut snap = self.snapshot.write().await;
                        snap.status.state = "ready".into();
                        snap.status.last_success_at = Some(now_millis());
                        snap.status.error = None;
                        snap.entries = payload.entries;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "更新远程应用目录失败");
                        let mut snap = self.snapshot.write().await;
                        snap.status.state = if snap.status.last_success_at.is_some() {
                            "stale"
                        } else {
                            "unavailable"
                        }
                        .into();
                        snap.status.error = Some(error.to_string());
                    }
                }
            }
        }
    }

    async fn persist_catalog(repo_dir: &Path, bytes: &[u8]) -> AppResult<()> {
        let repo_dir = repo_dir.to_path_buf();
        let bytes = bytes.to_vec();
        super::blocking(move || persist_file(&repo_dir, "catalog.json", ".catalog.tmp.", &bytes))
            .await
    }

    pub async fn available_apps(
        &self,
        installed: &[crate::db::apps::AppRecord],
    ) -> (CatalogStatus, Vec<CatalogApp>) {
        match &self.config {
            CatalogSourceConfig::Disabled => (
                CatalogStatus {
                    source: CatalogSourceType::Disabled,
                    repository: None,
                    state: "disabled".into(),
                    last_success_at: None,
                    error: None,
                },
                Vec::new(),
            ),
            CatalogSourceConfig::Local { path } => {
                let path = path.clone();
                let installed: HashMap<_, _> = installed
                    .iter()
                    .map(|app| (app.id.clone(), app.version.clone()))
                    .collect();
                let result = super::blocking(move || {
                    // 遍历、读取和 ZIP 解压都在阻塞线程完成，不把惰性迭代器带回 Tokio。
                    let mut best_by_id: BTreeMap<String, CatalogApp> = BTreeMap::new();
                    for pkg in Self::read_local_packages(&path) {
                        if best_by_id.get(&pkg.manifest.id).is_some_and(|old| {
                            semver::Version::parse(&old.manifest.version).unwrap()
                                >= semver::Version::parse(&pkg.manifest.version).unwrap()
                        }) {
                            continue;
                        }
                        let id = pkg.manifest.id.clone();
                        let update_available = installed.get(&id).is_some_and(|version| {
                            semver::Version::parse(version).ok()
                                < semver::Version::parse(&pkg.manifest.version).ok()
                        });
                        best_by_id.insert(
                            id,
                            CatalogApp {
                                manifest: pkg.manifest,
                                digest: pkg.digest,
                                size: pkg.archive.len(),
                                source: CatalogSourceType::Local,
                                source_url: None,
                                update_available,
                            },
                        );
                    }
                    Ok(best_by_id.into_values().collect())
                })
                .await;
                match result {
                    Ok(available) => (
                        CatalogStatus {
                            source: CatalogSourceType::Local,
                            repository: None,
                            state: "ready".into(),
                            last_success_at: Some(now_millis()),
                            error: None,
                        },
                        available,
                    ),
                    Err(_) => (
                        CatalogStatus {
                            source: CatalogSourceType::Local,
                            repository: None,
                            state: "unavailable".into(),
                            last_success_at: None,
                            error: Some("读取本地应用目录失败".into()),
                        },
                        Vec::new(),
                    ),
                }
            }
            CatalogSourceConfig::Github { repo } => {
                let snap = self.snapshot.read().await;
                let mut best_by_id: BTreeMap<String, CatalogEntry> = BTreeMap::new();
                for entry in &snap.entries {
                    if !entry.manifest.is_compatible() {
                        continue;
                    }
                    best_by_id.insert(entry.manifest.id.clone(), entry.clone());
                }
                let available = best_by_id
                    .into_values()
                    .map(|entry| {
                        let update_available = installed
                            .iter()
                            .find(|app| app.id == entry.manifest.id)
                            .is_some_and(|app| {
                                semver::Version::parse(&app.version).ok()
                                    < semver::Version::parse(&entry.manifest.version).ok()
                            });
                        CatalogApp {
                            manifest: entry.manifest,
                            digest: entry.sha256,
                            size: entry.size,
                            source: CatalogSourceType::Github,
                            source_url: Some(format!("https://github.com/{repo}/tree/main/apps")),
                            update_available,
                        }
                    })
                    .collect();
                (snap.status.clone(), available)
            }
        }
    }

    // 逐包限量读取，保留所有历史供精确摘要查找；列表只保留每个 ID 的最高版本元数据。
    fn read_local_packages(directory: &Path) -> impl Iterator<Item = AppPackage> {
        std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let path = item.ok()?.path();
                if !matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("tgapp" | "zip")
                ) {
                    return None;
                }
                let bytes = read_bounded_file(&path, MAX_PACKAGE_SIZE).ok()?;
                match AppPackage::parse(bytes) {
                    Ok(package) => Some(package),
                    Err(error) => {
                        tracing::warn!(%error, "跳过无效的本地应用包");
                        None
                    }
                }
            })
    }

    /// 根据 ID 与可选 digest 获取插件包二进制与清单，不持有安装锁。
    pub async fn get_package(
        &self,
        id: &str,
        digest: Option<&str>,
    ) -> AppResult<(Manifest, String, Vec<u8>)> {
        match &self.config {
            CatalogSourceConfig::Disabled => Err(AppError::NotFound),
            CatalogSourceConfig::Local { path } => {
                let path = path.clone();
                let id = id.to_owned();
                let digest = digest.map(str::to_owned);
                super::blocking(move || {
                    let mut packages = Self::read_local_packages(&path);
                    let pkg = if let Some(req_digest) = digest {
                        packages
                            .find(|pkg| pkg.manifest.id == id && pkg.digest == req_digest)
                            .ok_or_else(|| {
                                AppError::Conflict("插件包已经变化，请重新查看安装信息".into())
                            })?
                    } else {
                        packages
                            .filter(|pkg| pkg.manifest.id == id)
                            .max_by_key(|pkg| {
                                semver::Version::parse(&pkg.manifest.version).unwrap()
                            })
                            .ok_or(AppError::NotFound)?
                    };
                    Ok((pkg.manifest, pkg.digest, pkg.archive))
                })
                .await
            }
            CatalogSourceConfig::Github { repo } => {
                let entry = {
                    let snap = self.snapshot.read().await;
                    if let Some(req_digest) = digest {
                        snap.entries
                            .iter()
                            .find(|e| e.manifest.id == id && e.sha256 == req_digest)
                            .cloned()
                            .ok_or_else(|| {
                                AppError::Conflict("插件包已经变化，请重新查看安装信息".into())
                            })?
                    } else {
                        // 寻找兼容的最新版本
                        snap.entries
                            .iter()
                            .find(|e| e.manifest.id == id && e.manifest.is_compatible())
                            .cloned()
                            .ok_or(AppError::NotFound)?
                    }
                };

                let packages_dir = self.cache_root.join(sanitize_repo(repo)).join("packages");
                let package_path = packages_dir.join(format!("{}.tgapp", entry.sha256));
                {
                    let guard = self.package_cache_lock.clone().lock_owned().await;
                    let cached_path = package_path.clone();
                    let expected = entry.clone();
                    let cached = super::blocking(move || {
                        let _guard = guard;
                        if let Ok(bytes) = read_bounded_file(&cached_path, MAX_PACKAGE_SIZE) {
                            let digest = crate::engine::upload::to_hex(&Sha256::digest(&bytes));
                            if digest == expected.sha256 && bytes.len() == expected.size {
                                let package = Self::verify_package(bytes, &expected)?;
                                let _ = std::fs::File::open(&cached_path)
                                    .and_then(|file| file.set_modified(SystemTime::now()));
                                return Ok(Some(package));
                            }
                        }
                        // 损坏、超大或特殊缓存文件均不可信，可重新下载后验证。
                        if let Err(error) = std::fs::remove_file(&cached_path)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(error.into());
                        }
                        Ok(None)
                    })
                    .await?;
                    if let Some(package) = cached {
                        return Ok((package.manifest, package.digest, package.archive));
                    }
                }

                // 网络等待不持有快照锁、安装锁或缓存锁。
                let proxy = (self.proxy_provider)();
                let bytes = self
                    .transport
                    .fetch(
                        &entry.url,
                        repo,
                        &proxy,
                        Duration::from_secs(120),
                        MAX_PACKAGE_SIZE,
                    )
                    .await?;
                let package = super::blocking(move || Self::verify_package(bytes, &entry)).await?;
                let guard = self.package_cache_lock.clone().lock_owned().await;
                super::blocking(move || {
                    // 调用方取消也让阻塞写线程持锁直至原子替换完成。
                    let _guard = guard;
                    std::fs::create_dir_all(&packages_dir)?;
                    // 为临时文件预留空间，串行写入与淘汰使崩溃残留也无法累计超量。
                    Self::prune_package_cache(&packages_dir, package.archive.len() as u64)?;
                    persist_file(
                        &packages_dir,
                        package_path.file_name().unwrap().to_str().unwrap(),
                        ".pkg.tmp.",
                        &package.archive,
                    )?;
                    Ok((package.manifest, package.digest, package.archive))
                })
                .await
            }
        }
    }

    fn verify_package(bytes: Vec<u8>, entry: &CatalogEntry) -> AppResult<AppPackage> {
        if bytes.len() != entry.size || bytes.is_empty() || bytes.len() > MAX_PACKAGE_SIZE {
            return Err(AppError::BadRequest(
                "插件包实际大小与声明不一致或超过上限".into(),
            ));
        }
        if crate::engine::upload::to_hex(&Sha256::digest(&bytes)) != entry.sha256 {
            return Err(AppError::BadRequest("插件包摘要校验失败".into()));
        }
        let package = AppPackage::parse(bytes)
            .map_err(|_| AppError::BadRequest("插件包格式或清单校验失败".into()))?;
        if package.manifest != entry.manifest {
            return Err(AppError::BadRequest(
                "插件包内容与目录声明的清单不一致".into(),
            ));
        }
        Ok(package)
    }

    fn prune_package_cache(packages_dir: &Path, reserve: u64) -> AppResult<()> {
        remove_temporary_files(packages_dir, ".pkg.tmp.")?;
        if !packages_dir.exists() {
            return Ok(());
        }
        let mut files = Vec::new();
        let mut total_size = 0u64;
        for entry in std::fs::read_dir(packages_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("tgapp") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.len() > MAX_PACKAGE_SIZE as u64 {
                if !metadata.is_dir() {
                    std::fs::remove_file(&path)?;
                }
                continue;
            }
            total_size = total_size.saturating_add(metadata.len());
            files.push((
                path,
                metadata.len(),
                metadata.modified().unwrap_or(UNIX_EPOCH),
            ));
        }
        let limit = MAX_PACKAGE_CACHE_BYTES.saturating_sub(reserve);
        files.sort_by_key(|(_, _, mtime)| *mtime);
        for (path, size, _) in files {
            if total_size <= limit {
                break;
            }
            std::fs::remove_file(path)?;
            total_size -= size;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
