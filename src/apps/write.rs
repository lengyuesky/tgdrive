//! 条件元数据事务；目录和回收原语与网盘本体共用，回执与改动原子持久化。
use super::runtime::CallerSession;
use super::store::{self, Installation};
use super::{AppPlatform, PlatformError, PlatformResult, now};
use crate::db::Db;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tgdrive_contract::{error_code, files::*, http::*, validation::*};

pub(crate) fn caller_live(platform: &AppPlatform, caller: &CallerSession) -> PlatformResult<()> {
    if caller.cancelled.is_cancelled()
        || caller.expires_at.load(std::sync::atomic::Ordering::Acquire) <= now().0
        || (caller.is_page() && caller.owner_epoch != platform.host.auth.device_epoch())
    {
        return Err(PlatformError::new(
            error_code::SESSION_EXPIRED,
            "写入调用方已失效",
        ));
    }
    if platform.host.restore.maintenance()
        || platform.host.vfs.db.background_blocked()
        || platform.host.restore.shutdown.is_cancelled()
    {
        return Err(PlatformError::new(
            error_code::HOST_MAINTENANCE,
            "宿主正在维护",
        ));
    }
    Ok(())
}
pub(crate) fn declared(app: &Installation) -> PlatformResult<()> {
    if !app.manifest()?.files.write {
        return Err(PlatformError::new(
            error_code::WRITE_NOT_DECLARED,
            "应用未声明文件写入",
        ));
    }
    Ok(())
}
pub(crate) async fn write_access(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    id: i64,
) -> PlatformResult<()> {
    declared(app)?;
    if store::access(connection, app, id).await? != AccessLevel::ReadWrite {
        return Err(PlatformError::new(
            error_code::SCOPE_DENIED,
            "相关节点或父目录没有读写授权",
        ));
    }
    Ok(())
}
pub(crate) fn storage_error(error: crate::error::AppError) -> PlatformError {
    use crate::error::AppError;
    let code = match error {
        AppError::AlreadyExists => error_code::NAME_CONFLICT,
        AppError::NotFound => error_code::NOT_FOUND,
        AppError::NotDirectory => error_code::NOT_DIRECTORY,
        AppError::IsDirectory => error_code::IS_DIRECTORY,
        AppError::BadRequest(_) => error_code::INVALID_OPERATION,
        AppError::Telegram(_) | AppError::TelegramRejected(_) | AppError::TelegramUncertain(_) => {
            error_code::UPSTREAM_UNAVAILABLE
        }
        AppError::Conflict(_) => error_code::UPLOAD_STATE,
        _ => error_code::INTERNAL_ERROR,
    };
    PlatformError::new(code, "通用文件操作未能完成")
}
fn invalid() -> PlatformError {
    PlatformError::new(
        error_code::INVALID_OPERATION,
        "操作引用、树结构或同批次修改冲突",
    )
}
pub(crate) fn idempotency_key(key: &str) -> PlatformResult<()> {
    if !valid_idempotency_key(key) {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "幂等键无效",
        ));
    }
    Ok(())
}
pub(crate) async fn existing_request(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    key: &str,
    kind: &str,
    request: &str,
) -> PlatformResult<Option<String>> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as("SELECT generation,kind,request,receipt,expires_at FROM app_write_requests WHERE app_id=? AND key=?")
        .bind(&app.id).bind(key).fetch_optional(&mut *connection).await?;
    if let Some((generation, previous_kind, previous, receipt, expiry)) = row {
        if expiry <= now().0 {
            sqlx::query("DELETE FROM app_write_requests WHERE app_id=? AND key=?")
                .bind(&app.id)
                .bind(key)
                .execute(connection)
                .await?;
            return Ok(None);
        }
        if generation != app.generation {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "幂等键属于旧安装轮次",
            ));
        }
        if previous_kind != kind || previous != request {
            return Err(PlatformError::new(
                error_code::IDEMPOTENCY_CONFLICT,
                "幂等键已绑定不同请求",
            ));
        }
        return Ok(Some(receipt));
    }
    Ok(None)
}
pub(crate) async fn bind_request(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    key: &str,
    kind: &str,
    request: &str,
    receipt: &str,
    expiry: i64,
) -> PlatformResult<()> {
    sqlx::query("INSERT INTO app_write_requests(app_id,key,generation,kind,request,receipt,expires_at) VALUES(?,?,?,?,?,?,?)")
        .bind(&app.id).bind(key).bind(&app.generation).bind(kind).bind(request).bind(receipt).bind(expiry).execute(connection).await?;
    Ok(())
}
async fn reference(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    id: &str,
) -> PlatformResult<FileNode> {
    store::reference(
        connection,
        app,
        &NodeReference::Id {
            id: id.into(),
            content_version: None,
        },
    )
    .await
}
fn condition(node: &NodeCondition) -> PlatformResult<()> {
    if !valid_node_id(&node.id) || !valid_opaque_id(&node.revision) {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "节点条件无效",
        ));
    }
    Ok(())
}
async fn assert_node(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    node: &NodeCondition,
) -> PlatformResult<FileNode> {
    condition(node)?;
    let current = reference(connection, app, &node.id).await?;
    if current.revision != node.revision {
        return Err(PlatformError::new(
            error_code::NODE_CHANGED,
            "节点条件已改变",
        ));
    }
    Ok(current)
}
async fn directory(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    target: &DirectoryTarget,
    existing: &BTreeSet<String>,
    created: &BTreeMap<String, String>,
) -> PlatformResult<FileNode> {
    let id = match target {
        DirectoryTarget::Existing { id } => {
            if !existing.contains(id) {
                return Err(invalid());
            }
            id
        }
        DirectoryTarget::Created { operation_id } => {
            created.get(operation_id).ok_or_else(invalid)?
        }
    };
    let node = reference(connection, app, id)
        .await
        .map_err(|_| invalid())?;
    if node.kind != NodeKind::Directory {
        return Err(PlatformError::new(
            error_code::NOT_DIRECTORY,
            "目标父节点不是目录",
        ));
    }
    write_access(connection, app, store::node_id(id)?).await?;
    Ok(node)
}
async fn path_fits(
    connection: &mut sqlx::SqliteConnection,
    node: Option<&FileNode>,
    parent: &FileNode,
    name: &str,
) -> PlatformResult<()> {
    if !valid_drive_name(name) {
        return Err(PlatformError::new(
            error_code::INVALID_REQUEST,
            "节点名称无效",
        ));
    }
    let base = if parent.path == "/" {
        0
    } else {
        parent.path.len()
    } + 1
        + name.len();
    let suffix: i64 = if let Some(node) = node.filter(|node| node.kind == NodeKind::Directory) {
        sqlx::query_scalar("SELECT COALESCE(max(bytes),0) FROM (SELECT sum(length(CAST(n.name AS BLOB))+1) AS bytes FROM app_ancestors sub JOIN app_ancestors a ON a.node_id=sub.node_id JOIN nodes n ON n.id=a.ancestor_id JOIN app_ancestors below ON below.node_id=a.ancestor_id AND below.ancestor_id=? WHERE sub.ancestor_id=? AND a.ancestor_id<>? GROUP BY sub.node_id)")
            .bind(store::node_id(&node.id)?).bind(store::node_id(&node.id)?).bind(store::node_id(&node.id)?).fetch_one(connection).await?
    } else {
        0
    };
    if base as u64 + suffix.max(0) as u64 > MAX_DRIVE_PATH_BYTES {
        return Err(PlatformError::new(
            error_code::INVALID_PATH,
            "操作后的子树路径超过上限",
        ));
    }
    Ok(())
}

