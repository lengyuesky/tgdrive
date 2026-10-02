//! 版本化页面应用包：严格校验 ZIP、清单和资源路径，再写入不可变运行缓存。

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_PACKAGE_SIZE: usize = 16 * 1024 * 1024;
const MAX_EXPANDED_SIZE: usize = 32 * 1024 * 1024;
const MAX_FILE_SIZE: usize = 8 * 1024 * 1024;
const MAX_ICON_SIZE: usize = 256 * 1024;
pub const API_VERSION: u32 = 2;
pub const PERMISSIONS: &[&str] = &["files.read", "media.read", "favorites.write"];

/// 物化完成标记：摘要目录与预压缩目录各有一份，两者齐全才算可用。
pub const COMPLETE_MARK: &str = ".complete";
/// 预压缩变体目录：`<摘要目录>/.enc/<子目录>/<资源路径>`；以点开头，资源路径校验天然拒绝直接访问。
pub const ENCODED_DIR: &str = ".enc";
/// 预压缩变体：`(Content-Encoding 名, 子目录名)`，按服务偏好排序。
pub const ENCODINGS: &[(&str, &str)] = &[("br", "br"), ("gzip", "gz")];
/// 小于此大小的资源不尝试压缩：省不了几个字节；更大的资源仍由"至少省 10%"规则裁决。
const MIN_COMPRESS_SIZE: usize = 64;
/// 内容按摘要不可变，安装时只压一次。质量 9 与 11 相比体积只差约 8%，耗时却少约 8 倍
/// （图书包 4 MiB 原文：release 下 0.5 秒对 3.9 秒），安装与一键更新不至于明显停顿。
const BROTLI_QUALITY: u32 = 9;
const BROTLI_WINDOW: u32 = 22;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub default: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Integration {
    pub file_types: Vec<String>,
    pub directories: bool,
    pub changelog: String,
    pub formats: Vec<String>,
    pub limitations: Vec<String>,
    pub homepage: String,
    pub support: String,
    pub screenshots: Vec<String>,
    pub data_schema: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub min_host_version: String,
    pub description: String,
    pub author: String,
    pub entry: String,
    /// 可选图标：包内 SVG 资源路径；仅安装后由宿主展示，目录中未安装应用仍用字母图标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub settings: Vec<SettingField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<Integration>,
}

