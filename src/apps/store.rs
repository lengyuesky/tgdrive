//! 安装记录与最新目录身份授权；任何网盘访问都使用宿主自己的祖先链。

use super::{PlatformError, PlatformResult, now, opaque};
use crate::db::Db;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use std::collections::{BTreeMap, BTreeSet};
use tgdrive_contract::files::*;
use tgdrive_contract::manifest::Manifest;
use tgdrive_contract::runtime::*;
use tgdrive_contract::validation::{
    effective_access, valid_drive_path, valid_node_id, validate_scope,
};
use tgdrive_contract::{ByteCount, Timestamp, error_code};

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Installation {
    pub id: String,
    pub manifest: String,
    pub generation: String,
    pub state_revision: String,
    pub enabled: bool,
    pub pinned: bool,
    pub scope: String,
    pub scope_revision: String,
    pub target: String,
    pub package_sha256: String,
    pub package_size: i64,
    pub installed_at: i64,
    pub updated_at: i64,
}
impl Installation {
    pub fn manifest(&self) -> PlatformResult<Manifest> {
        Ok(serde_json::from_str(&self.manifest)?)
    }
    pub fn directory_scope(&self) -> PlatformResult<DirectoryScope> {
        Ok(serde_json::from_str(&self.scope)?)
    }
    pub fn identity(&self) -> PlatformResult<AppIdentity> {
        let m = self.manifest()?;
        Ok(AppIdentity {
            id: self.id.clone(),
            name: m.name,
            version: m.version,
            generation: self.generation.clone(),
        })
    }
    pub fn check(&self, generation: &str) -> PlatformResult<()> {
        if !self.enabled {
            return Err(PlatformError::new(error_code::APP_DISABLED, "应用已停用"));
        }
        if self.generation != generation {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "应用安装已改变，请重新打开",
            ));
        }
        Ok(())
    }
    pub fn backend(&self) -> PlatformResult<Option<BackendStatus>> {
        Ok(self.manifest()?.backend.map(|backend| BackendStatus {
            state: BackendState::Stopped,
            run_mode: backend.run_mode,
            run_id: None,
            error_code: Some(error_code::CAPABILITY_UNAVAILABLE.into()),
        }))
    }
}

pub(crate) async fn installation(
    connection: &mut SqliteConnection,
    id: &str,
) -> PlatformResult<Installation> {
    sqlx::query_as("SELECT * FROM app_installations WHERE id=?")
        .bind(id)
        .fetch_optional(connection)
        .await?
        .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "应用未安装"))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct NodeRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub created_at: i64,
    pub modified_at: i64,
    pub seed: String,
    pub content_version: Option<String>,
    pub children_revision: Option<String>,
    pub has_thumbnail: bool,
}
pub(crate) const NODE_COLUMNS: &str = "n.id,n.parent_id,n.name,n.is_dir,n.size,n.created_at,n.modified_at,m.revision AS seed,m.content_version,m.children_revision,EXISTS(SELECT 1 FROM chunks c WHERE c.node_id=n.id AND c.idx=0 AND c.tg_thumbnail_file_id IS NOT NULL) AS has_thumbnail";

pub(crate) fn node_id(value: &str) -> PlatformResult<i64> {
    if !valid_node_id(value) {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "节点 ID 无效",
        ));
    }
    value
        .parse()
        .map_err(|_| PlatformError::new(error_code::INVALID_REQUEST, "节点 ID 无效"))
}

pub(crate) async fn node_row(
    connection: &mut SqliteConnection,
    id: i64,
) -> PlatformResult<NodeRow> {
    let sql = format!(
        "SELECT {NODE_COLUMNS} FROM nodes n JOIN app_node_meta m ON m.node_id=n.id WHERE n.id=? AND n.status='ready' AND NOT EXISTS(SELECT 1 FROM app_ancestors a JOIN nodes p ON p.id=a.ancestor_id WHERE a.node_id=n.id AND p.status<>'ready')"
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(connection)
        .await?
        .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "节点不存在或不可见"))
}

