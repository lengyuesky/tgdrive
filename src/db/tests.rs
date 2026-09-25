use super::*;

const TEST_TARGET: i64 = 1;

fn stored(message_id: i64) -> StoredMessage {
    StoredMessage {
        storage_target_id: TEST_TARGET,
        message_id,
    }
}

#[tokio::test]
async fn tree_queries_walk_whole_chain_in_one_query() {
    let (db, _g) = test_db().await;
    let a = db.mkdir(ROOT_ID, "a").await.unwrap();
    let b = db.mkdir(a.id, "b").await.unwrap();
    let c = db.mkdir(b.id, "c").await.unwrap();
    assert_eq!(db.resolve(&[]).await.unwrap().unwrap().id, ROOT_ID);
    assert_eq!(
        db.resolve(&["a", "b", "c"]).await.unwrap().unwrap().id,
        c.id
    );
    assert!(db.resolve(&["a", "c"]).await.unwrap().is_none());
    assert!(db.resolve(&["a", "b", "c", "d"]).await.unwrap().is_none());
    assert!(db.is_ancestor(a.id, c.id).await.unwrap());
    assert!(db.is_ancestor(c.id, c.id).await.unwrap());
    assert!(!db.is_ancestor(c.id, a.id).await.unwrap());
    assert!(db.move_node(a.id, c.id, "a").await.is_err());

    db.trash_node(b.id, "/a/b").await.unwrap();
    assert!(db.resolve(&["a", "b", "c"]).await.unwrap().is_none());
    let trash = db.trash_list().await.unwrap();
    assert_eq!(trash.len(), 1);
    assert_eq!((trash[0].0.id, trash[0].1.as_str()), (b.id, "/a/b"));
}

#[tokio::test]
async fn copy_tree_is_all_or_nothing() {
    let (db, _g) = test_db().await;
    let src = db.mkdir(ROOT_ID, "src").await.unwrap();
    let inner = db.mkdir(src.id, "inner").await.unwrap();
    db.mkdir(inner.id, "leaf").await.unwrap();
    db.copy_tree(src.id, ROOT_ID, "dst").await.unwrap();
    assert!(
        db.resolve(&["dst", "inner", "leaf"])
            .await
            .unwrap()
            .is_some()
    );

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nodes")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let err = db.copy_tree(src.id, ROOT_ID, "dst").await.unwrap_err();
    assert!(matches!(err, AppError::AlreadyExists), "{err:?}");
    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nodes")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(before, after, "失败的复制不能留下半棵树");
}

#[tokio::test]
async fn pending_migration_snapshots_existing_db_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    let backups = || {
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".pre-migrate-")
            })
            .count()
    };
    let db = Db::open(path).await.unwrap();
    assert_eq!(backups(), 0, "全新库不应生成快照");
    let migrator = sqlx::migrate!("./migrations");
    snapshot_before_migration(&db.pool, &migrator, path)
        .await
        .unwrap();
    assert_eq!(backups(), 0, "无待执行迁移不应生成快照");
    sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    snapshot_before_migration(&db.pool, &migrator, path)
        .await
        .unwrap();
    assert_eq!(backups(), 1);
}

async fn test_db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let db = Db::open(path.to_str().unwrap()).await.unwrap();
    let target = db
        .ensure_storage_target("http://mock", "mock-token", "-1001")
        .await
        .unwrap();
    assert_eq!(target.id, TEST_TARGET);
    (db, dir)
}

#[tokio::test]
async fn tree_basic() {
    let (db, _g) = test_db().await;
    let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
    assert!(db.mkdir(ROOT_ID, "docs").await.is_err()); // 重名
    let sub = db.mkdir(docs.id, "2026").await.unwrap();
    let found = db.resolve(&["docs", "2026"]).await.unwrap().unwrap();
    assert_eq!(found.id, sub.id);
    assert!(db.resolve(&["nope"]).await.unwrap().is_none());

    // 移动 + 防环
    db.move_node(sub.id, ROOT_ID, "y2026").await.unwrap();
    assert!(db.resolve(&["y2026"]).await.unwrap().is_some());
    assert!(db.move_node(docs.id, docs.id, "x").await.is_err());
}

