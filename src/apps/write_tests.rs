//! 同一 SDK/Unix HTTP 协议的事务、上传、幂等回执和读写授权验收。
use super::backend_tests::{backend_fixture, test_file};
use super::*;
use tgdrive_backend_sdk::{
    HostClient, ReadOptions, StartContext, UnixHostClient, byte_stream, collect_bytes,
};
use tgdrive_contract::{CONTRACT_VERSION, files::*, runtime::*};

pub(super) fn client(run: &backend::Run) -> UnixHostClient {
    let bytes =
        std::fs::read(std::path::Path::new(&run.data_dir).join("run/context.json")).unwrap();
    UnixHostClient::new(&StartContext::parse(&bytes, CONTRACT_VERSION).unwrap())
}
fn node_condition(node: &FileNode) -> NodeCondition {
    NodeCondition {
        id: node.id.clone(),
        revision: node.revision.clone(),
    }
}
async fn scope(host: &UnixHostClient) -> String {
    host.scope().await.unwrap().revision
}

#[tokio::test]
async fn conditional_group_moves_are_atomic_and_receipts_survive_backend_restart() {
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let file = test_file(&platform, &fake, b"source").await;
    let source = host
        .stat(NodeReference::Id {
            id: file.id.clone(),
            content_version: None,
        })
        .await
        .unwrap();
    let root = host
        .stat(NodeReference::Id {
            id: "1".into(),
            content_version: None,
        })
        .await
        .unwrap();
    let request = FileMutationRequest {
        scope_revision: scope(&host).await,
        assertions: vec![FileAssertion::Children {
            directory_id: "1".into(),
            children_revision: root.children_revision.unwrap(),
        }],
        operations: vec![
            FileOperation::Mkdir {
                operation_id: "group".into(),
                parent: DirectoryTarget::Existing { id: "1".into() },
                name: "group".into(),
            },
            FileOperation::Move {
                operation_id: "move".into(),
                node: node_condition(&source),
                parent: DirectoryTarget::Created {
                    operation_id: "group".into(),
                },
                name: None,
            },
        ],
    };
    let receipt = host
        .mutate("group-mutation-key-0001".into(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        receipt.results[1].content_version.as_ref(),
        Some(&file.content_version)
    );
    assert_eq!(
        host.mutate("group-mutation-key-0001".into(), request)
            .await
            .unwrap(),
        receipt
    );
    let source = host
        .stat(NodeReference::Id {
            id: file.id.clone(),
            content_version: None,
        })
        .await
        .unwrap();
    assert_eq!(source.path, "/group/input.bin");
    let bad = FileMutationRequest {
        scope_revision: scope(&host).await,
        assertions: vec![],
        operations: vec![
            FileOperation::Mkdir {
                operation_id: "temporary".into(),
                parent: DirectoryTarget::Existing { id: "1".into() },
                name: "rollback".into(),
            },
            FileOperation::Move {
                operation_id: "conflict".into(),
                node: node_condition(&source),
                parent: DirectoryTarget::Existing { id: "1".into() },
                name: Some("group".into()),
            },
        ],
    };
    assert_eq!(
        host.mutate("rollback-mutation-key-0001".into(), bad)
            .await
            .unwrap_err()
            .code(),
        tgdrive_contract::error_code::NAME_CONFLICT
    );
    assert_eq!(
        host.stat(NodeReference::Path {
            path: "/rollback".into(),
            content_version: None
        })
        .await
        .unwrap_err()
        .code(),
        tgdrive_contract::error_code::NOT_FOUND
    );
    assert_eq!(
        host.mutation_receipt("rollback-mutation-key-0001".into())
            .await
            .unwrap_err()
            .code(),
        tgdrive_contract::error_code::NOT_FOUND
    );
    platform
        .backends
        .restart(&platform, "test-backend")
        .await
        .unwrap();
    let new = platform.backends.run("test-backend").unwrap();
    assert_ne!(new.caller.id, run.caller.id);
    assert_eq!(
        client(&new)
            .mutation_receipt("group-mutation-key-0001".into())
            .await
            .unwrap(),
        receipt
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn upload_staging_commits_once_preserves_replace_identity_and_renews_content_version() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let bytes = bytes::Bytes::from_static(b"uploaded content");
    let request = CreateUploadRequest {
        target: UploadTarget::Create {
            parent_id: "1".into(),
            name: "upload.bin".into(),
        },
        size: (bytes.len() as u64).into(),
        sha256: None,
        scope_revision: scope(&host).await,
    };
    let upload = host
        .create_upload("create-upload-key-0001".into(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        host.create_upload("create-upload-key-0001".into(), request)
            .await
            .unwrap()
            .id,
        upload.id
    );
    let ready = host
        .upload_body(
            upload.id.clone(),
            byte_stream(bytes.clone()),
            Some(bytes.len() as u64),
        )
        .await
        .unwrap();
    assert_eq!(ready.state, UploadState::Ready);
    assert_eq!(ready.received_bytes.0, bytes.len() as u64);
    assert_eq!(
        host.stat(NodeReference::Path {
            path: "/upload.bin".into(),
            content_version: None
        })
        .await
        .unwrap_err()
        .code(),
        tgdrive_contract::error_code::NOT_FOUND
    );
    assert_eq!(
        host.upload_body(
            upload.id.clone(),
            byte_stream(bytes.clone()),
            Some(bytes.len() as u64)
        )
        .await
        .unwrap_err()
        .code(),
        tgdrive_contract::error_code::UPLOAD_STATE
    );
    let receipt = host
        .mutate(
            "commit-upload-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::CommitUpload {
                    operation_id: "commit".into(),
                    upload_id: upload.id.clone(),
                }],
            },
        )
        .await
        .unwrap();
    let original = host
        .stat(NodeReference::Id {
            id: receipt.results[0].node_id.clone(),
            content_version: None,
        })
        .await
        .unwrap();
    assert_eq!(
        host.cancel_upload(upload.id).await.unwrap_err().code(),
        tgdrive_contract::error_code::UPLOAD_STATE
    );
    let response = host
        .bytes(
            ContentRef {
                id: original.id.clone(),
                content_version: original.content_version.clone().unwrap(),
            },
            ReadOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(collect_bytes(response.body, 1024).await.unwrap(), bytes);
    let replacement = host
        .create_upload(
            "replace-upload-key-0001".into(),
            CreateUploadRequest {
                target: UploadTarget::Replace {
                    node: node_condition(&original),
                },
                size: (bytes.len() as u64).into(),
                sha256: None,
                scope_revision: scope(&host).await,
            },
        )
        .await
        .unwrap();
    host.upload_body(
        replacement.id.clone(),
        byte_stream(bytes.clone()),
        Some(bytes.len() as u64),
    )
    .await
    .unwrap();
    let replaced = host
        .mutate(
            "replace-commit-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::CommitUpload {
                    operation_id: "replace".into(),
                    upload_id: replacement.id,
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(replaced.results[0].node_id, original.id);
    assert_ne!(
        replaced.results[0].content_version,
        original.content_version
    );
    let current = host
        .stat(NodeReference::Id {
            id: original.id,
            content_version: None,
        })
        .await
        .unwrap();
    assert_eq!(current.created_at, original.created_at);
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn upload_final_scope_checks_failed_bodies_and_root_parent_permissions_are_enforced() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let upload = host
        .create_upload(
            "short-upload-key-0001".into(),
            CreateUploadRequest {
                target: UploadTarget::Create {
                    parent_id: "1".into(),
                    name: "short.bin".into(),
                },
                size: 10.into(),
                sha256: None,
                scope_revision: scope(&host).await,
            },
        )
        .await
        .unwrap();
    assert!(
        host.upload_body(
            upload.id.clone(),
            byte_stream(bytes::Bytes::from_static(b"short")),
            Some(5)
        )
        .await
        .is_err()
    );
    assert_eq!(
        host.upload_status(upload.id).await.unwrap().state,
        UploadState::Failed
    );
    let ready = host
        .create_upload(
            "scope-upload-key-0001".into(),
            CreateUploadRequest {
                target: UploadTarget::Create {
                    parent_id: "1".into(),
                    name: "revoked.bin".into(),
                },
                size: 0.into(),
                sha256: None,
                scope_revision: scope(&host).await,
            },
        )
        .await
        .unwrap();
    host.upload_body(ready.id.clone(), byte_stream(bytes::Bytes::new()), Some(0))
        .await
        .unwrap();
    let directory = host
        .mutate(
            "create-root-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::Mkdir {
                    operation_id: "root".into(),
                    parent: DirectoryTarget::Existing { id: "1".into() },
                    name: "authorized".into(),
                }],
            },
        )
        .await
        .unwrap()
        .results[0]
        .node_id
        .clone();
    let app = platform.installed("test-backend").await.unwrap();
    store::update_scope(
        &platform.host.vfs.db,
        "test-backend",
        ScopeUpdateRequest {
            expected_revision: app.scope.revision,
            scope: DirectoryScope::Selected {
                directories: vec![DirectoryGrant {
                    directory_id: directory.clone(),
                    access: AccessLevel::ReadWrite,
                }],
            },
        },
    )
    .await
    .unwrap();
    assert!(
        host.mutate(
            "revoked-commit-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::CommitUpload {
                    operation_id: "commit".into(),
                    upload_id: ready.id
                }]
            }
        )
        .await
        .is_err()
    );
    let current = host
        .stat(NodeReference::Id {
            id: directory.clone(),
            content_version: None,
        })
        .await
        .unwrap();
    assert_eq!(
        host.mutate(
            "rename-root-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::Rename {
                    operation_id: "rename".into(),
                    node: node_condition(&current),
                    name: "denied".into()
                }]
            }
        )
        .await
        .unwrap_err()
        .code(),
        tgdrive_contract::error_code::SCOPE_DENIED
    );
    let result = host
        .mutate(
            "child-mkdir-key-0001".into(),
            FileMutationRequest {
                scope_revision: scope(&host).await,
                assertions: vec![],
                operations: vec![FileOperation::Mkdir {
                    operation_id: "child".into(),
                    parent: DirectoryTarget::Existing { id: directory },
                    name: "allowed".into(),
                }],
            },
        )
        .await
        .unwrap();
    let node = host
        .stat(NodeReference::Id {
            id: result.results[0].node_id.clone(),
            content_version: None,
        })
        .await
        .unwrap();
    host.mutate(
        "trash-child-key-0001".into(),
        FileMutationRequest {
            scope_revision: scope(&host).await,
            assertions: vec![],
            operations: vec![FileOperation::Trash {
                operation_id: "trash".into(),
                node: node_condition(&node),
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(platform.host.vfs.db.trash_list().await.unwrap().len(), 1);
    platform.backends.stop("test-backend").await.unwrap();
}
