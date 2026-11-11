//! 收件类型的候选目录。路径仅用于展示，已绑定目录以节点身份定位。

use super::{Extras, InboxRouteRule};
use crate::db::{Db, ROOT_ID};
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_INBOX_DIRECTORIES: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxDirectory {
    #[serde(default)]
    pub node_id: Option<i64>,
    pub path: String,
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxTypeDirectories {
    pub key: String,
    pub directories: Vec<InboxDirectory>,
}

pub fn custom_key(name: &str) -> String {
    format!("custom:{}", name.trim().to_lowercase())
}

pub fn legacy_routes(ex: &Extras) -> Vec<InboxTypeDirectories> {
    let mut routes: Vec<_> = [
        ("default", ex.inbox_dir.as_str()),
        ("video", ex.inbox_video_dir.as_str()),
        ("image", ex.inbox_image_dir.as_str()),
        ("book", ex.inbox_book_dir.as_str()),
    ]
    .into_iter()
    .map(|(key, path)| single_route(key, path))
    .collect();
    routes.extend(
        ex.inbox_rules
            .iter()
            .map(|rule| single_route(&custom_key(&rule.name), &rule.dir)),
    );
    routes
}

pub fn single_route(key: &str, path: &str) -> InboxTypeDirectories {
    InboxTypeDirectories {
        key: key.into(),
        directories: vec![InboxDirectory {
            node_id: None,
            path: path.into(),
            automatic: true,
            missing: false,
        }],
    }
}

/// 旧目录字段代表自动目标，不能因旧客户端编辑而清空其他候选目录。
pub fn set_legacy_auto(routes: &mut Vec<InboxTypeDirectories>, key: &str, path: &str) {
    let Some(route) = routes.iter_mut().find(|route| route.key == key) else {
        routes.push(single_route(key, path));
        return;
    };
    if let Some(index) = route
        .directories
        .iter()
        .position(|directory| directory.path == path && !directory.missing)
    {
        for (current, directory) in route.directories.iter_mut().enumerate() {
            directory.automatic = current == index;
        }
    } else if let Some(index) = route
        .directories
        .iter()
        .position(|directory| directory.automatic)
    {
        route.directories[index] = InboxDirectory {
            node_id: None,
            path: path.into(),
            automatic: true,
            missing: false,
        };
    } else {
        *route = single_route(key, path);
    }
}

/// 新配置损坏时停用对应类型，不能退回旧路径并悄悄重新创建目录。
pub async fn hydrate(db: &Db, ex: &mut Extras, raw: Option<&str>) -> AppResult<()> {
    let legacy = legacy_routes(ex);
    ex.inbox_routes = if let Some(raw) = raw {
        let values = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Array(values)) => values,
            _ => {
                tracing::warn!("收件目录配置损坏，请在管理页重新配置");
                Vec::new()
            }
        };
        legacy
            .into_iter()
            .map(|old| {
                let matched: Vec<_> = values
                    .iter()
                    .filter(|value| value["key"].as_str() == Some(old.key.as_str()))
                    .collect();
                if matched.len() == 1 {
                    serde_json::from_value::<InboxTypeDirectories>(matched[0].clone()).unwrap_or(
                        InboxTypeDirectories {
                            key: old.key,
                            directories: Vec::new(),
                        },
                    )
                } else {
                    InboxTypeDirectories {
                        key: old.key,
                        directories: Vec::new(),
                    }
                }
            })
            .collect()
    } else {
        legacy
    };
    let ids: Vec<_> = ex
        .inbox_routes
        .iter()
        .flat_map(|route| &route.directories)
        .filter_map(|directory| directory.node_id)
        .collect();
    let paths = db.inbox_directory_paths(&ids).await?;
    for route in &mut ex.inbox_routes {
        for directory in &mut route.directories {
            if let Some(id) = directory.node_id {
                match paths.get(&id) {
                    Some(path) => {
                        directory.path.clone_from(path);
                        directory.missing = false;
                    }
                    None => directory.missing = true,
                }
            }
        }
    }
    sync_legacy(ex);
    Ok(())
}

pub fn sync_legacy(ex: &mut Extras) {
    for route in &ex.inbox_routes {
        let Some(directory) = route.directories.iter().find(|d| d.automatic) else {
            continue;
        };
        match route.key.as_str() {
            "default" => ex.inbox_dir.clone_from(&directory.path),
            "video" => ex.inbox_video_dir.clone_from(&directory.path),
            "image" => ex.inbox_image_dir.clone_from(&directory.path),
            "book" => ex.inbox_book_dir.clone_from(&directory.path),
            _ => {
                for rule in &mut ex.inbox_rules {
                    if custom_key(&rule.name) == route.key {
                        rule.dir.clone_from(&directory.path);
                    }
                }
            }
        }
    }
}

