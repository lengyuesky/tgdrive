//! 真正的 ZIP/CRC/静态 ELF 校验。全部输出先写入新的空目录，任何失败都不切换安装。

use super::{PlatformError, PlatformResult};
use crc32fast::Hasher as Crc32;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tgdrive_contract::ByteCount;
use tgdrive_contract::error_code;
use tgdrive_contract::http::*;
use tgdrive_contract::manifest::*;
use tgdrive_contract::runtime::InstalledPackage;
use tgdrive_contract::validation::{
    parse_manifest, parse_package_metadata, valid_resource_path, validate_archive_layout,
};

#[derive(Debug, Clone)]
pub(crate) struct VerifiedPackage {
    pub manifest: Manifest,
    pub package: InstalledPackage,
    pub directory: PathBuf,
}

#[derive(Debug)]
struct ZipEntry {
    path: String,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u64,
    expanded: u64,
    local_offset: u64,
    kind: ArchiveEntryKind,
}

fn invalid(message: &str) -> PlatformError {
    PlatformError::new(error_code::INVALID_PACKAGE, message)
}
fn u16_at(bytes: &[u8], offset: usize) -> PlatformResult<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| invalid("ZIP 结构被截断"))
}
fn u32_at(bytes: &[u8], offset: usize) -> PlatformResult<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("已检查四字节")))
        .ok_or_else(|| invalid("ZIP 结构被截断"))
}
fn u64_at(bytes: &[u8], offset: usize) -> PlatformResult<u64> {
    bytes
        .get(offset..offset + 8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("已检查八字节")))
        .ok_or_else(|| invalid("ELF 结构被截断"))
}
fn read_at(file: &mut File, offset: u64, length: usize) -> PlatformResult<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut out = vec![0; length];
    file.read_exact(&mut out)
        .map_err(|_| invalid("包结构被截断"))?;
    Ok(out)
}
fn check_extra(bytes: &[u8]) -> PlatformResult<()> {
    let mut position = 0;
    while position < bytes.len() {
        let kind = u16_at(bytes, position)?;
        let length = usize::from(u16_at(bytes, position + 2)?);
        position = position
            .checked_add(4 + length)
            .ok_or_else(|| invalid("ZIP 扩展长度溢出"))?;
        if position > bytes.len() || matches!(kind, 0x0001 | 0x000d | 0x0017 | 0x756e | 0x9901) {
            return Err(invalid("不支持 ZIP64、链接或加密扩展"));
        }
    }
    Ok(())
}

