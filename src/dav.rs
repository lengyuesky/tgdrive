//! WebDAV 层:dav-server 的 DavFileSystem/DavFile 适配到 Vfs
//!
//! - MemLs 提供 Class 2 LOCK(Windows/Finder 需要)
//! - PUT 为顺序写(dav-server 只在 Apache 风格部分 PUT 时 seek,本实现拒绝)
//! - 目录 COPY 由 dav-server 自行递归(create_dir + 逐文件 copy)

use crate::api::AppState;
use crate::db::Node;
use crate::engine::reader::ChunkedReader;
use crate::engine::upload::UploadHandle;
use crate::error::AppError;
use crate::vfs::Vfs;
use axum::extract::{Request, State};
use axum::response::{IntoResponse as _, Response};
use bytes::{Buf, Bytes};
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dav_server::memls::MemLs;
use std::io::SeekFrom;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

pub const DAV_PREFIX: &str = "/dav";

pub fn build_handler(vfs: Vfs) -> DavHandler {
    DavHandler::builder()
        .strip_prefix(DAV_PREFIX)
        .filesystem(Box::new(TgDavFs { vfs }))
        .locksystem(MemLs::new())
        .build_handler()
}

/// axum 入口:/dav 全部方法(含 PROPFIND 等扩展方法),Basic 认证失败回 401+挑战头
///
/// 只接受 Basic,不认浏览器登录 Cookie:跨站导航带上的 Cookie 不能借 /dav 读写网盘。
pub async fn dav_handler(State(state): State<AppState>, req: Request) -> Response {
    match crate::api::authorize(&state, &req, false) {
        Ok(()) => {}
        Err(AppError::Unauthorized) => {
            return Response::builder()
                .status(http::StatusCode::UNAUTHORIZED)
                .header(http::header::WWW_AUTHENTICATE, "Basic realm=\"tgdrive\"")
                .body(axum::body::Body::from("未认证"))
                .expect("响应构造失败");
        }
        Err(error) => return error.into_response(),
    }
    let download = matches!(*req.method(), http::Method::GET | http::Method::HEAD);
    let mut resp = state.dav.handle(req).await.map(axum::body::Body::new);
    if download {
        harden_download(&mut resp);
    }
    resp
}

/// 用户文件与网盘同源:浏览器直接打开 /dav 下的 HTML/SVG/脚本时不得在本站执行。
/// 一律按附件下载并沙箱化;主动内容改报二进制类型,配合 nosniff 也无法被 `<script>` 引用。
fn harden_download(resp: &mut Response) {
    let headers = resp.headers_mut();
    let active = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .is_some_and(|mime| !crate::api::inline_mime_allowed(&mime));
    if active {
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/octet-stream"),
        );
    }
    headers.insert(
        http::header::CONTENT_DISPOSITION,
        http::HeaderValue::from_static("attachment"),
    );
    headers.insert(
        http::header::CONTENT_SECURITY_POLICY,
        http::HeaderValue::from_static("sandbox; default-src 'none'"),
    );
}

fn app_to_fs(e: AppError) -> FsError {
    match e {
        AppError::NotFound => FsError::NotFound,
        AppError::AlreadyExists => FsError::Exists,
        AppError::IsDirectory | AppError::NotDirectory => FsError::Forbidden,
        AppError::InvalidPath(_) | AppError::BadRequest(_) => FsError::Forbidden,
        AppError::Unauthorized => FsError::Forbidden,
        _ => FsError::GeneralFailure,
    }
}

/// DavPath → 内部 "/a/b" 形式
fn path_str(p: &DavPath) -> String {
    format!("/{}", p.as_rel_ospath().to_string_lossy())
}

// ---------- 元数据 ----------

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    is_dir: bool,
    modified: i64,
    created: i64,
}

impl From<&Node> for Meta {
    fn from(n: &Node) -> Self {
        Meta {
            len: n.size as u64,
            is_dir: n.is_dir,
            modified: n.modified_at,
            created: n.created_at,
        }
    }
}

