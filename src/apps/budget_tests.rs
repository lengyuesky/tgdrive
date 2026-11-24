//! 大项分页与缓存配额，不用降低默认条数来掩盖响应字节预算。
use super::tests::{authorize_all, fixture};
use tgdrive_contract::{data::*, error_code, files::*, http::*, runtime::*};

#[tokio::test]
async fn long_paths_end_pages_by_bytes_without_skipping_nodes() {
    let (_dir, platform, _) = fixture().await;
    let app = authorize_all(&platform).await;
    let db = &platform.host.vfs.db;
    let mut parent = 1;
    for index in 0..16 {
        parent = db
            .mkdir(parent, &format!("d{index:02}{}", "x".repeat(235)))
            .await
            .unwrap()
            .id;
    }
    sqlx::query("WITH RECURSIVE series(x) AS(SELECT 0 UNION ALL SELECT x+1 FROM series WHERE x<499) INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) SELECT ?,printf('file%06d.bin',x),0,1,'ready',1,1 FROM series").bind(parent).execute(&db.pool).await.unwrap();
    let mut query: FileQuery =
        serde_json::from_value(serde_json::json!({"roots":[parent.to_string()],"limit":500}))
            .unwrap();
    let mut total = 0;
    let mut pages = 0;
    loop {
        let result = platform
            .file_query(&app.manifest.id, &app.generation, query.clone())
            .await
            .unwrap();
        assert!(result.items.len() < 500);
        tgdrive_contract::validation::validate_json_page(&result.items, MAX_JSON_PAGE_BYTES)
            .unwrap();
        total += result.items.len();
        pages += 1;
        query.cursor = result.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    assert_eq!(total, 500);
    assert!(pages > 1);
    platform
        .uninstall(&app.manifest.id, &app.state_revision, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn cache_lru_evicts_to_ninety_percent_and_keeps_new_object() {
    let (_dir, platform, _) = fixture().await;
    let app = authorize_all(&platform).await;
    let launch = platform
        .launch(
            &app.manifest.id,
            LaunchPageRequest { open_node_id: None },
            "http://localhost".into(),
        )
        .await
        .unwrap();
    let page = platform
        .sessions
        .authenticate(
            &platform.cursors,
            &launch.bootstrap.session_token,
            &platform.host.auth.device_epoch(),
        )
        .unwrap();
    // 给默认元数据留出余量，第 513 次写入的对象正文总量也实际超过 1 GiB。
    let body = bytes::Bytes::from(vec![1; MAX_CACHE_OBJECT_BYTES as usize - 1024]);
    let object_count = DEFAULT_CACHE_BYTES / body.len() as u64 + 1;
    assert!(object_count >= 513);
    assert!(object_count * body.len() as u64 > DEFAULT_CACHE_BYTES);
    let started = std::time::Instant::now();
    for index in 0..object_count {
        platform
            .cache_put(
                &page,
                &format!("object-{index:03}"),
                "application/octet-stream".into(),
                CachePutMetadata::default(),
                body.clone(),
            )
            .await
            .unwrap();
        if index + 2 == object_count {
            let usage = platform.cache_usage(&app.manifest.id).await.unwrap();
            assert!(usage <= DEFAULT_CACHE_BYTES);
            assert!(usage > DEFAULT_CACHE_BYTES / 100 * CACHE_EVICTION_TARGET_PERCENT);
        }
    }
    let elapsed = started.elapsed();
    assert!(
        platform.cache_usage(&app.manifest.id).await.unwrap()
            <= DEFAULT_CACHE_BYTES / 100 * CACHE_EVICTION_TARGET_PERCENT
    );
    assert!(
        platform
            .cache_read(
                &page,
                &format!("object-{:03}", object_count - 1),
                false,
                None
            )
            .await
            .is_ok()
    );
    assert_eq!(
        platform
            .cache_read(&page, "object-000", false, None)
            .await
            .err()
            .unwrap()
            .code,
        error_code::NOT_FOUND
    );
    println!(
        "缓存淘汰验收：{object_count} 个真实对象、正文 {} 字节，写入与淘汰耗时 {:.3} 秒",
        object_count * body.len() as u64,
        elapsed.as_secs_f64()
    );
    platform
        .uninstall(&app.manifest.id, &app.state_revision, true)
        .await
        .unwrap();
}