#[tokio::test]
async fn migration_removes_blob_thumbnail_cache() {
    let (db, _g) = test_db().await;
    let (tables,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'thumbnails'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(tables, 0);

    let (columns,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('chunks') WHERE name = 'tg_thumbnail_file_id'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(columns, 1);

    let (caption_tables,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'caption_sync_jobs'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(caption_tables, 0);
}

#[tokio::test]
async fn backend_logs_are_newest_first_and_paginated() {
    let (db, _g) = test_db().await;
    db.append_backend_log("info", "维护", "第一条")
        .await
        .unwrap();
    db.append_backend_log("warn", "维护", "第二条")
        .await
        .unwrap();
    db.append_backend_log("unknown", "", "").await.unwrap();

    let none = BackendLogFilter::default();
    let (latest, has_more) = db.backend_logs(2, None, &none).await.unwrap();
    assert_eq!(latest.len(), 2);
    assert!(has_more);
    assert_eq!(latest[0].message, "后台事件");
    assert_eq!(latest[0].level, "info");
    assert_eq!(latest[1].message, "第二条");

    let (older, has_more) = db.backend_logs(2, Some(latest[1].id), &none).await.unwrap();
    assert!(!has_more);
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].message, "第一条");
}

#[tokio::test]
async fn backend_logs_filter_by_level_category_and_keyword() {
    let (db, _g) = test_db().await;
    db.append_backend_log("warn", "维护", "临时文件清理失败，RETRY")
        .await
        .unwrap();
    db.append_backend_log("error", "消息清理", "删除频道消息失败")
        .await
        .unwrap();
    db.append_backend_log("info", "维护", "临时文件已清空")
        .await
        .unwrap();

    let by_level = BackendLogFilter {
        level: Some("error".into()),
        ..Default::default()
    };
    let (rows, _) = db.backend_logs(50, None, &by_level).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category, "消息清理");

    let by_category = BackendLogFilter {
        category: Some("维护".into()),
        ..Default::default()
    };
    let (rows, _) = db.backend_logs(50, None, &by_category).await.unwrap();
    assert_eq!(rows.len(), 2);

    // 关键字同时匹配类别与内容,且大小写无关
    let by_keyword = BackendLogFilter {
        keyword: Some("retry".into()),
        ..Default::default()
    };
    let (rows, _) = db.backend_logs(50, None, &by_keyword).await.unwrap();
    assert_eq!(rows.len(), 1);

    // 关键字里的 LIKE 通配符必须当普通字符,不能命中全部
    let wildcard = BackendLogFilter {
        keyword: Some("%".into()),
        ..Default::default()
    };
    let (rows, _) = db.backend_logs(50, None, &wildcard).await.unwrap();
    assert!(rows.is_empty());

    // 非法级别视为不限制,避免前端传脏值时整页空白
    let bogus = BackendLogFilter {
        level: Some("verbose".into()),
        ..Default::default()
    };
    let (rows, _) = db.backend_logs(50, None, &bogus).await.unwrap();
    assert_eq!(rows.len(), 3);

    let facets = db.backend_log_facets().await.unwrap();
    assert_eq!(facets.categories, vec!["消息清理", "维护"]);
    assert_eq!(facets.level_counts.get("warn"), Some(&1));
    assert_eq!(facets.level_counts.get("error"), Some(&1));
    assert_eq!(facets.level_counts.get("debug"), None);
}