pub struct AppPackage {
    pub manifest: Manifest,
    pub digest: String,
    pub archive: Vec<u8>,
    pub files: BTreeMap<String, Vec<u8>>,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn valid_resource(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 240
        && path.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

impl SettingField {
    fn normalize(&self, value: &Value) -> AppResult<Value> {
        match self.kind.as_str() {
            "boolean" if value.is_boolean() => Ok(value.clone()),
            "number" if value.is_number() => Ok(value.clone()),
            "string" | "directory" => {
                let text = value
                    .as_str()
                    .filter(|text| text.len() <= 2048)
                    .ok_or_else(|| invalid(format!("{}必须是最多 2048 字节的文本", self.label)))?;
                if self.kind == "directory" {
                    let segments = crate::vfs::normalize(text)?;
                    Ok(Value::String(format!("/{}", segments.join("/"))))
                } else {
                    Ok(value.clone())
                }
            }
            _ => Err(invalid(format!("{}的设置类型无效", self.label))),
        }
    }
}

impl Manifest {
    pub fn is_compatible(&self) -> bool {
        let host = semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("主程序版本合法");
        let min = semver::Version::parse(&self.min_host_version).ok();
        (1..=API_VERSION).contains(&self.api_version) && min.is_some_and(|m| m <= host)
    }

    pub fn validate_catalog(&self) -> AppResult<()> {
        if !valid_id(&self.id) {
            return Err(invalid(
                "应用 ID 需以小写字母开头，仅含小写字母、数字和短横线，最多 64 字节",
            ));
        }
        for (name, text, limit) in [
            ("名称", self.name.as_str(), 120),
            ("简介", self.description.as_str(), 1600),
            ("作者", self.author.as_str(), 160),
        ] {
            if text.trim().is_empty() || text.len() > limit || text.chars().any(char::is_control) {
                return Err(invalid(format!("应用{name}为空、过长或包含控制字符")));
            }
        }
        if self.version.len() > 80 || self.min_host_version.len() > 80 {
            return Err(invalid("应用版本字段过长"));
        }
        semver::Version::parse(&self.version).map_err(|_| invalid("应用版本需采用 1.0.0 格式"))?;
        semver::Version::parse(&self.min_host_version)
            .map_err(|_| invalid("主程序最低版本格式无效"))?;
        if !valid_resource(&self.entry) || !self.entry.ends_with(".html") {
            return Err(invalid("应用入口必须是包内的 HTML 文件"));
        }
        if let Some(icon) = &self.icon
            && !(valid_resource(icon) && icon.ends_with(".svg"))
        {
            return Err(invalid("应用图标必须是包内的 SVG 文件"));
        }
        if let Some(info) = &self.integration {
            if self.api_version != 2
                || semver::Version::parse(&self.min_host_version).unwrap()
                    < semver::Version::new(0, 3, 0)
            {
                return Err(invalid("系统集成能力需要 API v2 和宿主 0.3.0"));
            }
            let mut extensions = HashSet::new();
            if info.file_types.len() > 32
                || info.file_types.iter().any(|s| {
                    s.is_empty()
                        || s.len() > 16
                        || !s
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                        || !extensions.insert(s)
                })
            {
                return Err(invalid("文件打开类型无效或重复"));
            }
            if (!info.file_types.is_empty() || info.directories)
                && !self.permissions.iter().any(|p| p == "files.read")
            {
                return Err(invalid("文件入口需要 files.read 权限"));
            }
            if info.data_schema == 0
                || info.changelog.len() > 4000
                || info.formats.len() > 24
                || info.limitations.len() > 12
                || info
                    .formats
                    .iter()
                    .any(|s| s.is_empty() || s.len() > 160 || s.chars().any(char::is_control))
                || info
                    .limitations
                    .iter()
                    .any(|s| s.is_empty() || s.len() > 800 || s.chars().any(char::is_control))
            {
                return Err(invalid("应用详情文本超限或无效"));
            }
            for url in [&info.homepage, &info.support] {
                if !url.is_empty()
                    && (url.len() > 2048
                        || reqwest::Url::parse(url).ok().is_none_or(|u| {
                            u.scheme() != "https"
                                || u.host_str().is_none()
                                || !u.username().is_empty()
                                || u.password().is_some()
                        }))
                {
                    return Err(invalid("应用链接必须是无用户凭据的 HTTPS 地址"));
                }
            }
            if info.screenshots.len() > 6
                || info.screenshots.iter().any(|p| {
                    !valid_resource(p)
                        || ![".png", ".jpg", ".webp"].iter().any(|ext| p.ends_with(ext))
                })
            {
                return Err(invalid("应用截图必须是最多六张包内 PNG/JPG/WebP 图片"));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> AppResult<()> {
        self.validate_catalog()?;
        let minimum = semver::Version::parse(&self.min_host_version)
            .map_err(|_| invalid("主程序最低版本格式无效"))?;
        let host = semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("主程序版本合法");
        if !(1..=API_VERSION).contains(&self.api_version) || minimum > host {
            return Err(invalid(format!(
                "应用与当前主程序不兼容：需要接口版本 {}、主程序至少 {}",
                self.api_version, minimum
            )));
        }
        let mut permissions = HashSet::new();
        if self.permissions.len() > PERMISSIONS.len()
            || self
                .permissions
                .iter()
                .any(|value| !PERMISSIONS.contains(&value.as_str()) || !permissions.insert(value))
        {
            return Err(invalid("应用声明了重复或不受支持的权限"));
        }
        if self.settings.len() > 24 {
            return Err(invalid("每个应用最多声明 24 项设置"));
        }
        let mut keys = HashSet::new();
        for field in &self.settings {
            if field.key.is_empty()
                || field.key.len() > 64
                || !field
                    .key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || matches!(
                    field.key.as_str(),
                    "__proto__" | "constructor" | "prototype"
                )
                || !keys.insert(&field.key)
                || field.label.trim().is_empty()
                || field.label.len() > 160
                || field.description.len() > 1200
            {
                return Err(invalid("应用设置声明无效"));
            }
            field.normalize(&field.default)?;
        }
        Ok(())
    }

    pub fn require(&self, permission: &str) -> AppResult<()> {
        if !self.permissions.iter().any(|value| value == permission) {
            return Err(AppError::Forbidden(format!("应用没有 {permission} 权限")));
        }
        Ok(())
    }

    pub fn settings_values(&self, saved: &str) -> AppResult<Map<String, Value>> {
        let saved: Map<String, Value> =
            serde_json::from_str(saved).map_err(|e| AppError::Internal(e.into()))?;
        self.settings
            .iter()
            .map(|field| {
                // 应用升级改变字段类型时使用新默认值，其他有效设置继续保留。
                let value = saved
                    .get(&field.key)
                    .and_then(|value| field.normalize(value).ok())
                    .unwrap_or_else(|| field.default.clone());
                Ok((field.key.clone(), field.normalize(&value)?))
            })
            .collect()
    }

    pub fn validate_patch(&self, patch: Value) -> AppResult<Map<String, Value>> {
        let values = patch
            .as_object()
            .ok_or_else(|| invalid("应用设置必须是 JSON 对象"))?;
        values
            .iter()
            .map(|(key, value)| {
                let field = self
                    .settings
                    .iter()
                    .find(|field| &field.key == key)
                    .ok_or_else(|| invalid(format!("应用没有声明设置项 {key}")))?;
                Ok((key.clone(), field.normalize(value)?))
            })
            .collect()
    }
}

impl AppPackage {
    pub fn parse(archive: Vec<u8>) -> AppResult<Self> {
        if archive.is_empty() || archive.len() > MAX_PACKAGE_SIZE {
            return Err(invalid("插件包为空或超过 16 MiB"));
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(&archive))
            .map_err(|_| invalid("插件包不是有效的 ZIP 文件"))?;
        if zip.len() > 256 {
            return Err(invalid("插件包最多包含 256 个文件和目录"));
        }
        let mut total = 0usize;
        let mut names = HashSet::new();
        let mut files = BTreeMap::new();
        for i in 0..zip.len() {
            let mut file = zip
                .by_index(i)
                .map_err(|_| invalid("无法读取 ZIP 条目，不支持加密包"))?;
            let name = file.name().trim_end_matches('/').to_string();
            if !valid_resource(&name) || !names.insert(name.to_ascii_lowercase()) {
                return Err(invalid("插件包包含非法路径或重复文件"));
            }
            if let Some(mode) = file.unix_mode()
                && !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)
            {
                return Err(invalid("插件包不能包含符号链接或设备文件"));
            }
            if file.is_dir() {
                continue;
            }
            if !matches!(
                file.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            ) || file.size() > MAX_FILE_SIZE as u64
            {
                return Err(invalid("插件文件超过 8 MiB 或采用了不支持的压缩方式"));
            }
            let declared = file.size() as usize;
            total = total
                .checked_add(declared)
                .ok_or_else(|| invalid("插件包大小异常"))?;
            if total > MAX_EXPANDED_SIZE {
                return Err(invalid("插件包解压后不能超过 32 MiB"));
            }
            let mut data = Vec::new();
            (&mut file)
                .take((declared + 1) as u64)
                .read_to_end(&mut data)
                .map_err(|_| invalid("插件文件校验失败"))?;
            if data.len() != declared {
                return Err(invalid("插件文件长度与声明不一致"));
            }
            files.insert(name, data);
        }
        let raw = files
            .get("app.json")
            .filter(|raw| raw.len() <= 64 * 1024)
            .ok_or_else(|| invalid("包根目录需要 app.json，且不能超过 64 KiB"))?;
        let manifest: Manifest =
            serde_json::from_slice(raw).map_err(|e| invalid(format!("应用清单格式无效：{e}")))?;
        manifest.validate()?;
        if !files.contains_key(&manifest.entry) {
            return Err(invalid("应用入口文件不存在"));
        }
        if let Some(icon) = &manifest.icon
            && !files
                .get(icon)
                .is_some_and(|raw| !raw.is_empty() && raw.len() <= MAX_ICON_SIZE)
        {
            return Err(invalid("应用图标必须是包内不超过 256 KiB 的非空 SVG 文件"));
        }
        if let Some(info) = &manifest.integration {
            for path in &info.screenshots {
                if !files
                    .get(path)
                    .is_some_and(|bytes| !bytes.is_empty() && bytes.len() <= 512 * 1024)
                {
                    return Err(invalid("应用截图不存在或超过 512 KiB"));
                }
            }
        }
        drop(zip);
        let digest = crate::engine::upload::to_hex(&Sha256::digest(&archive));
        Ok(Self {
            manifest,
            digest,
            archive,
            files,
        })
    }

    /// 解包到 `<root>/<id>/<摘要>/` 并生成预压缩变体；整目录先在临时目录完成再原子改名。
    pub fn materialize(&self, root: &Path) -> AppResult<PathBuf> {
        let parent = root.join(&self.manifest.id);
        let target = parent.join(&self.digest);
        if is_materialized(&target) {
            return Ok(target);
        }
        if target.join(COMPLETE_MARK).is_file() {
            // 旧版宿主物化的目录只缺预压缩变体：原地补齐，不重新解包。
            precompress(&target)?;
            return Ok(target);
        }
        std::fs::create_dir_all(&parent)?;
        let stage = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&parent)?;
        for (name, data) in &self.files {
            let path = stage.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, data)?;
        }
        precompress(stage.path())?;
        std::fs::write(stage.path().join(COMPLETE_MARK), &self.digest)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::rename(stage.path(), &target)?;
        Ok(target)
    }
}

/// 摘要目录与预压缩目录都带完成标记才算物化完成。
pub fn is_materialized(target: &Path) -> bool {
    target.join(COMPLETE_MARK).is_file() && target.join(ENCODED_DIR).join(COMPLETE_MARK).is_file()
}

/// 已压缩的图片、音视频、归档与字体不再压缩；其余类型交给"至少省 10%"规则裁决。
fn worth_compressing(name: &str) -> bool {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    match mime.type_().as_str() {
        "image" => mime.essence_str() == "image/svg+xml",
        "audio" | "video" => false,
        _ => !matches!(
            mime.subtype().as_str(),
            "zip"
                | "gzip"
                | "x-gzip"
                | "x-bzip2"
                | "x-xz"
                | "x-7z-compressed"
                | "vnd.rar"
                | "x-rar-compressed"
                | "woff"
                | "woff2"
                | "font-woff"
                | "font-woff2"
        ),
    }
}

fn compress_brotli(data: &[u8]) -> AppResult<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 3);
    {
        let mut writer =
            brotli::CompressorWriter::new(&mut out, 32 * 1024, BROTLI_QUALITY, BROTLI_WINDOW);
        writer.write_all(data)?;
        // 流在 drop 时结束（BROTLI_OPERATION_FINISH）。
    }
    Ok(out)
}