fn ts(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(ts(self.modified))
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn created(&self) -> FsResult<SystemTime> {
        Ok(ts(self.created))
    }
}

// ---------- 目录项 ----------

struct DirEntry {
    node: Node,
}

impl DavDirEntry for DirEntry {
    fn name(&self) -> Vec<u8> {
        self.node.name.clone().into_bytes()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = Meta::from(&self.node);
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }
    fn is_dir(&self) -> FsFuture<'_, bool> {
        let d = self.node.is_dir;
        Box::pin(async move { Ok(d) })
    }
}

// ---------- 文件系统 ----------

#[derive(Clone)]
struct TgDavFs {
    vfs: Vfs,
}

impl DavFileSystem for TgDavFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let p = path_str(path);
            let is_write =
                options.write || options.append || options.truncate || options.create_new;
            if is_write {
                if options.append {
                    return Err(FsError::NotImplemented); // 不支持追加写
                }
                let existing = self.vfs.resolve(&p).await;
                match &existing {
                    Ok(n) if n.is_dir => return Err(FsError::Forbidden),
                    Ok(_) if options.create_new => return Err(FsError::Exists),
                    Err(_) if !(options.create || options.create_new) => {
                        return Err(FsError::NotFound);
                    }
                    _ => {}
                }
                let (parent, name) = self.vfs.resolve_parent(&p).await.map_err(app_to_fs)?;
                let handle = self
                    .vfs
                    .start_upload(&parent, &name, None)
                    .await
                    .map_err(app_to_fs)?;
                Ok(Box::new(TgDavFile {
                    node: None,
                    reader: None,
                    writer: Some(handle),
                    write_pos: 0,
                }) as Box<dyn DavFile>)
            } else {
                let node = self.vfs.resolve(&p).await.map_err(app_to_fs)?;
                if node.is_dir {
                    return Err(FsError::Forbidden);
                }
                let reader = self.vfs.open_reader(&node, 0).await.map_err(app_to_fs)?;
                Ok(Box::new(TgDavFile {
                    node: Some(node),
                    reader: Some(tokio::sync::Mutex::new(reader)),
                    writer: None,
                    write_pos: 0,
                }) as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let children = self.vfs.list(&path_str(path)).await.map_err(app_to_fs)?;
            let items: Vec<FsResult<Box<dyn DavDirEntry>>> = children
                .into_iter()
                .map(|node| Ok(Box::new(DirEntry { node }) as Box<dyn DavDirEntry>))
                .collect();
            Ok(Box::pin(futures_util::stream::iter(items)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let node = self.vfs.resolve(&path_str(path)).await.map_err(app_to_fs)?;
            Ok(Box::new(Meta::from(&node)) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            // MKCOL 语义:父目录必须存在
            self.vfs.mkdir(&path_str(path)).await.map_err(app_to_fs)?;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = path_str(path);
            let node = self.vfs.resolve(&p).await.map_err(app_to_fs)?;
            if !node.is_dir {
                return Err(FsError::Forbidden);
            }
            self.vfs.delete(&p).await.map_err(app_to_fs)
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = path_str(path);
            let node = self.vfs.resolve(&p).await.map_err(app_to_fs)?;
            if node.is_dir {
                return Err(FsError::Forbidden);
            }
            self.vfs.delete(&p).await.map_err(app_to_fs)
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            // Overwrite:T 时 dav-server 已先删除目标
            self.vfs
                .rename(&path_str(from), &path_str(to))
                .await
                .map_err(app_to_fs)
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let to_s = path_str(to);
            // 文件级覆盖复制:目标若还存在则先删(dav-server 对 Depth:0 的场景不预删)
            if let Ok(existing) = self.vfs.resolve(&to_s).await
                && !existing.is_dir
            {
                self.vfs.delete(&to_s).await.map_err(app_to_fs)?;
            }
            self.vfs
                .copy(&path_str(from), &to_s)
                .await
                .map_err(app_to_fs)
        })
    }
}

// ---------- 文件句柄 ----------

struct TgDavFile {
    node: Option<Node>,
    /// Mutex 仅为满足 DavFile: Sync 约束;访问都经 &mut self + get_mut,无锁开销
    reader: Option<tokio::sync::Mutex<ChunkedReader>>,
    writer: Option<UploadHandle>,
    write_pos: u64,
}

impl std::fmt::Debug for TgDavFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TgDavFile")
            .field(
                "mode",
                &if self.writer.is_some() {
                    "write"
                } else {
                    "read"
                },
            )
            .finish()
    }
}