fn central_directory(file: &mut File, size: u64) -> PlatformResult<(Vec<ZipEntry>, u64)> {
    if !(22..=MAX_PACKAGE_BYTES).contains(&size) {
        return Err(PlatformError::new(
            if size > MAX_PACKAGE_BYTES {
                error_code::PAYLOAD_TOO_LARGE
            } else {
                error_code::INVALID_PACKAGE
            },
            "应用包长度无效或超过上限",
        ));
    }
    let tail_length = size.min(22 + u16::MAX as u64) as usize;
    let tail = read_at(file, size - tail_length as u64, tail_length)?;
    let position = (0..=tail.len() - 22)
        .rev()
        .find(|&p| {
            tail[p..p + 4] == [0x50, 0x4b, 0x05, 0x06]
                && u16_at(&tail, p + 20).is_ok_and(|n| p + 22 + usize::from(n) == tail.len())
        })
        .ok_or_else(|| invalid("缺少合法 ZIP 结束记录"))?;
    let end = &tail[position..];
    let count = u16_at(end, 10)?;
    let central_size = u64::from(u32_at(end, 12)?);
    let central_offset = u64::from(u32_at(end, 16)?);
    let eocd_offset = size - tail_length as u64 + position as u64;
    if u16_at(end, 4)? != 0
        || u16_at(end, 6)? != 0
        || u16_at(end, 8)? != count
        || count == u16::MAX
        || u32_at(end, 12)? == u32::MAX
        || u32_at(end, 16)? == u32::MAX
        || central_offset.checked_add(central_size) != Some(eocd_offset)
        || count == 0
        || u64::from(count) > MAX_PACKAGE_ENTRIES
    {
        return Err(invalid("不支持分卷、ZIP64 或不一致的中央目录"));
    }
    let mut entries = Vec::with_capacity(usize::from(count));
    let mut position = central_offset;
    let mut folded = BTreeSet::new();
    for _ in 0..count {
        if position + 46 > eocd_offset {
            return Err(invalid("中央目录越界"));
        }
        let header = read_at(file, position, 46)?;
        if u32_at(&header, 0)? != 0x02014b50
            || u16_at(&header, 6)? >= 45
            || u16_at(&header, 34)? != 0
        {
            return Err(invalid("中央目录格式或 ZIP 版本无效"));
        }
        let flags = u16_at(&header, 8)?;
        let method = u16_at(&header, 10)?;
        if flags & !0x080e != 0 || !matches!(method, 0 | 8) || (method == 0 && flags & 6 != 0) {
            return Err(invalid("ZIP 加密、标志或压缩方式不受支持"));
        }
        let name_length = usize::from(u16_at(&header, 28)?);
        let extra_length = usize::from(u16_at(&header, 30)?);
        let comment_length = usize::from(u16_at(&header, 32)?);
        let variable_length = name_length + extra_length + comment_length;
        if position + 46 + variable_length as u64 > eocd_offset {
            return Err(invalid("中央目录条目越界"));
        }
        let variable = read_at(file, position + 46, variable_length)?;
        let path = std::str::from_utf8(&variable[..name_length])
            .map_err(|_| invalid("包路径不是 UTF-8"))?
            .to_owned();
        let is_directory = path.ends_with('/');
        let normalized = path.strip_suffix('/').unwrap_or(&path);
        if path.len() > MAX_RESOURCE_PATH_BYTES as usize
            || !valid_resource_path(normalized)
            || !folded.insert(normalized.to_ascii_lowercase())
        {
            return Err(invalid("包包含非法或大小写折叠后重复的路径"));
        }
        check_extra(&variable[name_length..name_length + extra_length])?;
        let external = u32_at(&header, 38)?;
        let unix_type = (external >> 16) & 0xf000;
        if !matches!(unix_type, 0 | 0x4000 | 0x8000)
            || (unix_type == 0x4000 && !is_directory)
            || (unix_type == 0x8000 && is_directory)
            || (external & 0x10 != 0 && !is_directory)
        {
            return Err(invalid("包不允许链接、设备或不一致的文件种类"));
        }
        let compressed = u32_at(&header, 20)?;
        let expanded = u32_at(&header, 24)?;
        let offset = u32_at(&header, 42)?;
        if [compressed, expanded, offset].contains(&u32::MAX)
            || u64::from(expanded) > MAX_PACKAGE_FILE_BYTES
        {
            return Err(invalid("ZIP64 或单文件大小不受支持"));
        }
        if is_directory
            && ((method == 0 && compressed != 0) || expanded != 0 || u32_at(&header, 16)? != 0)
        {
            return Err(invalid("目录条目必须没有正文"));
        }
        entries.push(ZipEntry {
            path,
            flags,
            method,
            crc: u32_at(&header, 16)?,
            compressed: u64::from(compressed),
            expanded: u64::from(expanded),
            local_offset: u64::from(offset),
            kind: if is_directory {
                ArchiveEntryKind::Directory
            } else {
                ArchiveEntryKind::File
            },
        });
        position += 46 + variable_length as u64;
    }
    if position != eocd_offset {
        return Err(invalid("中央目录包含未声明的数据"));
    }
    for path in [MANIFEST_PATH, PACKAGE_METADATA_PATH] {
        if !entries.iter().any(|entry| {
            entry.path == path
                && entry.kind == ArchiveEntryKind::File
                && entry.expanded > 0
                && entry.expanded <= MAX_MANIFEST_BYTES
        }) {
            return Err(invalid("包元数据必须存在、非空且满足大小上限"));
        }
    }
    let folded_files = entries
        .iter()
        .filter(|entry| entry.kind == ArchiveEntryKind::File)
        .map(|entry| entry.path.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    for entry in &entries {
        let mut parent = entry
            .path
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(parent, _)| parent);
        while let Some(path) = parent {
            if folded_files.contains(&path.to_ascii_lowercase()) {
                return Err(invalid("包内文件与目录互相覆盖"));
            }
            parent = path.rsplit_once('/').map(|(parent, _)| parent);
        }
    }
    let mut tree_paths = std::collections::BTreeMap::new();
    for entry in &entries {
        let mut path = String::new();
        for part in entry.path.trim_end_matches('/').split('/') {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            let folded = path.to_ascii_lowercase();
            if let Some(previous) = tree_paths.insert(folded, path.clone())
                && previous != path
            {
                return Err(invalid("隐式目录存在大小写别名"));
            }
            if tree_paths.len() as u64 > MAX_PACKAGE_ENTRIES {
                return Err(invalid("包内文件和隐式目录总数超过上限"));
            }
        }
    }
    entries.sort_by_key(|e| e.local_offset);
    Ok((entries, central_offset))
}