pub fn normalize_routes(
    routes: &[InboxTypeDirectories],
    rules: &[InboxRouteRule],
) -> AppResult<Vec<InboxTypeDirectories>> {
    let mut required: HashSet<String> = ["default", "video", "image", "book"]
        .into_iter()
        .map(str::to_string)
        .collect();
    required.extend(rules.iter().map(|rule| custom_key(&rule.name)));
    let mut result = Vec::with_capacity(routes.len());
    for route in routes {
        if !required.remove(&route.key) {
            return Err(AppError::BadRequest(format!(
                "未知或重复的收件类型：{}",
                route.key
            )));
        }
        if route.directories.is_empty() || route.directories.len() > MAX_INBOX_DIRECTORIES {
            return Err(AppError::BadRequest(format!(
                "每种收件类型需要 1～{MAX_INBOX_DIRECTORIES} 个目录"
            )));
        }
        if route.directories.iter().filter(|d| d.automatic).count() != 1 {
            return Err(AppError::BadRequest(
                "每种类型必须指定一个自动收件目录".into(),
            ));
        }
        let mut paths = HashSet::new();
        let mut ids = HashSet::new();
        let mut directories = Vec::new();
        for directory in &route.directories {
            let path = super::normalize_dir(&directory.path, "收件目录")?;
            if path.len() > 2048 {
                return Err(AppError::BadRequest(
                    "收件目录完整路径最多 2048 字节".into(),
                ));
            }
            if !paths.insert(path.clone())
                || directory
                    .node_id
                    .is_some_and(|id| id <= ROOT_ID || !ids.insert(id))
            {
                return Err(AppError::BadRequest(
                    "同一类型的目录不能重复，且不能使用根目录".into(),
                ));
            }
            directories.push(InboxDirectory {
                path,
                node_id: directory.node_id,
                automatic: directory.automatic,
                missing: false,
            });
        }
        result.push(InboxTypeDirectories {
            key: route.key.clone(),
            directories,
        });
    }
    if !required.is_empty() {
        return Err(AppError::BadRequest("缺少收件类型的目录配置".into()));
    }
    Ok(result)
}

async fn existing_directory(db: &Db, path: &str) -> AppResult<Option<i64>> {
    let mut current = ROOT_ID;
    for segment in crate::vfs::normalize(path)? {
        match db.child(current, &segment).await? {
            Some(node) if node.is_dir => current = node.id,
            Some(_) => return Err(AppError::BadRequest(format!("收件路径被文件占用：{path}"))),
            None => {
                if db.uploading_child_exists(current, &segment).await? {
                    return Err(AppError::BadRequest(format!("收件路径正在上传：{path}")));
                }
                return Ok(None);
            }
        }
    }
    Ok(Some(current))
}

/// 全部路径与身份先校验，再创建旧配置中尚未创建的目录。
pub async fn bind_routes(db: &Db, routes: &mut [InboxTypeDirectories]) -> AppResult<()> {
    for route in routes.iter_mut() {
        for directory in &mut route.directories {
            if let Some(id) = directory.node_id {
                directory.path =
                    db.inbox_directory_path(id)
                        .await
                        .map_err(|error| match error {
                            AppError::NotFound | AppError::NotDirectory => AppError::BadRequest(
                                format!("收件目录已失效，请重新选择：{}", directory.path),
                            ),
                            other => other,
                        })?;
                if directory.path.len() > 2048 {
                    return Err(AppError::BadRequest(
                        "收件目录完整路径最多 2048 字节".into(),
                    ));
                }
            } else {
                directory.node_id = existing_directory(db, &directory.path).await?;
            }
        }
    }
    for route in routes {
        let mut seen = HashSet::new();
        for directory in &mut route.directories {
            if directory.node_id.is_none() {
                let mut current = ROOT_ID;
                for segment in crate::vfs::normalize(&directory.path)? {
                    let node = match db.child(current, &segment).await? {
                        Some(node) if node.is_dir => node,
                        Some(_) => return Err(AppError::NotDirectory),
                        None => match db.mkdir(current, &segment).await {
                            Ok(node) => node,
                            Err(AppError::AlreadyExists) => db
                                .child(current, &segment)
                                .await?
                                .ok_or(AppError::NotFound)?,
                            Err(error) => return Err(error),
                        },
                    };
                    if !node.is_dir {
                        return Err(AppError::NotDirectory);
                    }
                    current = node.id;
                }
                directory.node_id = Some(current);
            }
            if !seen.insert(directory.node_id) {
                return Err(AppError::BadRequest("同一类型的目录不能重复".into()));
            }
            directory.missing = false;
        }
    }
    Ok(())
}

/// 仅首次升级绑定旧路径；版本检查避免覆盖同时保存的新配置。
pub async fn ensure_bound(db: &Db) -> AppResult<Extras> {
    for _ in 0..3 {
        let mut ex = super::load(db).await?;
        if !ex
            .inbox_routes
            .iter()
            .flat_map(|route| &route.directories)
            .any(|d| d.node_id.is_none())
        {
            return Ok(ex);
        }
        let mut routes = normalize_routes(&ex.inbox_routes, &ex.inbox_rules)?;
        bind_routes(db, &mut routes).await?;
        ex.inbox_routes = routes;
        sync_legacy(&mut ex);
        let raw = serde_json::to_string(&ex.inbox_routes).map_err(anyhow::Error::from)?;
        if db
            .install_inbox_routes(ex.inbox_routes_revision, &raw)
            .await?
        {
            return super::load(db).await;
        }
    }
    Err(AppError::Conflict("收件配置正在变化，请稍后重试".into()))
}

impl Extras {
    pub fn inbox_directories_for(&self, name: &str) -> &[InboxDirectory] {
        let key = crate::inbox::inbox_type_key(self, name);
        self.inbox_routes
            .iter()
            .find(|route| route.key == key)
            .map(|route| route.directories.as_slice())
            .unwrap_or(&[])
    }
}
