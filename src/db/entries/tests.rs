use super::*;
use crate::db::ROOT_ID;
use sqlx::{AssertSqlSafe, Execute as _, Row};
// 动态测试 SQL 仅由测试常量、排序白名单或生产 QueryBuilder 生成，不插入用户输入。
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn fixture() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("entries.db").to_str().unwrap())
        .await
        .unwrap();
    (db, dir)
}

async fn populate(db: &Db, count: i64) {
    sqlx::query("WITH RECURSIVE seq(n) AS (SELECT 1 WHERE ? > 0 UNION ALL SELECT n + 1 FROM seq WHERE n < ?) INSERT INTO nodes(parent_id, name, is_dir, size, created_at, modified_at) SELECT 1, printf('文件%06d', n), 0, 7, 1, 13 FROM seq")
        .bind(count).bind(count).execute(&db.pool).await.unwrap();
}

async fn collect(db: &Db, options: &Options) -> Vec<i64> {
    let mut cursor = None;
    let mut ids = Vec::new();
    let mut total = None;
    loop {
        let page = db
            .entries_page(ROOT_ID, options, cursor.as_deref())
            .await
            .unwrap();
        if cursor.is_none() {
            total = page.total;
        } else {
            assert_eq!(page.total, None, "续批不得重复 COUNT");
        }
        assert!(page.nodes.len() <= usize::from(options.limit));
        ids.extend(page.nodes.iter().map(|node| node.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(!page.nodes.is_empty());
    }
    assert_eq!(total, Some(ids.len() as i64));
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    ids
}

#[tokio::test]
async fn entries_page_boundaries_empty_199_200_201() {
    for count in [0, 199, 200, 201] {
        let (db, _dir) = fixture().await;
        populate(&db, count).await;
        let page = db
            .entries_page(ROOT_ID, &Options::default(), None)
            .await
            .unwrap();
        assert_eq!(page.total, Some(count));
        assert_eq!(page.nodes.len(), count.min(200) as usize);
        assert_eq!(page.next_cursor.is_some(), count > 200);
        assert_eq!(
            collect(&db, &Options::default()).await.len(),
            count as usize
        );
        if count == 201 {
            let filtered = db
                .entries_page(
                    1,
                    &Options {
                        q: "文件000201".into(),
                        ..Options::default()
                    },
                    None,
                )
                .await
                .unwrap();
            assert_eq!(filtered.total, Some(1));
            assert_eq!(filtered.nodes[0].name, "文件000201");
        }
    }
}

#[tokio::test]
async fn entries_all_sorts_directions_kinds_literal_queries_and_ready() {
    let (db, _dir) = fixture().await;
    for (index, name) in [
        "a", "A", "a_1", "A%1", "中", "中文", "国", "é", "É", "file", "FILE",
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,created_at,modified_at) VALUES(1,?,?,?,?,?)")
            .bind(name).bind(index % 3 == 0).bind((index % 2) as i64).bind(1_i64).bind((index % 4) as i64)
            .execute(&db.pool).await.unwrap();
    }
    sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,status,size,created_at,modified_at) VALUES(1,'隐藏上传',0,'uploading',0,1,1),(1,'隐藏回收',1,'trashed',0,1,1)")
        .execute(&db.pool).await.unwrap();
    for sort in [Sort::Name, Sort::Size, Sort::Mtime] {
        for order in [Order::Asc, Order::Desc] {
            for kind in [Kind::All, Kind::Dir, Kind::File] {
                for q in ["", "a", "%", "_", "中", "é", "不存在"] {
                    let options = Options {
                        limit: 1,
                        sort,
                        order,
                        kind,
                        q: q.into(),
                    };
                    // 独立全量排序作为静态目录完整性参照；生产接口不使用这种查询。
                    let columns = options
                        .columns()
                        .iter()
                        .map(|column| {
                            format!(
                                "{column} {}",
                                if order == Order::Asc { "ASC" } else { "DESC" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let sql = format!(
                        "SELECT id FROM nodes WHERE parent_id=1 AND status='ready' AND (? IS NULL OR is_dir=?) AND instr(lower(name),lower(?))>0 ORDER BY is_dir DESC, {columns}"
                    );
                    let is_dir = match kind {
                        Kind::All => None,
                        Kind::Dir => Some(true),
                        Kind::File => Some(false),
                    };
                    let expected: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(sql))
                        .bind(is_dir)
                        .bind(is_dir)
                        .bind(q)
                        .fetch_all(&db.pool)
                        .await
                        .unwrap();
                    assert_eq!(collect(&db, &options).await, expected, "{options:?}");
                }
            }
        }
    }
}

#[tokio::test]
async fn entries_cursor_rejects_invalid_and_reused_conditions() {
    let (db, _dir) = fixture().await;
    populate(&db, 201).await;
    let options = Options::default();
    let raw = db
        .entries_page(1, &options, None)
        .await
        .unwrap()
        .next_cursor
        .unwrap();
    let cursor_json: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&raw).unwrap()).unwrap();
    for raw in [
        "".into(),
        "not-base64!".into(),
        "a".repeat(4097),
        URL_SAFE_NO_PAD.encode(b"{}"),
        URL_SAFE_NO_PAD.encode(b"null"),
    ] {
        assert!(db.entries_page(1, &options, Some(&raw)).await.is_err());
    }
    for (path, replacement) in [
        ("/version", serde_json::json!(2)),
        ("/directory_id", serde_json::json!(2)),
        ("/directory_id", serde_json::json!("1")),
        ("/last/id", serde_json::json!(-1)),
        ("/last/id", serde_json::json!(1.5)),
        ("/last/id", serde_json::json!(18446744073709551615_u64)),
        ("/last/name", serde_json::json!("../a")),
        ("/last/name", serde_json::json!("x".repeat(256))),
        ("/last/name", serde_json::json!("a\u{0}")),
        ("/last/name", serde_json::json!(null)),
        ("/last/is_dir", serde_json::json!("false")),
        ("/last/value", serde_json::json!(1)),
    ] {
        let mut bad = cursor_json.clone();
        *bad.pointer_mut(path).unwrap() = replacement;
        let raw = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&bad).unwrap());
        assert!(
            db.entries_page(1, &options, Some(&raw)).await.is_err(),
            "{path}"
        );
    }
    let mut unknown = cursor_json.clone();
    unknown["extra"] = true.into();
    assert!(
        Cursor::decode(
            &URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unknown).unwrap()),
            1,
            &options
        )
        .is_err()
    );
    assert!(db.entries_page(2, &options, Some(&raw)).await.is_err());
    for changed in [
        Options {
            limit: 199,
            ..options.clone()
        },
        Options {
            sort: Sort::Size,
            ..options.clone()
        },
        Options {
            order: Order::Desc,
            ..options.clone()
        },
        Options {
            kind: Kind::File,
            ..options.clone()
        },
        Options {
            q: "文".into(),
            ..options.clone()
        },
    ] {
        assert!(db.entries_page(1, &changed, Some(&raw)).await.is_err());
    }
    for limit in [0, 501] {
        assert!(
            db.entries_page(
                1,
                &Options {
                    limit,
                    ..options.clone()
                },
                None
            )
            .await
            .is_err()
        );
    }
    assert!(
        db.entries_page(
            1,
            &Options {
                q: "x".repeat(256),
                ..options
            },
            None
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn entries_thumbnail_batch_and_first_chunk_only() {
    let (db, _dir) = fixture().await;
    populate(&db, 3).await;
    sqlx::query("INSERT INTO chunks(node_id,idx,size,tg_file_id,tg_message_id,tg_thumbnail_file_id) VALUES(2,0,1,'first',1,'thumb'),(2,1,1,'second',2,NULL),(3,1,1,'not-first',3,'ignored'),(4,0,1,'plain',4,NULL)")
        .execute(&db.pool).await.unwrap();
    let page = db.entries_page(1, &Options::default(), None).await.unwrap();
    assert_eq!(page.thumbnails, HashSet::from([2]));
    assert_eq!(
        db.first_chunk(2).await.unwrap().unwrap().tg_file_id,
        "first"
    );
    assert!(db.first_chunk(3).await.unwrap().is_none());
    assert!(db.first_chunk(999).await.unwrap().is_none());
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM chunks WHERE node_id = 2 AND idx = 0",
        "EXPLAIN QUERY PLAN SELECT node_id FROM chunks WHERE idx = 0 AND tg_thumbnail_file_id IS NOT NULL AND node_id IN (2,3,4)",
    ] {
        let rows = sqlx::query(sql).fetch_all(&db.pool).await.unwrap();
        for row in rows {
            let detail: String = row.get("detail");
            println!("首块计划: {detail}");
            assert!(detail.contains("node_id=? AND idx=?"));
        }
    }
}

#[tokio::test]
async fn entries_100k_deep_cursor_seeks_every_tail_key() {
    let (db, _dir) = fixture().await;
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    println!("SQLite 版本: {version}");
    let started = std::time::Instant::now();
    populate(&db, 100_000).await;
    // 所有 name 的 NOCASE 值相同，size/mtime 也相同：专门击穿仅首排序键 seek 的伪优化。
    let expression = (0..17)
        .map(|bit| {
            format!(
                "CASE WHEN ((id-2) & {}) = 0 THEN 'a' ELSE 'A' END",
                1 << bit
            )
        })
        .collect::<Vec<_>>()
        .join(" || ");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE nodes SET name = {expression} WHERE parent_id = 1"
    )))
    .execute(&db.pool)
    .await
    .unwrap();
    println!("合成 100000 条同键目录耗时 {:?}", started.elapsed());
    for sort in [Sort::Name, Sort::Size, Sort::Mtime] {
        for order in [Order::Asc, Order::Desc] {
            let options = Options {
                sort,
                order,
                kind: Kind::File,
                ..Options::default()
            };
            let columns = options.columns();
            let direction = if order == Order::Asc { "ASC" } else { "DESC" };
            let ordering = columns
                .iter()
                .map(|column| format!("{column} {direction}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut work = Vec::new();
            for depth in [200, 90_000] {
                // OFFSET 仅在测试中选取参照末键，实际被测 SQL 全部来自生产 range_query。
                let last: Node = sqlx::query_as(AssertSqlSafe(format!(
                    "SELECT * FROM nodes WHERE parent_id=1 ORDER BY {ordering} LIMIT 1 OFFSET {}",
                    depth - 1
                )))
                .fetch_one(&db.pool)
                .await
                .unwrap();
                let expected: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(format!("SELECT id FROM nodes WHERE parent_id=1 ORDER BY {ordering} LIMIT 200 OFFSET {depth}"))).fetch_all(&db.pool).await.unwrap();
                let raw = Cursor::encode(1, &options, &last).unwrap();
                let page_started = std::time::Instant::now();
                let page = db.entries_page(1, &options, Some(&raw)).await.unwrap();
                println!(
                    "{sort:?}/{order:?} 深度 {depth} 完整续批耗时 {:?}",
                    page_started.elapsed()
                );
                assert_eq!(
                    page.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(page.total, None);
                let key = Key::from_node(&last, sort);
                let mut connection = db.pool.acquire().await.unwrap();
                for range in (0..columns.len()).rev() {
                    let mut query = range_query(1, false, &options, Some((&key, range)), 201);
                    let sql = query.sql().as_str().to_string();
                    let args = query.build().take_arguments().unwrap().unwrap();
                    let plan =
                        sqlx::query_with(AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")), args)
                            .fetch_all(&mut *connection)
                            .await
                            .unwrap();
                    for row in plan {
                        let detail: String = row.get("detail");
                        println!("{sort:?}/{order:?} 深度 {depth} 尾键 {range}: {detail}");
                        assert!(detail.contains(options.index()), "{detail}");
                        assert!(!detail.contains("TEMP B-TREE"), "{detail}");
                        assert!(
                            detail.contains(if order == Order::Asc { ">?" } else { "<?" }),
                            "必须包含实际范围 seek: {detail}"
                        );
                    }
                }
                let ops = Arc::new(AtomicUsize::new(0));
                let counted = ops.clone();
                connection
                    .lock_handle()
                    .await
                    .unwrap()
                    .set_progress_handler(1, move || {
                        counted.fetch_add(1, Ordering::Relaxed);
                        true
                    });
                let mut fetched = Vec::new();
                for range in (0..columns.len()).rev() {
                    if fetched.len() == 201 {
                        break;
                    }
                    let mut query =
                        range_query(1, false, &options, Some((&key, range)), 201 - fetched.len());
                    fetched.extend(
                        query
                            .build_query_as::<Node>()
                            .fetch_all(&mut *connection)
                            .await
                            .unwrap(),
                    );
                }
                connection
                    .lock_handle()
                    .await
                    .unwrap()
                    .remove_progress_handler();
                let count = ops.load(Ordering::Relaxed);
                println!(
                    "{sort:?}/{order:?} 深度 {depth}: {} 条、{count} VM 指令",
                    fetched.len()
                );
                assert_eq!(fetched.len(), 201);
                assert!(count < 20_000, "续批不得扫描前方数万条: {count}");
                work.push(count);
            }
            assert!(
                work[1] <= work[0] * 2 + 100,
                "深度增长不应线性放大扫描: {work:?}"
            );
        }
    }
}