#[tokio::test]
async fn upload_lifecycle_and_refcount() {
    let (db, _g) = test_db().await;
    // 上传 2 块的文件
    let up = db.create_uploading(ROOT_ID, "a.bin").await.unwrap();
    assert!(db.uploading_child_exists(ROOT_ID, "a.bin").await.unwrap());
    db.add_chunk(up, 0, 10, "f0", 100, TEST_TARGET, Some("thumb-0"))
        .await
        .unwrap();
    db.add_chunk(up, 1, 5, "f1", 101, TEST_TARGET, None)
        .await
        .unwrap();
    // uploading 期间不可见
    assert!(db.resolve(&["a.bin"]).await.unwrap().is_none());
    let freed = db.finalize_upload(up, 15, Some("aabb")).await.unwrap();
    assert!(freed.is_empty());
    let node = db.resolve(&["a.bin"]).await.unwrap().unwrap();
    assert!(!db.uploading_child_exists(ROOT_ID, "a.bin").await.unwrap());
    assert_eq!(node.size, 15);
    assert_eq!(node.sha256.as_deref(), Some("aabb"));

    // 复制共享消息 → 删除原文件不释放消息
    db.copy_file(node.id, ROOT_ID, "b.bin").await.unwrap();
    let copied = db.resolve(&["b.bin"]).await.unwrap().unwrap();
    assert_eq!(
        db.chunks_of(copied.id).await.unwrap()[0]
            .tg_thumbnail_file_id
            .as_deref(),
        Some("thumb-0")
    );
    let freed = db.delete_node(node.id).await.unwrap();
    assert!(freed.is_empty());
    // 删除副本后消息释放
    let copy = db.resolve(&["b.bin"]).await.unwrap().unwrap();
    let mut freed = db.delete_node(copy.id).await.unwrap();
    freed.sort();
    assert_eq!(freed, vec![stored(100), stored(101)]);
}

#[tokio::test]
async fn overwrite_replaces_old() {
    let (db, _g) = test_db().await;
    let v1 = db.create_uploading(ROOT_ID, "f").await.unwrap();
    db.add_chunk(v1, 0, 3, "f0", 200, TEST_TARGET, None)
        .await
        .unwrap();
    db.finalize_upload(v1, 3, None).await.unwrap();

    let v2 = db.create_uploading(ROOT_ID, "f").await.unwrap();
    db.add_chunk(v2, 0, 7, "f1", 201, TEST_TARGET, None)
        .await
        .unwrap();
    let freed = db.finalize_upload(v2, 7, None).await.unwrap();
    assert_eq!(freed, vec![stored(200)]); // 旧版本消息被释放
    let node = db.resolve(&["f"]).await.unwrap().unwrap();
    assert_eq!(node.size, 7);
    assert_eq!(node.id, v2);
}

#[tokio::test]
async fn finalize_never_replaces_directory_created_during_upload() {
    let (db, _g) = test_db().await;
    let up = db.create_uploading(ROOT_ID, "x").await.unwrap();
    db.add_chunk(up, 0, 3, "f0", 300, TEST_TARGET, None)
        .await
        .unwrap();
    // 上传进行中另建同名目录并放入内容
    let dir = db.mkdir(ROOT_ID, "x").await.unwrap();
    db.mkdir(dir.id, "child").await.unwrap();

    let err = db.finalize_upload(up, 3, None).await.unwrap_err();
    assert!(matches!(err, AppError::IsDirectory));
    // 目录与子树原样保留，上传占位仍在，可按常规放弃流程回收分块
    let kept = db.resolve(&["x"]).await.unwrap().unwrap();
    assert!(kept.is_dir && kept.id == dir.id);
    assert!(db.resolve(&["x", "child"]).await.unwrap().is_some());
    assert_eq!(db.abort_upload(up).await.unwrap(), vec![stored(300)]);
}

#[tokio::test]
async fn inbox_source_survives_copy_and_is_removed_with_its_node() {
    let (db, _dir) = test_db().await;
    let node = db.create_uploading(ROOT_ID, "channel.pdf").await.unwrap();
    db.record_inbox_source(node, InboxSource::Channel)
        .await
        .unwrap();
    db.add_chunk(node, 0, 12, "inbox-file", 201, TEST_TARGET, None)
        .await
        .unwrap();
    db.finalize_upload(node, 12, None).await.unwrap();
    let copy = db.copy_file(node, ROOT_ID, "copy.pdf").await.unwrap();
    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT node_id,source FROM inbox_sources ORDER BY node_id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        sources,
        vec![(node, "channel".into()), (copy, "channel".into())]
    );
    db.abort_upload_keep_messages(node).await.unwrap();
    db.abort_upload_keep_messages(copy).await.unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inbox_sources")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(db.pending_delete_count().await.unwrap(), 0);
}