impl DavFile for TgDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let meta = match &self.node {
                Some(n) => Meta::from(n),
                None => Meta {
                    len: self.write_pos,
                    is_dir: false,
                    modified: crate::db::now_ts(),
                    created: crate::db::now_ts(),
                },
            };
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        })
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let w = self.writer.as_ref().ok_or(FsError::GeneralFailure)?;
            self.write_pos += buf.len() as u64;
            w.feed(buf).await.map_err(|e| {
                tracing::warn!("WebDAV 写入失败: {e}");
                FsError::GeneralFailure
            })
        })
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            while buf.has_remaining() {
                let chunk = Bytes::copy_from_slice(buf.chunk());
                let n = chunk.len();
                let w = self.writer.as_ref().ok_or(FsError::GeneralFailure)?;
                w.feed(chunk).await.map_err(|_| FsError::GeneralFailure)?;
                self.write_pos += n as u64;
                buf.advance(n);
            }
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let r = self
                .reader
                .as_mut()
                .ok_or(FsError::GeneralFailure)?
                .get_mut();
            let mut out = vec![0u8; count];
            let mut filled = 0;
            while filled < count {
                let n = r.read(&mut out[filled..]).await.map_err(|e| {
                    tracing::warn!("WebDAV 读取失败: {e}");
                    FsError::GeneralFailure
                })?;
                if n == 0 {
                    break; // EOF
                }
                filled += n;
            }
            out.truncate(filled);
            Ok(Bytes::from(out))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            if let Some(r) = self.reader.as_mut() {
                return r
                    .get_mut()
                    .seek(pos)
                    .await
                    .map_err(|_| FsError::GeneralFailure);
            }
            // 写模式:只接受停留在当前位置的 seek(不支持部分 PUT)
            match pos {
                SeekFrom::Start(p) if p == self.write_pos => Ok(p),
                SeekFrom::Current(0) => Ok(self.write_pos),
                _ => Err(FsError::NotImplemented),
            }
        })
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if let Some(w) = self.writer.take() {
                let node = w.finish().await.map_err(|e| {
                    tracing::warn!("WebDAV 上传收尾失败: {e}");
                    FsError::GeneralFailure
                })?;
                self.node = Some(node);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod harden_tests {
    use super::*;

    fn served(mime: &str) -> Response {
        let mut resp = Response::builder()
            .header(http::header::CONTENT_TYPE, mime)
            .body(axum::body::Body::empty())
            .unwrap();
        harden_download(&mut resp);
        resp
    }

    #[test]
    fn active_content_is_downloaded_as_binary() {
        for mime in [
            "text/html; charset=utf-8",
            "image/svg+xml",
            "application/javascript",
        ] {
            let resp = served(mime);
            assert_eq!(
                resp.headers()[http::header::CONTENT_TYPE],
                "application/octet-stream"
            );
            assert_eq!(
                resp.headers()[http::header::CONTENT_DISPOSITION],
                "attachment"
            );
        }
        let png = served("image/png");
        assert_eq!(png.headers()[http::header::CONTENT_TYPE], "image/png");
        assert_eq!(
            png.headers()[http::header::CONTENT_DISPOSITION],
            "attachment"
        );
        assert!(
            png.headers()
                .contains_key(http::header::CONTENT_SECURITY_POLICY)
        );
    }
}
