//! VFS 门面:路径规范化 + 汇聚 db/engine/telegram 的业务操作,REST 与 WebDAV 共用

use crate::db::{Db, Node, ROOT_ID, StoredMessage};
use crate::engine::reader::ChunkedReader;
use crate::engine::upload::{UploadHandle, Uploader};
use crate::error::{AppError, AppResult};
use crate::telegram::TgRegistry;
use bytes::Bytes;
use std::sync::Arc;

/// 把用户路径拆成规范段:拒绝空名、`.`、`..`、控制字符与过长名称
pub fn normalize(path: &str) -> AppResult<Vec<String>> {
    let mut out = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(AppError::InvalidPath("不允许 ..".into()));
        }
        if seg.chars().any(|c| c.is_control()) {
            return Err(AppError::InvalidPath("包含控制字符".into()));
        }
        if seg.len() > 255 {
            return Err(AppError::InvalidPath("名称过长".into()));
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

/// 规范化为绝对路径：`a//b/./c` → `/a/b/c`，根目录为 `/`
pub fn canonical(path: &str) -> AppResult<String> {
    Ok(format!("/{}", normalize(path)?.join("/")))
}

/// 规范化路径所在的目录：`/a/b/c` → `/a/b`，根下直接子项与根本身 → `/`
pub fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(idx) => path[..idx].to_string(),
    }
}

#[derive(Clone)]
pub struct Vfs {
    pub db: Db,
    pub tg: Arc<TgRegistry>,
    pub uploader: Uploader,
}

impl Vfs {
    pub async fn resolve(&self, path: &str) -> AppResult<Node> {
        let segs = normalize(path)?;
        let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
        self.db.resolve(&refs).await?.ok_or(AppError::NotFound)
    }

    /// 解析出父目录节点与最后一段名称(用于创建/移动目标)
    pub async fn resolve_parent(&self, path: &str) -> AppResult<(Node, String)> {
        let segs = normalize(path)?;
        let name = segs
            .last()
            .ok_or(AppError::InvalidPath("路径不能为根".into()))?
            .clone();
        let parent_refs: Vec<&str> = segs[..segs.len() - 1].iter().map(String::as_str).collect();
        let parent = self
            .db
            .resolve(&parent_refs)
            .await?
            .ok_or(AppError::NotFound)?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        Ok((parent, name))
    }

    pub async fn list(&self, path: &str) -> AppResult<Vec<Node>> {
        let node = self.resolve(path).await?;
        if !node.is_dir {
            return Err(AppError::NotDirectory);
        }
        self.db.list_children(node.id).await
    }

    /// 递归创建目录(REST 用)
    pub async fn mkdir_p(&self, path: &str) -> AppResult<Node> {
        let segs = normalize(path)?;
        let mut cur = self.db.node(ROOT_ID).await?;
        for seg in &segs {
            cur = match self.db.child(cur.id, seg).await? {
                Some(n) if n.is_dir => n,
                Some(_) => return Err(AppError::NotDirectory),
                None => self.db.mkdir(cur.id, seg).await?,
            };
        }
        Ok(cur)
    }

    /// 非递归创建(WebDAV MKCOL 语义:父目录必须已存在)
    pub async fn mkdir(&self, path: &str) -> AppResult<Node> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.db.mkdir(parent.id, &name).await
    }

    /// 删除 = 进回收站(软删除);TG 消息等彻底删除/超期清理时才释放。
    /// REST 与 WebDAV 共用,rclone 误删同样可以恢复。
    pub async fn delete(&self, path: &str) -> AppResult<()> {
        let node = self.resolve(path).await?;
        let canon = format!("/{}", normalize(path)?.join("/"));
        self.db.trash_node(node.id, &canon).await
    }

    /// 永久删除文件或目录树；仅供用户明确选择“直接删除”时调用。
    /// 返回引用归零且已进入持久化删除队列的 Telegram 消息。
    pub async fn delete_permanently(&self, path: &str) -> AppResult<Vec<StoredMessage>> {
        let node = self.resolve(path).await?;
        self.db.delete_node(node.id).await
    }

    pub async fn rename(&self, from: &str, to: &str) -> AppResult<()> {
        let node = self.resolve(from).await?;
        let (new_parent, new_name) = self.resolve_parent(to).await?;
        self.db.move_node(node.id, new_parent.id, &new_name).await
    }

    /// 复制文件或整棵目录(元数据级,零流量)
    pub async fn copy(&self, from: &str, to: &str) -> AppResult<()> {
        let src = self.resolve(from).await?;
        let (dst_parent, dst_name) = self.resolve_parent(to).await?;
        if src.is_dir {
            if self.db.is_ancestor(src.id, dst_parent.id).await? {
                return Err(AppError::BadRequest("不能把目录复制到自己内部".into()));
            }
            self.copy_dir(src.id, dst_parent.id, &dst_name).await
        } else {
            self.db.copy_file(src.id, dst_parent.id, &dst_name).await?;
            Ok(())
        }
    }

    fn copy_dir<'a>(
        &'a self,
        src_id: i64,
        dst_parent: i64,
        dst_name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let new_dir = self.db.mkdir(dst_parent, dst_name).await?;
            for child in self.db.list_children(src_id).await? {
                if child.is_dir {
                    self.copy_dir(child.id, new_dir.id, &child.name).await?;
                } else {
                    self.db.copy_file(child.id, new_dir.id, &child.name).await?;
                }
            }
            Ok(())
        })
    }

    /// 打开文件读取流,start 为文件内起始偏移
    pub async fn open_reader(&self, node: &Node, start: u64) -> AppResult<ChunkedReader> {
        if node.is_dir {
            return Err(AppError::IsDirectory);
        }
        let chunks = self.db.chunks_of(node.id).await?;
        Ok(ChunkedReader::new(self.tg.clone(), &chunks, start))
    }

    /// 启动上传:目标为 dir_path 下的 name(自动递归建目录由调用方决定)
    pub async fn start_upload(
        &self,
        parent: &Node,
        name: &str,
        thumbnail: Option<Bytes>,
    ) -> AppResult<UploadHandle> {
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        let display = format!(
            "{}/{}",
            if parent.id == ROOT_ID {
                "".into()
            } else {
                self.node_path(parent).await?
            },
            name
        );
        self.uploader
            .start(parent.id, name, &display, thumbnail)
            .await
    }

    /// 由节点回溯完整路径，用于上传失败日志等展示。
    async fn node_path(&self, node: &Node) -> AppResult<String> {
        let mut parts = vec![node.name.clone()];
        let mut cur = node.parent_id;
        while let Some(id) = cur {
            let n = self.db.node(id).await?;
            if n.id == ROOT_ID {
                break;
            }
            parts.push(n.name.clone());
            cur = n.parent_id;
        }
        parts.reverse();
        Ok(format!("/{}", parts.join("/")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("/a/b/c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(normalize("a//b/").unwrap(), vec!["a", "b"]);
        assert_eq!(normalize("/").unwrap(), Vec::<String>::new());
        assert_eq!(normalize("./a/./b").unwrap(), vec!["a", "b"]);
        assert!(normalize("/a/../b").is_err());
        assert!(normalize("a\u{0}b").is_err());
    }
}