#[tokio::test]
async fn stale_upload_cleanup() {
    let (db, _g) = test_db().await;
    let up = db.create_uploading(ROOT_ID, "x").await.unwrap();
    db.add_chunk(up, 0, 1, "f", 300, TEST_TARGET, None)
        .await
        .unwrap();
    let freed = db.cleanup_stale_uploads().await.unwrap();
    assert_eq!(freed, vec![stored(300)]);
    assert!(db.node(up).await.is_err());
}

#[tokio::test]
async fn concurrent_uploads_to_same_path_are_rejected() {
    let (db, _g) = test_db().await;
    let (a, b) = tokio::join!(
        db.create_uploading(ROOT_ID, "same.bin"),
        db.create_uploading(ROOT_ID, "same.bin")
    );
    assert_ne!(a.is_ok(), b.is_ok(), "同一路径只能创建一个上传任务");
    let err = if let Err(e) = a { e } else { b.unwrap_err() };
    assert!(matches!(err, AppError::AlreadyExists));
}

/// 建一个 ready 文件的辅助
async fn mk_file(db: &Db, parent: i64, name: &str, msg: i64, sha: &str) -> i64 {
    let up = db.create_uploading(parent, name).await.unwrap();
    db.add_chunk(up, 0, 4, &format!("f{msg}"), msg, TEST_TARGET, None)
        .await
        .unwrap();
    db.finalize_upload(up, 4, Some(sha)).await.unwrap();
    up
}

#[tokio::test]
async fn full_path_walks_ancestors_in_one_query_and_hides_trashed_chains() {
    let (db, _g) = test_db().await;
    let a = db.mkdir(ROOT_ID, "a").await.unwrap();
    let b = db.mkdir(a.id, "b").await.unwrap();
    let f = mk_file(&db, b.id, "f.txt", 1, "sha").await;
    assert_eq!(
        db.full_path_if_visible(ROOT_ID).await.unwrap().as_deref(),
        Some("/")
    );
    assert_eq!(
        db.full_path_if_visible(f).await.unwrap().as_deref(),
        Some("/a/b/f.txt")
    );
    assert_eq!(
        db.full_path_if_visible(b.id).await.unwrap().as_deref(),
        Some("/a/b")
    );
    // 祖先进回收站后整条链不可见，自身仍是 ready 的子节点也一样
    db.trash_node(a.id, "/a").await.unwrap();
    assert_eq!(db.full_path_if_visible(f).await.unwrap(), None);
    assert_eq!(db.full_path_if_visible(a.id).await.unwrap(), None);
    assert!(matches!(
        db.full_path_if_visible(999_999).await,
        Err(AppError::NotFound)
    ));
}

#[tokio::test]
async fn node_usage_counts_directory_tree_without_downloading() {
    let (db, _g) = test_db().await;
    let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
    let sub = db.mkdir(docs.id, "sub").await.unwrap();
    let first = mk_file(&db, docs.id, "a.bin", 450, "a").await;
    mk_file(&db, sub.id, "b.bin", 451, "b").await;

    let usage = db.node_usage(docs.id).await.unwrap();
    assert_eq!(usage.file_count, 2);
    assert_eq!(usage.dir_count, 1);
    assert_eq!(usage.total_size, 8);
    assert_eq!(usage.chunk_count, 2);

    let file_usage = db.node_usage(first).await.unwrap();
    assert_eq!(file_usage.file_count, 1);
    assert_eq!(file_usage.dir_count, 0);
    assert_eq!(file_usage.total_size, 4);
    assert_eq!(file_usage.chunk_count, 1);
}