pub(crate) async fn access(
    connection: &mut SqliteConnection,
    app: &Installation,
    id: i64,
) -> PlatformResult<AccessLevel> {
    let ancestors:Vec<(i64,)>=sqlx::query_as("SELECT a.ancestor_id FROM app_ancestors a JOIN nodes n ON n.id=a.ancestor_id WHERE a.node_id=? AND n.status='ready'").bind(id).fetch_all(connection).await?;
    let ancestors = ancestors
        .into_iter()
        .map(|(id,)| id.to_string())
        .collect::<Vec<_>>();
    let references = ancestors.iter().map(String::as_str).collect::<Vec<_>>();
    effective_access(&app.directory_scope()?, &app.manifest()?.files, &references)
        .ok_or_else(|| PlatformError::new(error_code::SCOPE_DENIED, "应用没有该目录的读取授权"))
}

/// 批量获取结果行的祖先路径；只读少量当前页元数据，不把整树装进内存。
pub(crate) async fn file_nodes(
    connection: &mut SqliteConnection,
    rows: Vec<NodeRow>,
) -> PlatformResult<Vec<FileNode>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT a.node_id,n.id,n.name,m.revision FROM app_ancestors a JOIN nodes n ON n.id=a.ancestor_id JOIN app_node_meta m ON m.node_id=n.id WHERE a.node_id IN(",
    );
    let mut separated = query.separated(",");
    for id in ids {
        separated.push_bind(id);
    }
    separated.push_unseparated(") ORDER BY a.node_id,a.depth DESC");
    let ancestors: Vec<(i64, i64, String, String)> =
        query.build_query_as().fetch_all(&mut *connection).await?;
    let mut paths = BTreeMap::<i64, (String, Sha256)>::new();
    for (node, ancestor, name, revision) in ancestors {
        let (path, hash) = paths
            .entry(node)
            .or_insert_with(|| (String::new(), Sha256::new()));
        if ancestor != crate::db::ROOT_ID {
            path.push('/');
            path.push_str(&name);
        }
        // 节点路径只依赖祖先身份与名称，不因兄弟上传导致整棵子树的修订跳动。
        hash.update(ancestor.to_le_bytes());
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        if ancestor == node {
            hash.update(revision.as_bytes());
        }
    }
    rows.into_iter()
        .map(|row| {
            let (mut path, path_hash) = paths
                .remove(&row.id)
                .ok_or_else(|| PlatformError::new(error_code::NOT_FOUND, "节点祖先链不完整"))?;
            if path.is_empty() {
                path = "/".into();
            }
            if !valid_drive_path(&path) {
                return Err(PlatformError::new(
                    error_code::INVALID_PATH,
                    "节点路径不符合契约边界",
                ));
            }
            let signature = crate::engine::upload::to_hex(&path_hash.finalize());
            let revision =
                crate::engine::upload::to_hex(&Sha256::digest(format!("{}:{signature}", row.seed)));
            let children_revision = row.children_revision.map(|seed| {
                crate::engine::upload::to_hex(&Sha256::digest(format!("{seed}:{signature}")))
            });
            Ok(FileNode {
                id: row.id.to_string(),
                parent_id: row.parent_id.map(|id| id.to_string()),
                name: row.name,
                path,
                kind: if row.is_dir {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                size: ByteCount(if row.is_dir {
                    0
                } else {
                    row.size.max(0) as u64
                }),
                created_at: Timestamp(
                    row.created_at
                        .saturating_mul(1000)
                        .clamp(0, tgdrive_contract::http::MAX_TIMESTAMP_MILLIS as i64),
                ),
                modified_at: Timestamp(
                    row.modified_at
                        .saturating_mul(1000)
                        .clamp(0, tgdrive_contract::http::MAX_TIMESTAMP_MILLIS as i64),
                ),
                revision,
                content_version: row.content_version,
                children_revision,
                has_thumbnail: !row.is_dir && row.has_thumbnail,
            })
        })
        .collect()
}