fn compress_gzip(data: &[u8]) -> AppResult<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::best(),
    );
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// 为目录内的可压缩资源生成 brotli 与 gzip 旁文件，只保留至少省 10% 的变体。
/// 先在临时目录整体生成，完成后原子改名为 `.enc`；已完成则直接返回，重复调用幂等。
pub fn precompress(directory: &Path) -> AppResult<()> {
    let encoded = directory.join(ENCODED_DIR);
    if encoded.join(COMPLETE_MARK).is_file() {
        return Ok(());
    }
    let stage = tempfile::Builder::new()
        .prefix(".enc-")
        .tempdir_in(directory)?;
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in std::fs::read_dir(directory.join(&relative))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // 跳过完成标记、变体目录与安装临时目录（资源路径不允许以点开头）。
            if name.starts_with('.') {
                continue;
            }
            let child = relative.join(&*name);
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(child);
                continue;
            }
            if !kind.is_file() || !worth_compressing(&name) {
                continue;
            }
            let data = std::fs::read(entry.path())?;
            if data.len() < MIN_COMPRESS_SIZE {
                continue;
            }
            let variants = [compress_brotli(&data)?, compress_gzip(&data)?];
            for ((_, subdir), compressed) in ENCODINGS.iter().zip(variants) {
                if compressed.len() * 10 > data.len() * 9 {
                    continue;
                }
                let path = stage.path().join(subdir).join(&child);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, compressed)?;
            }
        }
    }
    std::fs::write(stage.path().join(COMPLETE_MARK), b"")?;
    if encoded.exists() {
        std::fs::remove_dir_all(&encoded)?;
    }
    std::fs::rename(stage.path(), &encoded)?;
    Ok(())
}

#[cfg(test)]
mod tests;