#[tokio::test]
async fn trash_lifecycle() {
    let (db, _g) = test_db().await;
    let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
    let fid = mk_file(&db, docs.id, "a.txt", 500, "s1").await;
    db.share_create(fid, None, None, None).await.unwrap();

    // 软删除目录:子树不可见,名字可重用,分享被清掉,消息不释放
    db.trash_node(docs.id, "/docs").await.unwrap();
    assert!(db.resolve(&["docs"]).await.unwrap().is_none());
    assert!(db.shares_list().await.unwrap().is_empty());
    db.mkdir(ROOT_ID, "docs").await.unwrap(); // 同名目录可再建
    let (files, _, _) = db.stats().await.unwrap();
    assert_eq!(files, 0, "回收站里的文件不计入统计");

    // 列表 + 恢复(原名被占 → 自动改名)
    let list = db.trash_list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].1, "/docs");
    let back = db.trash_restore(list[0].0.id).await.unwrap();
    assert_eq!(back, "/docs (恢复)");
    assert!(
        db.resolve(&["docs (恢复)", "a.txt"])
            .await
            .unwrap()
            .is_some()
    );

    // 再删一次后超期清理:消息引用归零被释放
    db.trash_node(
        db.resolve(&["docs (恢复)"]).await.unwrap().unwrap().id,
        "/docs (恢复)",
    )
    .await
    .unwrap();
    let freed = db.trash_purge_expired(0).await.unwrap();
    assert_eq!(freed, vec![stored(500)]);
    assert!(db.trash_list().await.unwrap().is_empty());
}