async fn reference_row(
    connection: &mut SqliteConnection,
    reference: &NodeReference,
) -> PlatformResult<NodeRow> {
    let id = match reference {
        NodeReference::Id { id, .. } => node_id(id)?,
        NodeReference::Path { path, .. } => {
            if !valid_drive_path(path) {
                return Err(PlatformError::new(
                    error_code::INVALID_PATH,
                    "网盘路径不是规范化绝对路径",
                ));
            }
            let mut id = crate::db::ROOT_ID;
            if path != "/" {
                for segment in path[1..].split('/') {
                    let child: Option<(i64,)> = sqlx::query_as(
                        "SELECT id FROM nodes WHERE parent_id=? AND name=? AND status='ready'",
                    )
                    .bind(id)
                    .bind(segment)
                    .fetch_optional(&mut *connection)
                    .await?;
                    id = child
                        .ok_or_else(|| {
                            PlatformError::new(error_code::NOT_FOUND, "节点不存在或不可见")
                        })?
                        .0;
                }
            }
            id
        }
    };
    node_row(connection, id).await
}
fn check_content(row: &NodeRow, reference: &NodeReference) -> PlatformResult<()> {
    let version = match reference {
        NodeReference::Id {
            content_version, ..
        }
        | NodeReference::Path {
            content_version, ..
        } => content_version,
    };
    if version
        .as_ref()
        .is_some_and(|version| row.is_dir || row.content_version.as_ref() != Some(version))
    {
        return Err(PlatformError::new(
            error_code::CONTENT_CHANGED,
            "文件内容版本已改变",
        ));
    }
    Ok(())
}
pub(crate) async fn owner_reference(
    connection: &mut SqliteConnection,
    reference: &NodeReference,
) -> PlatformResult<FileNode> {
    let row = reference_row(connection, reference).await?;
    check_content(&row, reference)?;
    Ok(file_nodes(connection, vec![row]).await?.remove(0))
}
pub(crate) async fn reference(
    connection: &mut SqliteConnection,
    app: &Installation,
    reference: &NodeReference,
) -> PlatformResult<FileNode> {
    let row = reference_row(connection, reference).await?;
    access(connection, app, row.id).await?;
    check_content(&row, reference)?;
    Ok(file_nodes(connection, vec![row]).await?.remove(0))
}

pub(crate) async fn scope_info(
    connection: &mut SqliteConnection,
    app: &Installation,
) -> PlatformResult<ScopeInfo> {
    let scope = app.directory_scope()?;
    let grants = match &scope {
        DirectoryScope::None => Vec::new(),
        DirectoryScope::All { access } => vec![DirectoryGrant {
            directory_id: crate::db::ROOT_ID.to_string(),
            access: *access,
        }],
        DirectoryScope::Selected { directories } => directories.clone(),
    };
    let mut roots = Vec::with_capacity(grants.len());
    for grant in grants {
        let row = match node_row(connection, node_id(&grant.directory_id)?).await {
            Ok(row) if row.is_dir => Some(row),
            Ok(_) => None,
            Err(error) if error.code == error_code::NOT_FOUND => None,
            Err(error) => return Err(error),
        };
        let node = if let Some(row) = row {
            file_nodes(connection, vec![row]).await?.pop()
        } else {
            None
        };
        roots.push(AuthorizedRoot {
            directory_id: grant.directory_id,
            access: grant.access,
            node,
        });
    }
    Ok(ScopeInfo {
        revision: app.scope_revision.clone(),
        scope,
        roots,
    })
}

