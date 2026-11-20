//! 复用默认二十万文件夹具，计时实际事务接口，并用 SQLite VM 预算阻止线性扫描回退。
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tgdrive_contract::{
    files::*,
    runtime::{InstalledApp, ScopeUpdateRequest},
};

pub(super) async fn benchmark(platform: &Arc<AppPlatform>, app: &InstalledApp, root: i64) {
    let db = &platform.host.vfs.db;
    sqlx::query("UPDATE app_installations SET manifest=json_set(manifest,'$.files.write',json('true')) WHERE id=?")
        .bind(&app.manifest.id).execute(&db.pool).await.unwrap();
    store::update_scope(
        db,
        &app.manifest.id,
        ScopeUpdateRequest {
            expected_revision: app.scope.revision.clone(),
            scope: DirectoryScope::All {
                access: AccessLevel::ReadWrite,
            },
        },
    )
    .await
    .unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    let installation = store::installation(&mut connection, &app.manifest.id)
        .await
        .unwrap();
    drop(connection);
    let caller = platform
        .sessions
        .create(&installation, platform.host.auth.device_epoch());
    let revision = installation.scope_revision;
    let target = db.mkdir(root, "write-target").await.unwrap();
    let other = db.mkdir(root, "write-other").await.unwrap();
    let files = platform
        .file_query(
            &app.manifest.id,
            &app.generation,
            serde_json::from_value(serde_json::json!({"roots":[root.to_string()],"limit":100}))
                .unwrap(),
        )
        .await
        .unwrap()
        .items;
    assert_eq!(files.len(), 100);
    let ids = files.into_iter().map(|node| node.id).collect::<Vec<_>>();
    let active = Arc::new(AtomicBool::new(false));
    let steps = Arc::new(AtomicU64::new(0));
    let mut connections = Vec::new();
    for _ in 0..8 {
        let mut connection = db.pool.acquire().await.unwrap();
        let active = active.clone();
        let steps = steps.clone();
        connection
            .lock_handle()
            .await
            .unwrap()
            .set_progress_handler(1000, move || {
                !active.load(Ordering::Relaxed)
                    || steps.fetch_add(1000, Ordering::Relaxed) < 2_000_000
            });
        connections.push(connection);
    }
    drop(connections);
    let measure = || {
        steps.store(0, Ordering::Relaxed);
        active.store(true, Ordering::Relaxed);
        Instant::now()
    };
    let finish = |start: Instant| {
        active.store(false, Ordering::Relaxed);
        (start.elapsed(), steps.load(Ordering::Relaxed))
    };
    let mut mkdir_times = Vec::new();
    let mut mkdir_steps = 0;
    for index in 0..20 {
        let request = FileMutationRequest {
            scope_revision: revision.clone(),
            assertions: vec![],
            operations: vec![FileOperation::Mkdir {
                operation_id: "mkdir".into(),
                parent: DirectoryTarget::Existing {
                    id: root.to_string(),
                },
                name: format!("write-{index:02}"),
            }],
        };
        let start = measure();
        platform
            .mutate(
                &caller,
                &format!("performance-mkdir-key-{index:04}"),
                request,
            )
            .await
            .unwrap();
        let (time, count) = finish(start);
        mkdir_times.push(time);
        mkdir_steps = mkdir_steps.max(count);
    }
    let mut first_times = Vec::new();
    let mut second_times = Vec::new();
    let mut group_times = Vec::new();
    let mut move_steps = 0;
    for iteration in 0..10 {
        let mut connection = db.pool.acquire().await.unwrap();
        let app = store::installation(&mut connection, &app.manifest.id)
            .await
            .unwrap();
        let mut operations = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            let node = store::reference(
                &mut connection,
                &app,
                &NodeReference::Id {
                    id: id.clone(),
                    content_version: None,
                },
            )
            .await
            .unwrap();
            operations.push(FileOperation::Move {
                operation_id: format!("move-{index}"),
                node: NodeCondition {
                    id: node.id,
                    revision: node.revision,
                },
                parent: DirectoryTarget::Existing {
                    id: if iteration % 2 == 0 {
                        target.id
                    } else {
                        other.id
                    }
                    .to_string(),
                },
                name: None,
            });
        }
        drop(connection);
        let mut times = Vec::new();
        // 契约每批最多 64 项，100 项的完整计时明确由 64+36 两个原子批次组成。
        for (batch, operations) in operations.chunks(64).enumerate() {
            let request = FileMutationRequest {
                scope_revision: revision.clone(),
                assertions: vec![],
                operations: operations.to_vec(),
            };
            let start = measure();
            platform
                .mutate(
                    &caller,
                    &format!("performance-move-{iteration:04}-{batch}"),
                    request,
                )
                .await
                .unwrap();
            let (time, count) = finish(start);
            times.push(time);
            move_steps = move_steps.max(count);
        }
        first_times.push(times[0]);
        second_times.push(times[1]);
        group_times.push(times.iter().copied().sum());
    }
    let mut commit_times = Vec::new();
    let mut commit_steps = 0;
    for index in 0..20 {
        let upload = platform
            .create_upload(
                &caller,
                &format!("performance-upload-key-{index:04}"),
                CreateUploadRequest {
                    target: UploadTarget::Create {
                        parent_id: target.id.to_string(),
                        name: format!("upload-{index:02}.bin"),
                    },
                    size: 0.into(),
                    sha256: None,
                    scope_revision: revision.clone(),
                },
            )
            .await
            .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("content-type", "application/octet-stream".parse().unwrap());
        headers.insert("content-length", "0".parse().unwrap());
        platform
            .upload_body(&caller, &upload.id, axum::body::Body::empty(), &headers)
            .await
            .unwrap();
        let start = measure();
        platform
            .mutate(
                &caller,
                &format!("performance-commit-key-{index:04}"),
                FileMutationRequest {
                    scope_revision: revision.clone(),
                    assertions: vec![],
                    operations: vec![FileOperation::CommitUpload {
                        operation_id: "commit".into(),
                        upload_id: upload.id,
                    }],
                },
            )
            .await
            .unwrap();
        let (time, count) = finish(start);
        commit_times.push(time);
        commit_steps = commit_steps.max(count);
    }
    fn report(label: &str, times: &[Duration], steps: u64) {
        let mut sorted = times.to_vec();
        sorted.sort();
        let total: Duration = times.iter().copied().sum();
        println!(
            "写入性能：{label}，{} 次平均 {:.3?}，中位 {:.3?}，p95 {:.3?}，SQLite VM 最大 {steps} 步",
            times.len(),
            total / times.len() as u32,
            sorted[sorted.len() / 2],
            sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)]
        );
    }
    report("mkdir 单项", &mkdir_times, mkdir_steps);
    report("move 第一批 64 项", &first_times, move_steps);
    report("move 第二批 36 项", &second_times, move_steps);
    report("move 完整 100 项（两个事务）", &group_times, move_steps);
    report(
        "ready 上传提交（不计 Telegram 网络）",
        &commit_times,
        commit_steps,
    );
    assert!(mkdir_steps < 100_000);
    assert!(move_steps < 1_000_000);
    assert!(commit_steps < 100_000);
}
