//! 默认测试即执行二十万节点验收，测试替身不会进入正式可执行文件。

use super::tests::{authorize_all, fixture};
use std::time::Instant;
use tgdrive_contract::files::*;

#[tokio::test]
async fn two_hundred_thousand_nodes_use_keysets_and_indexed_random() {
    println!(
        "性能构建：{}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    let (_dir, platform, fake) = fixture().await;
    let _plan_guard = super::query::PLAN_CAPTURE_LOCK.lock().await;
    let app = authorize_all(&platform).await;
    *super::query::CAPTURE_GENERATION.lock().unwrap() = Some(app.generation.clone());
    let db = &platform.host.vfs.db;
    let start = Instant::now();
    let root = db.mkdir(1, "benchmark").await.unwrap();
    let mut folders = Vec::new();
    for index in 0..200 {
        folders.push(
            db.mkdir(root.id, &format!("part-{index:03}"))
                .await
                .unwrap()
                .id,
        );
    }
    let base = folders[0];
    sqlx::query("WITH RECURSIVE series(x) AS(SELECT 0 UNION ALL SELECT x+1 FROM series WHERE x<199999) INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) SELECT ?+(x%200),printf('%09d.%s',x,CASE WHEN x%2=0 THEN 'jpg' ELSE 'bin' END),0,100,'ready',1700000000,1700000000 FROM series")
        .bind(base).execute(&db.pool).await.unwrap();
    println!(
        "性能：构造 200000 文件、201 目录，用时 {:.2?}",
        start.elapsed()
    );
    let query: FileQuery =
        serde_json::from_value(serde_json::json!({"roots":[root.id.to_string()],"limit":500}))
            .unwrap();
    let directory:DirectoriesWithFilesQuery=serde_json::from_value(serde_json::json!({"roots":[root.id.to_string()],"file_filter":{"extensions":["jpg"]},"limit":500})).unwrap();
    let random:RandomFilesRequest=serde_json::from_value(serde_json::json!({"roots":[root.id.to_string()],"filter":{"extensions":["jpg"]},"limit":64})).unwrap();
    super::query::CAPTURE_PLANS.store(true, std::sync::atomic::Ordering::Relaxed);
    platform
        .file_query(&app.manifest.id, &app.generation, query.clone())
        .await
        .unwrap();
    platform
        .directories_with_files(&app.manifest.id, &app.generation, directory.clone())
        .await
        .unwrap();
    platform
        .random_files(&app.manifest.id, &app.generation, random.clone())
        .await
        .unwrap();
    super::query::CAPTURE_PLANS.store(false, std::sync::atomic::Ordering::Relaxed);
    for (label, plan) in super::query::PLANS.lock().unwrap().drain(..) {
        println!("EXPLAIN {label}：{}", plan.join("；"));
        if label == "query" {
            assert!(plan.iter().any(|line| line.contains("app_nodes_name")));
        }
        if label == "directories-with-files" {
            assert!(plan.iter().any(|line| line.contains("app_dirs_name")));
        }
        if label == "random" {
            assert!(
                plan.iter()
                    .any(|line| line.contains("SEARCH app_random_index USING PRIMARY KEY"))
            );
            assert!(!plan.iter().any(|line| line.starts_with("SCAN n")));
        }
    }
    let start = Instant::now();
    for _ in 0..20 {
        assert_eq!(
            platform
                .file_query(&app.manifest.id, &app.generation, query.clone())
                .await
                .unwrap()
                .items
                .len(),
            500
        );
    }
    println!(
        "性能：query 首批 500 行，20 次平均 {:.2?}",
        start.elapsed() / 20
    );
    let start = Instant::now();
    for _ in 0..20 {
        let result = platform
            .directories_with_files(&app.manifest.id, &app.generation, directory.clone())
            .await
            .unwrap();
        assert_eq!(result.items.len(), 100);
    }
    println!(
        "性能：directories-with-files 100 个直属匹配目录，20 次平均 {:.2?}",
        start.elapsed() / 20
    );
    let start = Instant::now();
    for _ in 0..20 {
        let result = platform
            .random_files(&app.manifest.id, &app.generation, random.clone())
            .await
            .unwrap();
        assert_eq!(result.items.len(), 64);
        assert!(
            result
                .items
                .iter()
                .any(|node| node.id.parse::<i64>().unwrap() > 10000)
        );
    }
    println!("性能：random 64 行，20 次平均 {:.2?}", start.elapsed() / 20);
    let start = Instant::now();
    let mut page = query;
    let mut total = 0;
    let mut pages = 0;
    let mut durations = Vec::new();
    loop {
        let number = pages + 1;
        if number == 200 {
            super::query::CAPTURE_PLANS.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let page_start = Instant::now();
        let result = platform
            .file_query(&app.manifest.id, &app.generation, page.clone())
            .await
            .unwrap();
        durations.push(page_start.elapsed());
        if number == 200 {
            super::query::CAPTURE_PLANS.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if [1, 100, 200, 300, 400].contains(&number) {
            println!("分页深度：第 {number} 页 {:.2?}", durations.last().unwrap());
        }
        total += result.items.len();
        pages += 1;
        page.cursor = result.next_cursor;
        if page.cursor.is_none() {
            break;
        }
    }
    for (index, segment) in durations.chunks(80).enumerate() {
        let total: std::time::Duration = segment.iter().copied().sum();
        println!(
            "分页分段：{}-{} 页平均 {:.2?}",
            index * 80 + 1,
            index * 80 + segment.len(),
            total / segment.len() as u32
        );
    }
    for (label, plan) in super::query::PLANS.lock().unwrap().drain(..) {
        println!("EXPLAIN 深页 {label}：{}", plan.join("；"));
        if label == "query.cursor" {
            assert!(
                plan.iter()
                    .any(|line| line.contains("SEARCH n USING INDEX app_nodes_name")),
                "深页必须按游标主键范围寻址"
            );
        }
    }
    assert_eq!(total, 200000);
    assert_eq!(pages, 400);
    println!(
        "性能：400 次键集分页完整读取 200000 文件，用时 {:.2?}",
        start.elapsed()
    );
    let request:RandomFilesRequest=serde_json::from_value(serde_json::json!({"roots":[root.id.to_string()],"filter":{"extensions":["jpg"],"name_contains":"没有匹配"},"limit":20})).unwrap();
    assert_eq!(
        platform
            .random_files(&app.manifest.id, &app.generation, request)
            .await
            .unwrap_err()
            .code,
        tgdrive_contract::error_code::QUOTA_EXCEEDED
    );
    assert_eq!(
        fake.ordinary_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        fake.strict_reads.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}