fn extract_entry(
    file: &mut File,
    entry: &ZipEntry,
    central_offset: u64,
    staging: &Path,
    total: &mut u64,
) -> PlatformResult<u64> {
    let header = read_at(file, entry.local_offset, 30)?;
    if u32_at(&header, 0)? != 0x04034b50
        || u16_at(&header, 4)? >= 45
        || u16_at(&header, 6)? != entry.flags
        || u16_at(&header, 8)? != entry.method
    {
        return Err(invalid("本地文件头与中央目录不符"));
    }
    let name_length = usize::from(u16_at(&header, 26)?);
    let extra_length = usize::from(u16_at(&header, 28)?);
    let variable = read_at(file, entry.local_offset + 30, name_length + extra_length)?;
    if variable[..name_length] != *entry.path.as_bytes() {
        return Err(invalid("本地文件名与中央目录不符"));
    }
    check_extra(&variable[name_length..])?;
    let expected = [entry.crc, entry.compressed as u32, entry.expanded as u32];
    for (offset, want) in [14, 18, 22].into_iter().zip(expected) {
        let actual = u32_at(&header, offset)?;
        if actual != want && !(entry.flags & 8 != 0 && actual == 0) {
            return Err(invalid("本地文件长度或 CRC 与中央目录不符"));
        }
    }
    let start = entry.local_offset + 30 + (name_length + extra_length) as u64;
    let end = start
        .checked_add(entry.compressed)
        .filter(|end| *end <= central_offset)
        .ok_or_else(|| invalid("ZIP 压缩数据越界"))?;
    let path = staging.join(entry.path.strip_suffix('/').unwrap_or(&entry.path));
    if entry.kind == ArchiveEntryKind::Directory {
        fs::create_dir_all(path)?;
        if entry.method == 8 && entry.compressed > 0 {
            file.seek(SeekFrom::Start(start))?;
            let mut decoder =
                flate2::read::DeflateDecoder::new((&mut *file).take(entry.compressed));
            copy_checked(&mut decoder, &mut std::io::sink(), entry, total)?;
            if decoder.total_in() != entry.compressed {
                return Err(invalid("目录 Deflate 条目含多余压缩数据"));
            }
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.seek(SeekFrom::Start(start))?;
        let bounded = (&mut *file).take(entry.compressed);
        if entry.method == 0 {
            copy_checked(bounded, &mut output, entry, total)?;
            if entry.compressed != entry.expanded {
                return Err(invalid("Stored 条目长度不符"));
            }
        } else {
            let mut decoder = flate2::read::DeflateDecoder::new(bounded);
            copy_checked(&mut decoder, &mut output, entry, total)?;
            if decoder.total_in() != entry.compressed {
                return Err(invalid("Deflate 条目含未消费的压缩数据"));
            }
        }
        output.sync_all()?;
    }
    if entry.flags & 8 == 0 {
        return Ok(end);
    }
    let descriptor = read_at(file, end, 4)?;
    let signature = u32_at(&descriptor, 0)? == 0x08074b50;
    let descriptor_start = end + if signature { 4 } else { 0 };
    if descriptor_start + 12 > central_offset {
        return Err(invalid("ZIP 数据描述符越界"));
    }
    let descriptor = read_at(file, descriptor_start, 12)?;
    if u32_at(&descriptor, 0)? != entry.crc
        || u32_at(&descriptor, 4)? != entry.compressed as u32
        || u32_at(&descriptor, 8)? != entry.expanded as u32
    {
        return Err(invalid("ZIP 数据描述符与中央目录不符"));
    }
    Ok(descriptor_start + 12)
}

fn copy_checked(
    mut reader: impl Read,
    output: &mut impl Write,
    entry: &ZipEntry,
    total: &mut u64,
) -> PlatformResult<()> {
    let mut crc = Crc32::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|_| invalid("ZIP 解压失败"))?;
        if n == 0 {
            break;
        }
        length = length
            .checked_add(n as u64)
            .ok_or_else(|| invalid("解压长度溢出"))?;
        *total = total
            .checked_add(n as u64)
            .ok_or_else(|| invalid("解压累计长度溢出"))?;
        if length > entry.expanded
            || length > MAX_PACKAGE_FILE_BYTES
            || *total > MAX_PACKAGE_EXPANDED_BYTES
        {
            return Err(invalid("ZIP 实际解压长度超过声明或上限"));
        }
        crc.update(&buffer[..n]);
        output.write_all(&buffer[..n])?;
    }
    if length != entry.expanded || crc.finalize() != entry.crc {
        return Err(invalid("ZIP 实际长度或 CRC 不符"));
    }
    Ok(())
}