#[tokio::test]
async fn instant_link_hits_and_overwrites() {
    let (db, _g) = test_db().await;
    let src_id = mk_file(&db, ROOT_ID, "src.bin", 600, "deadbeef").await;
    sqlx::query("UPDATE chunks SET tg_thumbnail_file_id = 'instant-thumb' WHERE node_id = ?")
        .bind(src_id)
        .execute(&db.pool)
        .await
        .unwrap();

    // 对已经成功的同一路径重复探测必须是无副作用成功，分块不能被误删。
    let (same_id, freed) = db
        .instant_link("deadbeef", 4, ROOT_ID, "src.bin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(same_id, src_id);
    assert!(freed.is_empty());
    assert_eq!(db.chunks_of(src_id).await.unwrap().len(), 1);

    // 未命中
    assert!(
        db.instant_link("nope", 4, ROOT_ID, "x")
            .await
            .unwrap()
            .is_none()
    );
    // 命中:零流量链接,共享消息
    let (nid, freed) = db
        .instant_link("deadbeef", 4, ROOT_ID, "copy.bin")
        .await
        .unwrap()
        .unwrap();
    assert!(freed.is_empty());
    let n = db.node(nid).await.unwrap();
    assert_eq!(n.sha256.as_deref(), Some("deadbeef"));
    assert_eq!(
        db.chunks_of(nid).await.unwrap()[0]
            .tg_thumbnail_file_id
            .as_deref(),
        Some("instant-thumb")
    );
    // 删源文件不释放消息(副本还引用着)
    let src = db.resolve(&["src.bin"]).await.unwrap().unwrap();
    assert!(db.delete_node(src.id).await.unwrap().is_empty());

    // 秒传覆盖同名文件:旧消息释放
    mk_file(&db, ROOT_ID, "old.bin", 700, "cafe").await;
    let (_, freed) = db
        .instant_link("deadbeef", 4, ROOT_ID, "old.bin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(freed, vec![stored(700)]);
}

#[tokio::test]
async fn search_skips_trashed() {
    let (db, _g) = test_db().await;
    let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
    mk_file(&db, docs.id, "报告.pdf", 800, "s").await;
    mk_file(&db, ROOT_ID, "年度报告.pdf", 801, "s2").await;

    let hits = db.search("报告", 100).await.unwrap();
    let mut paths: Vec<_> = hits.iter().map(|(_, p)| p.clone()).collect();
    paths.sort();
    assert_eq!(paths, vec!["/docs/报告.pdf", "/年度报告.pdf"]);

    db.trash_node(docs.id, "/docs").await.unwrap();
    let hits = db.search("报告", 100).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "/年度报告.pdf");
}

#[tokio::test]
async fn search_under_limits_to_subtree() {
    let (db, _g) = test_db().await;
    // 目录名带 SQL/GLOB 元字符，验证前缀比较不会被当通配符
    let a = db.mkdir(ROOT_ID, "影视_100%[新]").await.unwrap();
    let sub = db.mkdir(a.id, "子集").await.unwrap();
    let b = db.mkdir(ROOT_ID, "影视x100y新z").await.unwrap();
    mk_file(&db, a.id, "x.mp4", 900, "a").await;
    mk_file(&db, sub.id, "y.mp4", 901, "b").await;
    mk_file(&db, b.id, "z.mp4", 902, "c").await;

    let filter = SearchFilter {
        extensions: vec!["mp4".into()],
        under: Some("/影视_100%[新]".into()),
        ..SearchFilter::default()
    };
    let mut paths: Vec<_> = db
        .search_filtered(&filter, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["/影视_100%[新]/x.mp4", "/影视_100%[新]/子集/y.mp4"]
    );

    // 根目录等价于不限制
    let all = SearchFilter {
        extensions: vec!["mp4".into()],
        under: Some("/".into()),
        ..SearchFilter::default()
    };
    assert_eq!(db.search_filtered(&all, 100).await.unwrap().len(), 3);

    // 不存在的目录返回空
    let none = SearchFilter {
        under: Some("/查无此目录".into()),
        ..SearchFilter::default()
    };
    assert!(db.search_filtered(&none, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn advanced_search_filters_and_favorites() {
    let (db, _g) = test_db().await;
    let docs = db.mkdir(ROOT_ID, "docs").await.unwrap();
    let report = mk_file(&db, docs.id, "季度报告.PDF", 820, "pdf").await;
    mk_file(&db, ROOT_ID, "录音.mp3", 821, "audio").await;
    db.set_favorite(report, true).await.unwrap();
    db.set_favorite(docs.id, true).await.unwrap();
    sqlx::query("UPDATE nodes SET modified_at = 2000 WHERE id = ?")
        .bind(report)
        .execute(&db.pool)
        .await
        .unwrap();

    let hits = db
        .search_filtered(
            &SearchFilter {
                query: Some("报告".into()),
                is_dir: Some(false),
                extensions: vec!["pdf".into()],
                min_size: Some(4),
                max_size: Some(4),
                modified_after: Some(1000),
                modified_before: Some(3000),
                favorite_only: true,
                under: None,
                sort: SearchSort::Modified,
                descending: true,
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "/docs/季度报告.PDF");
    assert!(hits[0].0.favorite);

    let favorites = db
        .search_filtered(
            &SearchFilter {
                favorite_only: true,
                ..SearchFilter::default()
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(favorites.len(), 2, "文件和目录都可以收藏");
}

#[tokio::test]
async fn resumable_upload_session_lifecycle() {
    let (db, _g) = test_db().await;
    let sha = "ab".repeat(32);
    let session = db
        .create_upload_session(ROOT_ID, "large.bin", "/large.bin", 10, &sha, 4, TEST_TARGET)
        .await
        .unwrap();
    assert_eq!(session.part_count(), 3);
    assert_eq!(session.expected_part_size(2), Some(2));
    let same = db
        .create_upload_session(
            ROOT_ID,
            "large.bin",
            "/large.bin",
            10,
            &sha,
            99,
            TEST_TARGET,
        )
        .await
        .unwrap();
    assert_eq!(same.session.id, session.session.id, "相同内容应恢复原会话");

    assert!(matches!(
        db.claim_upload_part(&session.session.id, 0).await.unwrap(),
        UploadPartClaim::Claimed(_)
    ));
    db.finish_upload_part(&session.session.id, 0, 4, "part-0", 900, TEST_TARGET, None)
        .await
        .unwrap();
    assert!(
        db.complete_upload_session(&session.session.id)
            .await
            .is_err()
    );
    assert!(db.cleanup_stale_uploads().await.unwrap().is_empty());

    for (idx, size) in [(1, 4), (2, 2)] {
        db.claim_upload_part(&session.session.id, idx)
            .await
            .unwrap();
        db.finish_upload_part(
            &session.session.id,
            idx,
            size,
            &format!("part-{idx}"),
            900 + idx,
            TEST_TARGET,
            None,
        )
        .await
        .unwrap();
    }
    let node = db
        .complete_upload_session(&session.session.id)
        .await
        .unwrap();
    assert_eq!(node.size, 10);
    assert_eq!(node.sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(db.chunks_of(node.id).await.unwrap().len(), 3);
    assert!(db.upload_session_info(&session.session.id).await.is_err());
}

#[tokio::test]
async fn storage_message_migration_switches_all_shared_references() {
    let (db, _g) = test_db().await;
    let old = db
        .ensure_storage_target("http://mock", "200:old", "-1001")
        .await
        .unwrap();
    let uploading = db.create_uploading(ROOT_ID, "old.bin").await.unwrap();
    db.add_chunk(uploading, 0, 4, "old-file", 55, old.id, Some("old-thumb"))
        .await
        .unwrap();
    db.finalize_upload(uploading, 4, Some("old-sha"))
        .await
        .unwrap();
    let copied = db
        .copy_file(uploading, ROOT_ID, "shared.bin")
        .await
        .unwrap();

    let sources = db.storage_migration_sources(TEST_TARGET).await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].messages, 1);
    assert_eq!(sources[0].chunks, 2);
    let candidate = db
        .storage_migration_candidate(TEST_TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.source_target_id, old.id);
    assert_eq!(candidate.source_message_id, 55);

    db.storage_migration_begin(TEST_TARGET, 1, 2).await.unwrap();
    let updated = db
        .finish_storage_message_migration(
            old.id,
            55,
            TEST_TARGET,
            "main-file",
            88,
            Some("main-thumb"),
        )
        .await
        .unwrap();
    assert_eq!(updated, 2);
    for node_id in [uploading, copied] {
        let chunk = db.chunks_of(node_id).await.unwrap().remove(0);
        assert_eq!(chunk.storage_target_id, Some(TEST_TARGET));
        assert_eq!(chunk.tg_message_id, 88);
        assert_eq!(chunk.tg_file_id, "main-file");
        assert_eq!(chunk.tg_thumbnail_file_id.as_deref(), Some("main-thumb"));
    }
    assert!(
        db.storage_migration_sources(TEST_TARGET)
            .await
            .unwrap()
            .is_empty()
    );
    let job = db.storage_migration_job().await.unwrap().unwrap();
    assert_eq!(job.migrated_messages, 1);
    assert_eq!(job.migrated_chunks, 2);
}

#[tokio::test]
async fn share_roundtrip() {
    let (db, _g) = test_db().await;
    let fid = mk_file(&db, ROOT_ID, "a.txt", 900, "sh").await;
    let s = db
        .share_create(fid, Some("1234"), Some(now_ts() + 100), Some(5))
        .await
        .unwrap();
    assert_eq!(s.token.len(), 12);
    let got = db.share_by_token(&s.token).await.unwrap().unwrap();
    assert_eq!(got.password.as_deref(), Some("1234"));
    assert!(
        db.share_claim_download(got.id, "ticket-a", fid, now_ts() + 100)
            .await
            .unwrap()
    );
    // 同一票据读取同一文件（例如多个 Range）不重复计数。
    assert!(
        db.share_claim_download(got.id, "ticket-a", fid, now_ts() + 100)
            .await
            .unwrap()
    );
    assert_eq!(
        db.share_by_token(&s.token)
            .await
            .unwrap()
            .unwrap()
            .downloads,
        1
    );

    // 并发票据也不能冲破下载上限。
    let limited = db.share_create(fid, None, None, Some(1)).await.unwrap();
    let (a, b) = tokio::join!(
        db.share_claim_download(limited.id, "ticket-1", fid, now_ts() + 100),
        db.share_claim_download(limited.id, "ticket-2", fid, now_ts() + 100),
    );
    let accepted = [a.unwrap(), b.unwrap()]
        .into_iter()
        .filter(|accepted| *accepted)
        .count();
    assert_eq!(accepted, 1);
    assert_eq!(
        db.share_by_token(&limited.token)
            .await
            .unwrap()
            .unwrap()
            .downloads,
        1
    );
    // 彻底删除节点 → 分享级联消失
    db.delete_node(fid).await.unwrap();
    assert!(db.share_by_token(&s.token).await.unwrap().is_none());
}
