//! 锚定目录 FD 的文件操作；缓存与清理不跟随应用可替换的符号链接。

use super::{PlatformError, PlatformResult, opaque};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use tgdrive_contract::error_code;

pub(crate) struct Directory {
    file: File,
}
fn name(value: &str) -> PlatformResult<CString> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(PlatformError::new(
            error_code::INVALID_PATH,
            "内部文件路径无效",
        ));
    }
    CString::new(value)
        .map_err(|_| PlatformError::new(error_code::INVALID_PATH, "内部文件路径无效"))
}
fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}
fn checked_file(fd: RawFd, directory: bool) -> PlatformResult<File> {
    checked_file_owner(fd, directory, None)
}
fn checked_file_owner(
    fd: RawFd,
    directory: bool,
    extra_owner: Option<u32>,
) -> PlatformResult<File> {
    if fd < 0 {
        return Err(errno().into());
    }
    // FD 已由 open/openat 新分配，立即转移所有权，所有错误路径都会关闭它。
    let file = unsafe { File::from_raw_fd(fd) };
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    let owner = unsafe { libc::geteuid() };
    if (metadata.uid() != owner && extra_owner != Some(metadata.uid()))
        || (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(PlatformError::new(
            error_code::INVALID_PATH,
            "内部文件类型、所有者或链接数无效",
        ));
    }
    Ok(file)
}
impl Directory {
    pub fn open(path: &Path, create: bool) -> PlatformResult<Self> {
        if create {
            std::fs::create_dir_all(path)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() {
            return Err(PlatformError::new(
                error_code::INVALID_PATH,
                "内部根目录类型无效",
            ));
        }
        Ok(Self { file })
    }
    pub fn child(&self, value: &str, create: bool) -> PlatformResult<Self> {
        self.child_owner(value, create, None)
    }
    pub fn child_owner(
        &self,
        value: &str,
        create: bool,
        owner: Option<u32>,
    ) -> PlatformResult<Self> {
        let value = name(value)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // 父 FD 持有已验证目录；O_NOFOLLOW 阻止逐级路径替换为链接。
        let mut fd = unsafe { libc::openat(self.file.as_raw_fd(), value.as_ptr(), flags) };
        if fd < 0 && create && errno().kind() == std::io::ErrorKind::NotFound {
            let created = unsafe { libc::mkdirat(self.file.as_raw_fd(), value.as_ptr(), 0o700) };
            if created < 0 && errno().kind() != std::io::ErrorKind::AlreadyExists {
                return Err(errno().into());
            }
            fd = unsafe { libc::openat(self.file.as_raw_fd(), value.as_ptr(), flags) };
        }
        Ok(Self {
            file: checked_file_owner(fd, true, owner)?,
        })
    }
    pub fn permissions(&self, uid: u32, gid: u32, mode: u32) -> PlatformResult<()> {
        if unsafe { libc::fchown(self.file.as_raw_fd(), uid, gid) } < 0
            || unsafe { libc::fchmod(self.file.as_raw_fd(), mode as libc::mode_t) } < 0
        {
            return Err(errno().into());
        }
        Ok(())
    }
    pub fn file_permissions(
        &self,
        value: &str,
        uid: u32,
        gid: u32,
        mode: u32,
    ) -> PlatformResult<()> {
        let file = self.read(value)?;
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } < 0
            || unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } < 0
        {
            return Err(errno().into());
        }
        Ok(())
    }
    pub fn usage(&self, extra_owner: Option<u32>) -> PlatformResult<u64> {
        let mut total = 0u64;
        for value in self.names()? {
            let key = name(&value)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    key.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                if errno().kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(errno().into());
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                total = total.saturating_add(
                    self.child_owner(&value, false, extra_owner)?
                        .usage(extra_owner)?,
                );
            } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
                total = total.saturating_add(stat.st_size.max(0) as u64);
            } else if stat.st_mode & libc::S_IFMT != libc::S_IFLNK {
                return Err(PlatformError::new(
                    error_code::INVALID_PATH,
                    "应用目录含特殊文件",
                ));
            }
        }
        Ok(total)
    }
    pub fn read(&self, value: &str) -> PlatformResult<File> {
        let value = name(value)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                value.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        checked_file(fd, false)
    }
    pub fn read_bytes(&self, value: &str, maximum: u64) -> PlatformResult<Vec<u8>> {
        let file = self.read(value)?;
        if file.metadata()?.len() > maximum {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "内部对象长度超过上限",
            ));
        }
        let mut bytes = Vec::new();
        file.take(maximum + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > maximum {
            return Err(PlatformError::new(
                error_code::PAYLOAD_TOO_LARGE,
                "内部对象长度超过上限",
            ));
        }
        Ok(bytes)
    }
    pub fn write_atomic(&self, value: &str, bytes: &[u8]) -> PlatformResult<()> {
        let target = name(value)?;
        let temporary = name(&format!("tmp-{}", opaque()))?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        let mut file = checked_file(fd, false)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            let renamed = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    temporary.as_ptr(),
                    self.file.as_raw_fd(),
                    target.as_ptr(),
                )
            };
            if renamed < 0 {
                return Err(errno().into());
            }
            self.file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(self.file.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }
    pub fn remove(&self, value: &str) -> PlatformResult<()> {
        let value = name(value)?;
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), value.as_ptr(), 0) };
        if result < 0 && errno().kind() != std::io::ErrorKind::NotFound {
            return Err(errno().into());
        }
        Ok(())
    }
    pub fn names(&self) -> PlatformResult<Vec<String>> {
        let duplicate = unsafe { libc::dup(self.file.as_raw_fd()) };
        if duplicate < 0 {
            return Err(errno().into());
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(errno().into());
        }
        unsafe { libc::rewinddir(directory) };
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                break;
            }
            let value = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            if value != "." && value != ".." {
                names.push(value);
            }
        }
        unsafe { libc::closedir(directory) };
        Ok(names)
    }
    fn clear(&self) -> PlatformResult<()> {
        self.clear_owner(None)
    }
    fn clear_owner(&self, owner: Option<u32>) -> PlatformResult<()> {
        // 包目录不可变；卸载清理时只修改已打开、宿主拥有的目录 FD 权限。
        if unsafe { libc::fchmod(self.file.as_raw_fd(), 0o700) } < 0 {
            return Err(errno().into());
        }
        for value in self.names()? {
            let key = name(&value)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    key.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result < 0 {
                if errno().kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(errno().into());
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = self.child_owner(&value, false, owner)?;
                child.clear_owner(owner)?;
                if unsafe {
                    libc::unlinkat(self.file.as_raw_fd(), key.as_ptr(), libc::AT_REMOVEDIR)
                } < 0
                {
                    return Err(errno().into());
                }
            } else {
                self.remove(&value)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn remove_tree(root: &Path, value: &str) -> PlatformResult<()> {
    remove_tree_owner(root, value, None)
}
pub(crate) fn remove_tree_owner(
    root: &Path,
    value: &str,
    owner: Option<u32>,
) -> PlatformResult<()> {
    if !root.exists() {
        return Ok(());
    }
    let parent = Directory::open(root, false)?;
    let key = name(value)?;
    let child = match parent.child_owner(value, false, owner) {
        Ok(child) => child,
        Err(error) if error.code == error_code::NOT_FOUND => return Ok(()),
        Err(error) if error.code == error_code::INTERNAL_ERROR => {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    parent.file.as_raw_fd(),
                    key.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
                && errno().kind() == std::io::ErrorKind::NotFound
            {
                return Ok(());
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if owner.is_none() {
        child.clear()?;
    } else {
        child.clear_owner(owner)?;
    }
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), key.as_ptr(), libc::AT_REMOVEDIR) } < 0
        && errno().kind() != std::io::ErrorKind::NotFound
    {
        return Err(errno().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn links_never_redirect_cache_reads_writes_or_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"kept").unwrap();
        let directory = Directory::open(root.path(), false).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(directory.child("linked", false).is_err());
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("file"))
            .unwrap();
        assert!(directory.read("file").is_err());
        directory.write_atomic("file", b"own").unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"kept"
        );
        directory.clear().unwrap();
        assert!(outside.path().join("secret").exists());
    }
}