pub(crate) async fn installed_app(
    connection: &mut SqliteConnection,
    app: Installation,
) -> PlatformResult<InstalledApp> {
    let target = serde_json::from_value(serde_json::Value::String(app.target.clone()))?;
    Ok(InstalledApp {
        manifest: app.manifest()?,
        generation: app.generation.clone(),
        state_revision: app.state_revision.clone(),
        enabled: app.enabled,
        pinned: app.pinned,
        scope: scope_info(connection, &app).await?,
        package: InstalledPackage {
            target,
            sha256: app.package_sha256.clone(),
            size: ByteCount(app.package_size as u64),
        },
        backend: app.backend()?,
        installed_at: Timestamp(app.installed_at),
        updated_at: Timestamp(app.updated_at),
    })
}

pub(crate) async fn update_scope(
    db: &Db,
    id: &str,
    request: ScopeUpdateRequest,
) -> PlatformResult<ScopeInfo> {
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let app = installation(&mut tx, id).await?;
    if app.scope_revision != request.expected_revision {
        return Err(PlatformError::new(
            error_code::SCOPE_CHANGED,
            "目录授权已改变，请重新读取",
        ));
    }
    validate_scope(&request.scope, &app.manifest()?.files)?;
    if let DirectoryScope::Selected { directories } = &request.scope {
        for grant in directories {
            let node = node_row(&mut tx, node_id(&grant.directory_id)?).await?;
            if !node.is_dir {
                return Err(PlatformError::new(
                    error_code::NOT_DIRECTORY,
                    "授权目标不是目录",
                ));
            }
        }
    }
    let revision = opaque();
    sqlx::query("UPDATE app_installations SET scope=?,scope_revision=?,updated_at=? WHERE id=?")
        .bind(serde_json::to_string(&request.scope)?)
        .bind(&revision)
        .bind(now().0)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let updated = installation(&mut tx, id).await?;
    let result = scope_info(&mut tx, &updated).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn query_roots(
    connection: &mut SqliteConnection,
    app: &Installation,
    requested: &[String],
) -> PlatformResult<Vec<i64>> {
    let manifest = app.manifest()?;
    if !manifest.files.read {
        return Err(PlatformError::new(
            error_code::SCOPE_DENIED,
            "应用未声明文件读取",
        ));
    }
    let ids = if requested.is_empty() {
        match app.directory_scope()? {
            DirectoryScope::None => {
                return Err(PlatformError::new(
                    error_code::SCOPE_DENIED,
                    "应用尚未获得目录授权",
                ));
            }
            DirectoryScope::All { .. } => vec![crate::db::ROOT_ID],
            DirectoryScope::Selected { directories } => directories
                .into_iter()
                .map(|g| node_id(&g.directory_id))
                .collect::<PlatformResult<Vec<_>>>()?,
        }
    } else {
        requested
            .iter()
            .map(|id| node_id(id))
            .collect::<PlatformResult<Vec<_>>>()?
    };
    let mut roots = BTreeSet::new();
    for id in ids {
        match node_row(connection, id).await {
            Ok(row) => {
                if !row.is_dir {
                    return Err(PlatformError::new(
                        error_code::NOT_DIRECTORY,
                        "查询根不是目录",
                    ));
                }
                access(connection, app, id).await?;
                roots.insert(id);
            }
            Err(error) if error.code == error_code::NOT_FOUND && requested.is_empty() => {}
            Err(error) => return Err(error),
        }
    }
    // 按身份祖先去重；重叠授权取并集，不用路径前缀匹配。
    let candidates = roots.iter().copied().collect::<Vec<_>>();
    for id in candidates {
        let ancestors: Vec<(i64,)> =
            sqlx::query_as("SELECT ancestor_id FROM app_ancestors WHERE node_id=? AND depth>0")
                .bind(id)
                .fetch_all(&mut *connection)
                .await?;
        if ancestors.iter().any(|(ancestor,)| roots.contains(ancestor)) {
            roots.remove(&id);
        }
    }
    Ok(roots.into_iter().collect())
}