/// 从真实 ELF64 程序头检查架构与静态依赖；不依赖扩展名或包权限。
fn validate_elf(path: &Path, target: PackageTarget) -> PlatformResult<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let header = read_at(&mut file, 0, 64)?;
    if &header[..4] != b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || !matches!(header[7], 0 | 3)
        || !matches!(u16_at(&header, 16)?, 2 | 3)
        || u32_at(&header, 20)? != 1
        || u16_at(&header, 52)? != 64
        || u16_at(&header, 54)? != 56
    {
        return Err(invalid("后端必须是有效的 Linux 64 位小端 ELF"));
    }
    let machine = match target {
        PackageTarget::LinuxAmd64 => 62,
        PackageTarget::LinuxArm64 => 183,
        PackageTarget::Web => return Err(invalid("页面目标不能有后端")),
    };
    if u16_at(&header, 18)? != machine {
        return Err(invalid("ELF 架构与构建目标不符"));
    }
    let offset = u64_at(&header, 32)?;
    let count = u16_at(&header, 56)?;
    if count == 0
        || count == u16::MAX
        || offset < 64
        || offset
            .checked_add(u64::from(count) * 56)
            .is_none_or(|end| end > size)
    {
        return Err(invalid("ELF 程序头越界"));
    }
    let mut has_load = false;
    let mut executable_entry = false;
    let entry = u64_at(&header, 24)?;
    for index in 0..count {
        let program = read_at(&mut file, offset + u64::from(index) * 56, 56)?;
        let kind = u32_at(&program, 0)?;
        let file_offset = u64_at(&program, 8)?;
        let file_size = u64_at(&program, 32)?;
        let memory_size = u64_at(&program, 40)?;
        if file_offset
            .checked_add(file_size)
            .is_none_or(|end| end > size)
            || (kind == 1 && file_size > memory_size)
        {
            return Err(invalid("ELF 段越界"));
        }
        if kind == 3 {
            return Err(invalid("后端不得依赖动态解释器 PT_INTERP"));
        }
        if kind == 1 {
            has_load = true;
            let virtual_address = u64_at(&program, 16)?;
            executable_entry |= u32_at(&program, 4)? & 1 != 0
                && entry >= virtual_address
                && virtual_address
                    .checked_add(memory_size)
                    .is_some_and(|end| entry < end);
        }
        if kind == 2 {
            if file_size % 16 != 0 {
                return Err(invalid("ELF 动态段长度无效"));
            }
            let mut terminated = false;
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, file.try_clone()?);
            reader.seek(SeekFrom::Start(file_offset))?;
            for _ in 0..file_size / 16 {
                let mut dynamic = [0u8; 16];
                reader
                    .read_exact(&mut dynamic)
                    .map_err(|_| invalid("ELF 动态段被截断"))?;
                let tag = u64_at(&dynamic, 0)?;
                if tag == 0 {
                    terminated = true;
                    break;
                }
                if matches!(tag, 1 | 0x7ffffffd | 0x7fffffff) {
                    return Err(invalid("后端不得有 DT_NEEDED 或加载过滤依赖"));
                }
            }
            if !terminated {
                return Err(invalid("ELF 动态段未终止"));
            }
        }
    }
    if !has_load || !executable_entry {
        return Err(invalid("ELF 缺少可执行入口或加载段"));
    }
    Ok(())
}

