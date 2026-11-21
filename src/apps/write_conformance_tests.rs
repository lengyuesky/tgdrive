//! 写入边界、整组条件回滚、上传完整性和撤销的真实 SDK 符合性验收。
use super::backend_tests::{backend_fixture, test_file};
use super::write_tests::client;
use super::*;
use tgdrive_backend_sdk::{HostClient, byte_stream};
use tgdrive_contract::{error_code, files::*, runtime::*};

fn mkdir(revision: &str, name: &str) -> FileMutationRequest {
    FileMutationRequest {
        scope_revision: revision.into(),
        assertions: vec![],
        operations: vec![FileOperation::Mkdir {
            operation_id: "mkdir".into(),
            parent: DirectoryTarget::Existing { id: "1".into() },
            name: name.into(),
        }],
    }
}

#[tokio::test]
async fn stale_assertions_duplicate_nodes_and_transaction_limits_do_not_leave_partial_changes() {
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let file = test_file(&platform, &fake, b"original").await;
    let node = host
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
    let revision = host.scope().await.unwrap().revision;
    platform.host.vfs.db.mkdir(1, "concurrent").await.unwrap();
    let mut request = mkdir(&revision, "must-not-exist");
    request.assertions.push(FileAssertion::Children {
        directory_id: "1".into(),
        children_revision: root.children_revision.unwrap(),
    });
    assert_eq!(
        host.mutate("stale-children-key-0001".into(), request)
            .await
            .unwrap_err()
            .code(),
        error_code::CHILDREN_CHANGED
    );
    assert!(
        platform
            .host
            .vfs
            .db
            .child(1, "must-not-exist")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        host.mutation_receipt("stale-children-key-0001".into())
            .await
            .unwrap_err()
            .code(),
        error_code::NOT_FOUND
    );
    let condition = NodeCondition {
        id: node.id.clone(),
        revision: node.revision.clone(),
    };
    let repeated = FileMutationRequest {
        scope_revision: revision.clone(),
        assertions: vec![],
        operations: vec![
            FileOperation::Rename {
                operation_id: "one".into(),
                node: condition.clone(),
                name: "one.bin".into(),
            },
            FileOperation::Trash {
                operation_id: "two".into(),
                node: condition.clone(),
            },
        ],
    };
    assert_eq!(
        host.mutate("duplicate-node-key-0001".into(), repeated)
            .await
            .unwrap_err()
            .code(),
        error_code::INVALID_OPERATION
    );
    platform
        .host
        .vfs
        .db
        .move_node(node.id.parse().unwrap(), 1, "changed.bin")
        .await
        .unwrap();
    let stale = FileMutationRequest {
        scope_revision: revision.clone(),
        assertions: vec![],
        operations: vec![FileOperation::Trash {
            operation_id: "stale".into(),
            node: condition,
        }],
    };
    assert_eq!(
        host.mutate("stale-node-key-0001".into(), stale)
            .await
            .unwrap_err()
            .code(),
        error_code::NODE_CHANGED
    );
    let mut maximum = mkdir(&revision, "unused");
    maximum.operations = (0..64)
        .map(|index| FileOperation::Mkdir {
            operation_id: format!("op-{index}"),
            parent: DirectoryTarget::Existing { id: "1".into() },
            name: format!("bounded-{index}"),
        })
        .collect();
    assert_eq!(
        host.mutate("max-operations-key-0001".into(), maximum.clone())
            .await
            .unwrap()
            .results
            .len(),
        64
    );
    maximum.operations.push(FileOperation::Mkdir {
        operation_id: "extra".into(),
        parent: DirectoryTarget::Existing { id: "1".into() },
        name: "extra".into(),
    });
    assert_eq!(
        host.mutate("too-many-operations-key-0001".into(), maximum)
            .await
            .unwrap_err()
            .code(),
        error_code::INVALID_REQUEST
    );
    assert!(
        platform
            .host
            .vfs
            .db
            .child(1, "extra")
            .await
            .unwrap()
            .is_none()
    );
    let receipt = host
        .mutate(
            "idempotent-directory-key-0001".into(),
            mkdir(&revision, "idempotent"),
        )
        .await
        .unwrap();
    assert_eq!(
        host.mutate(
            "idempotent-directory-key-0001".into(),
            mkdir(&revision, "different")
        )
        .await
        .unwrap_err()
        .code(),
        error_code::IDEMPOTENCY_CONFLICT
    );
    let app = platform.installed("test-backend").await.unwrap();
    platform
        .update_scope(
            "test-backend",
            ScopeUpdateRequest {
                expected_revision: app.scope.revision,
                scope: DirectoryScope::None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        host.mutation_receipt("idempotent-directory-key-0001".into())
            .await
            .unwrap(),
        receipt
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn body_integrity_and_restart_revoke_uncommitted_uploads_but_keep_creation_binding() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let revision = host.scope().await.unwrap().revision;
    for (label, size, sha, expected) in [
        ("oversized", 1, None, error_code::PAYLOAD_TOO_LARGE),
        (
            "checksum",
            3,
            Some("0".repeat(64)),
            error_code::CONTENT_CHANGED,
        ),
    ] {
        let upload = host
            .create_upload(
                format!("{label}-upload-key-0001"),
                CreateUploadRequest {
                    target: UploadTarget::Create {
                        parent_id: "1".into(),
                        name: format!("{label}.bin"),
                    },
                    size: size.into(),
                    sha256: sha,
                    scope_revision: revision.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            host.upload_body(
                upload.id.clone(),
                byte_stream(bytes::Bytes::from_static(b"bad")),
                Some(3)
            )
            .await
            .unwrap_err()
            .code(),
            expected
        );
        assert_eq!(
            host.upload_status(upload.id).await.unwrap().state,
            UploadState::Failed
        );
        assert!(
            platform
                .host
                .vfs
                .db
                .child(1, &format!("{label}.bin"))
                .await
                .unwrap()
                .is_none()
        );
    }
    let request = CreateUploadRequest {
        target: UploadTarget::Create {
            parent_id: "1".into(),
            name: "cancelled.bin".into(),
        },
        size: 0.into(),
        sha256: None,
        scope_revision: revision,
    };
    let upload = host
        .create_upload("restart-upload-key-0001".into(), request.clone())
        .await
        .unwrap();
    host.upload_body(upload.id.clone(), byte_stream(bytes::Bytes::new()), Some(0))
        .await
        .unwrap();
    platform
        .backends
        .restart(&platform, "test-backend")
        .await
        .unwrap();
    platform.prune_uploads().await.unwrap();
    let new = platform.backends.run("test-backend").unwrap();
    let host = client(&new);
    assert_eq!(
        host.upload_status(upload.id.clone()).await.unwrap().state,
        UploadState::Cancelled
    );
    assert_eq!(
        host.create_upload("restart-upload-key-0001".into(), request)
            .await
            .unwrap()
            .id,
        upload.id
    );
    let commit = FileMutationRequest {
        scope_revision: host.scope().await.unwrap().revision,
        assertions: vec![],
        operations: vec![FileOperation::CommitUpload {
            operation_id: "commit".into(),
            upload_id: upload.id,
        }],
    };
    assert_eq!(
        host.mutate("restart-commit-key-0001".into(), commit)
            .await
            .unwrap_err()
            .code(),
        error_code::UPLOAD_EXPIRED
    );
    assert!(
        platform
            .host
            .vfs
            .db
            .child(1, "cancelled.bin")
            .await
            .unwrap()
            .is_none()
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn upload_creation_idempotency_lasts_until_the_corresponding_commit_receipt_expires() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let revision = host.scope().await.unwrap().revision;
    let key = "long-upload-create-key-0001";
    let upload = host
        .create_upload(
            key.into(),
            CreateUploadRequest {
                target: UploadTarget::Create {
                    parent_id: "1".into(),
                    name: "long.bin".into(),
                },
                size: 0.into(),
                sha256: None,
                scope_revision: revision.clone(),
            },
        )
        .await
        .unwrap();
    host.upload_body(upload.id.clone(), byte_stream(bytes::Bytes::new()), Some(0))
        .await
        .unwrap();
    // 模拟上传花了接近一天：创建绑定的原期限早于最终提交回执。
    sqlx::query("UPDATE app_write_requests SET expires_at=? WHERE app_id=? AND key=?")
        .bind(now().0 + 1000)
        .bind("test-backend")
        .bind(key)
        .execute(&platform.host.vfs.db.pool)
        .await
        .unwrap();
    let receipt = host
        .mutate(
            "long-upload-commit-key-0001".into(),
            FileMutationRequest {
                scope_revision: revision,
                assertions: vec![],
                operations: vec![FileOperation::CommitUpload {
                    operation_id: "commit".into(),
                    upload_id: upload.id.clone(),
                }],
            },
        )
        .await
        .unwrap();
    let expiry: i64 =
        sqlx::query_scalar("SELECT expires_at FROM app_write_requests WHERE app_id=? AND key=?")
            .bind("test-backend")
            .bind(key)
            .fetch_one(&platform.host.vfs.db.pool)
            .await
            .unwrap();
    assert_eq!(expiry, receipt.expires_at.0);
    assert_eq!(
        host.upload_status(upload.id).await.unwrap().expires_at,
        receipt.expires_at
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn scope_revocation_interrupts_an_idle_upload_without_waiting_for_another_body_chunk() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let host = client(&run);
    let upload = host
        .create_upload(
            "idle-upload-create-key-0001".into(),
            CreateUploadRequest {
                target: UploadTarget::Create {
                    parent_id: "1".into(),
                    name: "idle.bin".into(),
                },
                size: 8.into(),
                sha256: None,
                scope_revision: host.scope().await.unwrap().revision,
            },
        )
        .await
        .unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);
    sender
        .send(bytes::Bytes::from_static(b"1234"))
        .await
        .unwrap();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, std::io::Error>(bytes), receiver))
    });
    let body = axum::body::Body::from_stream(stream);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.insert("content-length", "8".parse().unwrap());
    let task_platform = platform.clone();
    let caller = run.caller.clone();
    let id = upload.id.clone();
    let task = tokio::spawn(async move {
        task_platform
            .upload_body(&caller, &id, body, &headers)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if host
                .upload_status(upload.id.clone())
                .await
                .unwrap()
                .received_bytes
                .0
                == 4
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let epoch = platform
        .host
        .vfs
        .db
        .app_epoch
        .load(std::sync::atomic::Ordering::Acquire);
    let app = platform.installed("test-backend").await.unwrap();
    platform
        .update_scope(
            "test-backend",
            ScopeUpdateRequest {
                expected_revision: app.scope.revision,
                scope: DirectoryScope::None,
            },
        )
        .await
        .unwrap();
    assert!(
        platform
            .host
            .vfs
            .db
            .app_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            > epoch
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        error_code::SCOPE_CHANGED
    );
    drop(sender);
    assert!(
        platform
            .host
            .vfs
            .db
            .child(1, "idle.bin")
            .await
            .unwrap()
            .is_none()
    );
    platform.backends.stop("test-backend").await.unwrap();
}