impl AppPlatform {
    pub async fn mutation_receipt(
        &self,
        caller: &CallerSession,
        key: &str,
    ) -> PlatformResult<MutationReceipt> {
        idempotency_key(key)?;
        let row: Option<String> = sqlx::query_scalar("SELECT receipt FROM app_write_requests WHERE app_id=? AND key=? AND kind='mutation' AND expires_at>?").bind(&caller.app_id).bind(key).bind(now().0).fetch_optional(&self.host.vfs.db.pool).await?;
        Ok(serde_json::from_str(&row.ok_or_else(|| {
            PlatformError::new(error_code::NOT_FOUND, "没有已提交回执")
        })?)?)
    }
    pub async fn mutate(
        self: &Arc<Self>,
        caller: &Arc<CallerSession>,
        key: &str,
        request: FileMutationRequest,
    ) -> PlatformResult<MutationReceipt> {
        idempotency_key(key)?;
        if request.operations.is_empty()
            || request.operations.len() as u64 > MAX_MUTATION_OPERATIONS
            || request.assertions.len() as u64 > MAX_MUTATION_ASSERTIONS
            || !valid_opaque_id(&request.scope_revision)
        {
            return Err(PlatformError::new(
                error_code::INVALID_REQUEST,
                "事务规模或授权修订无效",
            ));
        }
        caller_live(self, caller)?;
        let canonical = serde_json::to_string(&request)?;
        let mut tx = self.host.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
        let app = store::installation(&mut tx, &caller.app_id).await?;
        app.check(&caller.generation)?;
        if let Some(receipt) = existing_request(&mut tx, &app, key, "mutation", &canonical).await? {
            return Ok(serde_json::from_str(&receipt)?);
        }
        declared(&app)?;
        if app.scope_revision != request.scope_revision {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "目录授权修订已改变",
            ));
        }
        let mut operation_ids = BTreeSet::new();
        let mut changed = BTreeSet::new();
        let mut parents = BTreeSet::new();
        let mut upload_ids = BTreeSet::new();
        let mut uploads = BTreeMap::new();
        // 断言和所有既有节点条件都在开始快照核对，不因本批次自己先移动祖先而失效。
        for assertion in &request.assertions {
            match assertion {
                FileAssertion::Node { id, revision } => {
                    assert_node(
                        &mut tx,
                        &app,
                        &NodeCondition {
                            id: id.clone(),
                            revision: revision.clone(),
                        },
                    )
                    .await?;
                }
                FileAssertion::Children {
                    directory_id,
                    children_revision,
                } => {
                    if !valid_opaque_id(children_revision) {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "直属集合修订无效",
                        ));
                    }
                    let current = reference(&mut tx, &app, directory_id).await?;
                    if current.kind != NodeKind::Directory {
                        return Err(PlatformError::new(
                            error_code::NOT_DIRECTORY,
                            "断言目标不是目录",
                        ));
                    }
                    if current.children_revision.as_ref() != Some(children_revision) {
                        return Err(PlatformError::new(
                            error_code::CHILDREN_CHANGED,
                            "目录直属集合已改变",
                        ));
                    }
                }
            }
        }
        for operation in &request.operations {
            let operation_id = match operation {
                FileOperation::Mkdir {
                    operation_id,
                    parent,
                    name,
                } => {
                    if !valid_drive_name(name) {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "目录名无效",
                        ));
                    }
                    if let DirectoryTarget::Existing { id } = parent {
                        parents.insert(id.clone());
                    }
                    operation_id
                }
                FileOperation::Rename {
                    operation_id,
                    node,
                    name,
                } => {
                    if !valid_drive_name(name) {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "文件名无效",
                        ));
                    }
                    assert_node(&mut tx, &app, node).await?;
                    if !changed.insert(node.id.clone()) {
                        return Err(invalid());
                    }
                    operation_id
                }
                FileOperation::Move {
                    operation_id,
                    node,
                    parent,
                    name,
                } => {
                    if name.as_deref().is_some_and(|name| !valid_drive_name(name)) {
                        return Err(PlatformError::new(
                            error_code::INVALID_REQUEST,
                            "文件名无效",
                        ));
                    }
                    assert_node(&mut tx, &app, node).await?;
                    if !changed.insert(node.id.clone()) {
                        return Err(invalid());
                    }
                    if let DirectoryTarget::Existing { id } = parent {
                        parents.insert(id.clone());
                    }
                    operation_id
                }
                FileOperation::Trash { operation_id, node } => {
                    assert_node(&mut tx, &app, node).await?;
                    if !changed.insert(node.id.clone()) {
                        return Err(invalid());
                    }
                    operation_id
                }
                FileOperation::CommitUpload {
                    operation_id,
                    upload_id,
                } => {
                    if !valid_opaque_id(upload_id) || !upload_ids.insert(upload_id.clone()) {
                        return Err(invalid());
                    }
                    let upload = self
                        .upload_record(&mut tx, &caller.app_id, upload_id)
                        .await?;
                    self.validate_upload_commit(&mut tx, &app, &upload).await?;
                    match upload.target()? {
                        UploadTarget::Create { parent_id, .. } => {
                            parents.insert(parent_id);
                        }
                        UploadTarget::Replace { node } => {
                            if !changed.insert(node.id) {
                                return Err(invalid());
                            }
                        }
                    }
                    uploads.insert(upload_id.clone(), upload);
                    operation_id
                }
            };
            if !valid_operation_id(operation_id) || !operation_ids.insert(operation_id) {
                return Err(invalid());
            }
        }
        for parent in &parents {
            let current = reference(&mut tx, &app, parent).await?;
            if current.kind != NodeKind::Directory {
                return Err(PlatformError::new(
                    error_code::NOT_DIRECTORY,
                    "目标父节点不是目录",
                ));
            }
        }
        let mut created = BTreeMap::new();
        let mut results = Vec::new();
        for operation in &request.operations {
            let (operation_id, node_id, content_version) = match operation {
                FileOperation::Mkdir {
                    operation_id,
                    parent,
                    name,
                } => {
                    let parent = directory(&mut tx, &app, parent, &parents, &created).await?;
                    path_fits(&mut tx, None, &parent, name).await?;
                    let node = Db::mkdir_in(&mut tx, store::node_id(&parent.id)?, name)
                        .await
                        .map_err(storage_error)?;
                    created.insert(operation_id.clone(), node.id.to_string());
                    (operation_id, node.id.to_string(), None)
                }
                FileOperation::Rename {
                    operation_id,
                    node,
                    name,
                }
                | FileOperation::Move {
                    operation_id,
                    node,
                    name: Some(name),
                    ..
                } => {
                    let current = reference(&mut tx, &app, &node.id)
                        .await
                        .map_err(|_| invalid())?;
                    let source = current.parent_id.as_ref().ok_or_else(invalid)?;
                    write_access(&mut tx, &app, store::node_id(&current.id)?).await?;
                    write_access(&mut tx, &app, store::node_id(source)?).await?;
                    let parent = if let FileOperation::Move { parent, .. } = operation {
                        directory(&mut tx, &app, parent, &parents, &created).await?
                    } else {
                        reference(&mut tx, &app, source).await?
                    };
                    // 授权是祖先权限并集；根获得读写意味着整棵子树都继承该读写，不存在只读拒绝覆盖。
                    path_fits(&mut tx, Some(&current), &parent, name).await?;
                    Db::move_in(
                        &mut tx,
                        store::node_id(&current.id)?,
                        store::node_id(&parent.id)?,
                        name,
                    )
                    .await
                    .map_err(storage_error)?;
                    (operation_id, current.id, current.content_version)
                }
                FileOperation::Move {
                    operation_id,
                    node,
                    parent,
                    name: None,
                } => {
                    let current = reference(&mut tx, &app, &node.id)
                        .await
                        .map_err(|_| invalid())?;
                    let source = current.parent_id.as_ref().ok_or_else(invalid)?;
                    write_access(&mut tx, &app, store::node_id(&current.id)?).await?;
                    write_access(&mut tx, &app, store::node_id(source)?).await?;
                    let parent = directory(&mut tx, &app, parent, &parents, &created).await?;
                    path_fits(&mut tx, Some(&current), &parent, &current.name).await?;
                    Db::move_in(
                        &mut tx,
                        store::node_id(&current.id)?,
                        store::node_id(&parent.id)?,
                        &current.name,
                    )
                    .await
                    .map_err(storage_error)?;
                    (operation_id, current.id, current.content_version)
                }
                FileOperation::Trash { operation_id, node } => {
                    let current = reference(&mut tx, &app, &node.id)
                        .await
                        .map_err(|_| invalid())?;
                    let parent = current.parent_id.as_ref().ok_or_else(invalid)?;
                    write_access(&mut tx, &app, store::node_id(&current.id)?).await?;
                    write_access(&mut tx, &app, store::node_id(parent)?).await?;
                    Db::trash_in(&mut tx, store::node_id(&current.id)?, &current.path)
                        .await
                        .map_err(storage_error)?;
                    (operation_id, current.id, current.content_version)
                }
                FileOperation::CommitUpload {
                    operation_id,
                    upload_id,
                } => {
                    let upload = uploads.get(upload_id).expect("上传已在开始快照校验");
                    let result = self.commit_upload_in(&mut tx, &app, upload).await?;
                    (operation_id, result.id, Some(result.content_version))
                }
            };
            results.push(MutationResult {
                operation_id: operation_id.clone(),
                node_id,
                content_version,
            });
        }
        caller_live(self, caller)?;
        let current = store::installation(&mut tx, &caller.app_id).await?;
        current.check(&caller.generation)?;
        if current.scope != app.scope {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "事务期间目录授权改变",
            ));
        }
        for upload in uploads.values() {
            self.upload_owner_live(upload)?;
        }
        let committed_at = now();
        let receipt = MutationReceipt {
            idempotency_key: key.into(),
            committed_at,
            results,
            expires_at: tgdrive_contract::Timestamp(
                committed_at.0 + MUTATION_RECEIPT_SECONDS as i64 * 1000,
            ),
        };
        for upload in uploads.values() {
            // 创建幂等绑定必须覆盖提交回执的完整期限，不能在上传第 24 小时提交后提前过期。
            sqlx::query("UPDATE app_uploads SET expires_at=? WHERE id=? AND state='committed'")
                .bind(receipt.expires_at.0)
                .bind(&upload.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE app_write_requests SET expires_at=max(expires_at,?) WHERE app_id=? AND key=? AND kind='upload'")
                .bind(receipt.expires_at.0).bind(&app.id).bind(&upload.idempotency_key).execute(&mut *tx).await?;
        }
        bind_request(
            &mut tx,
            &app,
            key,
            "mutation",
            &canonical,
            &serde_json::to_string(&receipt)?,
            receipt.expires_at.0,
        )
        .await?;
        tx.commit().await?;
        self.forget_committed_uploads(upload_ids.iter());
        Ok(receipt)
    }
}