pub(crate) fn verify_and_materialize(
    zip: &Path,
    package_root: &Path,
) -> PlatformResult<VerifiedPackage> {
    fs::create_dir_all(package_root)?;
    let mut file = File::open(zip)?;
    let size = file.metadata()?.len();
    if size > MAX_PACKAGE_BYTES {
        return Err(PlatformError::new(
            error_code::PAYLOAD_TOO_LARGE,
            "应用包超过压缩上限",
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let sha256 = crate::engine::upload::to_hex(&digest.finalize());
    let (entries, central_offset) = central_directory(&mut file, size)?;
    let staging = tempfile::Builder::new()
        .prefix("staging-")
        .tempdir_in(package_root)?;
    let mut total = 0;
    let mut position = 0;
    for entry in &entries {
        if entry.local_offset != position {
            return Err(invalid("ZIP 本地条目重叠或含未声明数据"));
        }
        position = extract_entry(&mut file, entry, central_offset, staging.path(), &mut total)?;
    }
    if position != central_offset {
        return Err(invalid("ZIP 本地文件区长度不符"));
    }
    let manifest = parse_manifest(&fs::read(staging.path().join(MANIFEST_PATH))?)?;
    let metadata = parse_package_metadata(&fs::read(staging.path().join(PACKAGE_METADATA_PATH))?)?;
    let layout = entries
        .iter()
        .map(|entry| ArchiveEntry {
            path: entry.path.clone(),
            kind: entry.kind,
            expanded_size: entry.expanded,
            compression: if entry.method == 0 {
                ArchiveCompression::Stored
            } else {
                ArchiveCompression::Deflate
            },
            encrypted: false,
        })
        .collect::<Vec<_>>();
    validate_archive_layout(&manifest, &metadata, &layout)?;
    if manifest.backend.is_some() {
        validate_elf(&staging.path().join(BACKEND_ENTRY), metadata.target)?;
    }
    let directory = package_root.join(&sha256);
    if directory.exists() {
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() || !directory.is_dir() {
            return Err(invalid("包目录不是可信普通目录"));
        }
        // 即使命中摘要目录也重新验 ZIP；验证过的目录由宿主独占，不接受应用写入。
    } else {
        fs::rename(staging.path(), &directory)?;
        // 校验和原子物化完成后再收紧权限，失败 staging 与重复校验目录仍可完整清理。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in entries.iter().rev() {
                let path = directory.join(entry.path.trim_end_matches('/'));
                let mode =
                    if entry.kind == ArchiveEntryKind::Directory || entry.path == BACKEND_ENTRY {
                        0o555
                    } else {
                        0o444
                    };
                fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            }
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o555))?;
        }
    }
    Ok(VerifiedPackage {
        manifest,
        package: InstalledPackage {
            target: metadata.target,
            sha256,
            size: ByteCount(size),
        },
        directory,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 测试现场生成 ZIP，既不依赖应用构建产物，也不执行包内代码。
    pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        zip_options(entries, 0, false)
    }
    fn zip_options(entries: &[(&str, &[u8])], method: u16, descriptor: bool) -> Vec<u8> {
        let mut local = Vec::new();
        let mut central = Vec::new();
        let flags = if descriptor { 8u16 } else { 0 };
        for (name, body) in entries {
            let compressed = if method == 8 {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body).unwrap();
                encoder.finish().unwrap()
            } else {
                body.to_vec()
            };
            let offset = local.len() as u32;
            let crc = crc32fast::hash(body);
            let compressed_size = compressed.len() as u32;
            let expanded_size = body.len() as u32;
            local.extend(0x04034b50u32.to_le_bytes());
            for value in [20u16, flags, method, 0, 0] {
                local.extend(value.to_le_bytes());
            }
            for value in if descriptor {
                [0, 0, 0]
            } else {
                [crc, compressed_size, expanded_size]
            } {
                local.extend(value.to_le_bytes());
            }
            local.extend((name.len() as u16).to_le_bytes());
            local.extend(0u16.to_le_bytes());
            local.extend(name.as_bytes());
            local.extend(compressed);
            if descriptor {
                local.extend(0x08074b50u32.to_le_bytes());
                for value in [crc, compressed_size, expanded_size] {
                    local.extend(value.to_le_bytes());
                }
            }
            central.extend(0x02014b50u32.to_le_bytes());
            for value in [0x0314u16, 20, flags, method, 0, 0] {
                central.extend(value.to_le_bytes());
            }
            for value in [crc, compressed_size, expanded_size] {
                central.extend(value.to_le_bytes());
            }
            for value in [name.len() as u16, 0, 0, 0, 0] {
                central.extend(value.to_le_bytes());
            }
            let mode = if name.ends_with('/') {
                0x41ed0000u32
            } else {
                0x81a40000u32
            };
            central.extend(mode.to_le_bytes());
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let offset = local.len() as u32;
        let size = central.len() as u32;
        local.extend(central);
        local.extend(0x06054b50u32.to_le_bytes());
        for value in [0u16, 0, entries.len() as u16, entries.len() as u16] {
            local.extend(value.to_le_bytes());
        }
        local.extend(size.to_le_bytes());
        local.extend(offset.to_le_bytes());
        local.extend(0u16.to_le_bytes());
        local
    }
    pub fn page_package(version: &str) -> Vec<u8> {
        let manifest = format!(
            r#"{{"id":"test-page","name":"测试页面","version":"{version}","description":"测试契约","author":"测试","contract":{{"major":1,"minor":0}},"files":{{"read":true,"write":true}},"page":{{"entry":"page/index.html"}}}}"#
        );
        zip(&[
            (MANIFEST_PATH, manifest.as_bytes()),
            (
                PACKAGE_METADATA_PATH,
                br#"{"format":{"major":1,"minor":0},"target":"web"}"#,
            ),
            (
                "page/index.html",
                b"<!doctype html><script src=\"app.js\"></script>",
            ),
            ("page/app.js", b"document.body.textContent='test';"),
        ])
    }
    fn verify(bytes: &[u8]) -> PlatformResult<VerifiedPackage> {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("input.tgapp");
        fs::write(&source, bytes).unwrap();
        verify_and_materialize(&source, &dir.path().join("packages"))
    }
    #[test]
    fn deflate_data_descriptors_and_empty_directories_are_verified() {
        let manifest=r#"{"id":"deflate-page","name":"测试","version":"1.0.0","contract":{"major":1,"minor":0},"description":"测试","author":"测试","page":{"entry":"page/index.html"}}"#.as_bytes();
        let entries = [
            (MANIFEST_PATH, manifest),
            (
                PACKAGE_METADATA_PATH,
                br#"{"format":{"major":1,"minor":0},"target":"web"}"#.as_slice(),
            ),
            ("page/", b"".as_slice()),
            ("page/index.html", b"<!doctype html>".as_slice()),
        ];
        for descriptor in [false, true] {
            let bytes = zip_options(&entries, 8, descriptor);
            assert!(verify(&bytes).is_ok());
        }
    }
    #[test]
    fn implicit_directories_count_toward_entry_budget_and_have_no_case_aliases() {
        let mut entries=vec![(MANIFEST_PATH,r#"{"id":"budget-page","name":"测试","version":"1.0.0","contract":{"major":1,"minor":0},"description":"测试","author":"测试","page":{"entry":"page/index.html"}}"#.as_bytes()),(PACKAGE_METADATA_PATH,br#"{"format":{"major":1,"minor":0},"target":"web"}"#.as_slice()),("page/index.html",b"html".as_slice())];
        let paths = (0..1000)
            .map(|i| format!("assets/{i}/a/b/c/file.txt"))
            .collect::<Vec<_>>();
        for path in &paths {
            entries.push((path.as_str(), b"x"));
        }
        assert!(verify(&zip(&entries)).is_err());
        assert!(
            verify(&zip(&[
                (MANIFEST_PATH, entries[0].1),
                (PACKAGE_METADATA_PATH, entries[1].1),
                ("page/index.html", b"html"),
                ("page/A/one.js", b"one"),
                ("page/a/two.js", b"two")
            ]))
            .is_err()
        );
    }
    #[test]
    fn real_zip_validates_and_binds_digest() {
        let bytes = page_package("1.0.0");
        let result = verify(&bytes).unwrap();
        assert_eq!(result.manifest.id, "test-page");
        assert_eq!(result.package.size.0, bytes.len() as u64);
        assert_eq!(
            result.package.sha256,
            crate::engine::upload::to_hex(&Sha256::digest(&bytes))
        );
    }
    #[test]
    fn rejects_crc_local_mismatch_links_and_duplicate_paths() {
        let good = page_package("1.0.0");
        let mut bad = good.clone();
        bad[14] ^= 1;
        assert!(verify(&bad).is_err());
        let mut bad = good.clone();
        bad[30] ^= 1;
        assert!(verify(&bad).is_err());
        let mut bad = good.clone();
        let pos = bad
            .windows(4)
            .position(|b| b == 0x02014b50u32.to_le_bytes())
            .unwrap();
        bad[pos + 38..pos + 42].copy_from_slice(&0xa1ff0000u32.to_le_bytes());
        assert!(verify(&bad).is_err());
        for name in [
            "../escape",
            "/absolute",
            "page/.hidden",
            "page/a\\b",
            "page/é.js",
        ] {
            assert!(verify(&zip(&[(name, b"bad")])).is_err());
        }
        assert!(verify(&zip(&[("page/A.js", b"a"), ("page/a.js", b"b")])).is_err());
    }
    #[test]
    fn rejects_zip64_multi_disk_and_trailing_payload() {
        let mut bad = page_package("1.0.0");
        let end = bad.len() - 22;
        bad[end + 4] = 1;
        assert!(verify(&bad).is_err());
        let mut bad = page_package("1.0.0");
        let end = bad.len() - 22;
        bad[end + 10..end + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(verify(&bad).is_err());
        let mut bad = page_package("1.0.0");
        bad.extend(b"hidden");
        assert!(verify(&bad).is_err());
    }
    #[test]
    fn real_elf_rejects_wrong_architecture_and_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app");
        let mut elf = vec![0u8; 120];
        elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[24..32].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        elf[64..68].copy_from_slice(&1u32.to_le_bytes());
        elf[68..72].copy_from_slice(&5u32.to_le_bytes());
        elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
        elf[96..104].copy_from_slice(&120u64.to_le_bytes());
        elf[104..112].copy_from_slice(&120u64.to_le_bytes());
        fs::write(&path, &elf).unwrap();
        assert!(validate_elf(&path, PackageTarget::LinuxAmd64).is_ok());
        assert!(validate_elf(&path, PackageTarget::LinuxArm64).is_err());
        elf[64..68].copy_from_slice(&3u32.to_le_bytes());
        fs::write(&path, &elf).unwrap();
        assert!(validate_elf(&path, PackageTarget::LinuxAmd64).is_err());
    }
}
