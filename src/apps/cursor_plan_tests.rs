//! 真实深页游标的组合回归；计划采集限定本测试安装 generation，避免并发测试污染。
use super::cursor::{CursorData, binding};
use super::store::{Installation, NodeRow};
use super::tests::{authorize_all, fixture};
use super::*;
use std::future::Future;
use std::sync::atomic::Ordering;
use tgdrive_contract::{data::*, files::*, http::*};

fn cursor<T: serde::Serialize>(
    platform: &AppPlatform,
    app: &Installation,
    kind: &str,
    request: &T,
    row: &NodeRow,
    sort: NodeSort,
    scan: &str,
) -> String {
    platform
        .cursors
        .encode(&CursorData {
            kind: kind.into(),
            app: app.id.clone(),
            generation: app.generation.clone(),
            scope: Some(app.scope_revision.clone()),
            expires_at: Some(super::now().0 + PAGE_CURSOR_SECONDS as i64 * 1000),
            binding: binding(request).unwrap(),
            position: super::query::last_key(row, sort),
            scan: Some(scan.into()),
            epoch: None,
            high_water: None,
        })
        .unwrap()
}
async fn capture<T>(
    case: &str,
    generation: &str,
    table: &str,
    operation: impl Future<Output = PlatformResult<T>>,
    failures: &mut Vec<String>,
) {
    *super::query::CAPTURE_GENERATION.lock().unwrap() = Some(generation.into());
    super::query::PLANS.lock().unwrap().clear();
    super::query::CAPTURE_PLANS.store(true, Ordering::Relaxed);
    let result = operation.await;
    super::query::CAPTURE_PLANS.store(false, Ordering::Relaxed);
    let plans = std::mem::take(&mut *super::query::PLANS.lock().unwrap());
    if let Err(error) = result {
        failures.push(format!("{case} 请求失败：{error}"));
        return;
    }
    if plans.is_empty() {
        failures.push(format!("{case} 未采集实际 SQL"));
        return;
    }
    for (_, plan) in plans {
        let search = format!("SEARCH {table} ");
        let scan = format!("SCAN {table} ");
        if !plan.iter().any(|line| line.starts_with(&search))
            || plan.iter().any(|line| line.starts_with(&scan))
        {
            failures.push(format!("{case}：{}", plan.join("；")));
        }
    }
}

#[tokio::test]
async fn all_deep_cursor_combinations_search_main_indexes() {
    let _guard = super::query::PLAN_CAPTURE_LOCK.lock().await;
    let (_dir, platform, _) = fixture().await;
    let installed = authorize_all(&platform).await;
    let db = &platform.host.vfs.db;
    let root = db.mkdir(1, "matrix").await.unwrap();
    let mut folders = Vec::new();
    for index in 0..40 {
        folders.push(db.mkdir(root.id, &format!("part{index:03}")).await.unwrap());
    }
    sqlx::query("WITH RECURSIVE series(x) AS(SELECT 0 UNION ALL SELECT x+1 FROM series WHERE x<19999) INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) SELECT ?+(x%40),printf('item%05d.%s',x/40,CASE WHEN x%2=0 THEN 'jpg' ELSE 'bin' END),0,x%100,'ready',1700000000+x/40,1700000000+x/20 FROM series")
        .bind(folders[0].id).execute(&db.pool).await.unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    let app = store::installation(&mut connection, &installed.manifest.id)
        .await
        .unwrap();
    let deep = store::node_row(&mut connection, folders.last().unwrap().id + 1 + 9998)
        .await
        .unwrap();
    let directory = store::node_row(&mut connection, folders[20].id)
        .await
        .unwrap();
    let directory_bin = store::node_row(&mut connection, folders[21].id)
        .await
        .unwrap();
    let scan = super::query::checkpoint(&mut connection, &platform.cursors, &app)
        .await
        .unwrap();
    drop(connection);
    let sorts = [
        NodeSort::Name,
        NodeSort::Size,
        NodeSort::CreatedAt,
        NodeSort::ModifiedAt,
        NodeSort::Id,
    ];
    let directions = [SortOrder::Asc, SortOrder::Desc];
    let mut failures = Vec::new();
    let mut cases = 0;
    for sort in sorts {
        for direction in directions {
            for explicit in [false, true] {
                for suffix in [false, true] {
                    let mut query: FileQuery =
                        serde_json::from_value(serde_json::json!({"limit":20})).unwrap();
                    query.sort = sort;
                    query.order = direction;
                    if explicit {
                        query.roots = vec![root.id.to_string()];
                    }
                    if suffix {
                        query.filter.extensions = vec!["jpg".into()];
                    }
                    query.cursor =
                        Some(cursor(&platform, &app, "query", &query, &deep, sort, &scan));
                    // 主机用中段节点定位后，测试真正的生产分页所签发的下一页游标。
                    query.cursor = platform
                        .file_query(&app.id, &app.generation, query.clone())
                        .await
                        .unwrap()
                        .next_cursor;
                    assert!(query.cursor.is_some(), "中段查询必须能产生真实续页游标");
                    capture(
                        &format!("query {sort:?} {direction:?} roots={explicit} suffix={suffix}"),
                        &app.generation,
                        "n",
                        platform.file_query(&app.id, &app.generation, query),
                        &mut failures,
                    )
                    .await;
                    cases += 1;
                }
            }
        }
    }
    let parent = deep.parent_id.unwrap();
    let parent_node = db.node(parent).await.unwrap();
    for sort in sorts {
        for direction in directions {
            for by_path in [false, true] {
                // list 只接受单个目录，不存在 roots/后缀参数；两种目录引用都规范化成相同 FileQuery。
                let normalized = FileQuery {
                    roots: vec![parent.to_string()],
                    recursive: false,
                    filter: FileFilter {
                        kind: QueryKind::All,
                        include_hidden: true,
                        ..FileFilter::default()
                    },
                    sort,
                    order: direction,
                    limit: 20,
                    cursor: None,
                };
                let token = cursor(&platform, &app, "query", &normalized, &deep, sort, &scan);
                let reference = if by_path {
                    NodeReference::Path {
                        path: format!("/matrix/{}", parent_node.name),
                        content_version: None,
                    }
                } else {
                    NodeReference::Id {
                        id: parent.to_string(),
                        content_version: None,
                    }
                };
                let mut request = ListDirectoryRequest {
                    directory: reference,
                    sort,
                    order: direction,
                    limit: 20,
                    cursor: Some(token),
                };
                request.cursor = platform
                    .list_directory(&app.id, &app.generation, request.clone())
                    .await
                    .unwrap()
                    .next_cursor;
                assert!(
                    request.cursor.is_some(),
                    "中段直属列表必须能产生真实续页游标"
                );
                capture(
                    &format!("list {sort:?} {direction:?} path={by_path}"),
                    &app.generation,
                    "n",
                    platform.list_directory(&app.id, &app.generation, request),
                    &mut failures,
                )
                .await;
                cases += 1;
            }
        }
    }
    for sort in [
        DirectorySort::Name,
        DirectorySort::CreatedAt,
        DirectorySort::ModifiedAt,
        DirectorySort::Id,
    ] {
        for direction in directions {
            for explicit in [false, true] {
                for extension in ["jpg", "bin"] {
                    let mut request: DirectoriesWithFilesQuery = serde_json::from_value(
                        serde_json::json!({"file_filter":{"extensions":[extension]},"limit":2}),
                    )
                    .unwrap();
                    request.sort = sort;
                    request.order = direction;
                    if explicit {
                        request.roots = vec![root.id.to_string()];
                    }
                    let node_sort = match sort {
                        DirectorySort::Name => NodeSort::Name,
                        DirectorySort::CreatedAt => NodeSort::CreatedAt,
                        DirectorySort::ModifiedAt => NodeSort::ModifiedAt,
                        DirectorySort::Id => NodeSort::Id,
                    };
                    request.cursor = Some(cursor(
                        &platform,
                        &app,
                        "directories",
                        &request,
                        if extension == "jpg" {
                            &directory
                        } else {
                            &directory_bin
                        },
                        node_sort,
                        &scan,
                    ));
                    request.cursor = platform
                        .directories_with_files(&app.id, &app.generation, request.clone())
                        .await
                        .unwrap()
                        .next_cursor;
                    assert!(
                        request.cursor.is_some(),
                        "中段目录匹配必须能产生真实续页游标"
                    );
                    capture(&format!("directories {sort:?} {direction:?} roots={explicit} extension={extension}"),&app.generation,"n",platform.directories_with_files(&app.id,&app.generation,request),&mut failures).await;
                    cases += 1;
                }
            }
        }
    }
    let invalid: DirectoriesWithFilesQuery =
        serde_json::from_value(serde_json::json!({"file_filter":{},"limit":10})).unwrap();
    assert!(
        platform
            .directories_with_files(&app.id, &app.generation, invalid)
            .await
            .is_err(),
        "直属匹配目录必须提供非空后缀，不能伪造无后缀续页"
    );
    // 再覆盖 query 自身的直属模式：默认授权根与显式根指向同一文件目录。
    store::update_scope(
        db,
        &app.id,
        tgdrive_contract::runtime::ScopeUpdateRequest {
            expected_revision: app.scope_revision.clone(),
            scope: DirectoryScope::Selected {
                directories: vec![DirectoryGrant {
                    directory_id: parent.to_string(),
                    access: AccessLevel::ReadOnly,
                }],
            },
        },
    )
    .await
    .unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    let narrow = store::installation(&mut connection, &app.id).await.unwrap();
    let direct_scan = super::query::checkpoint(&mut connection, &platform.cursors, &narrow)
        .await
        .unwrap();
    drop(connection);
    for sort in sorts {
        for direction in directions {
            for explicit in [false, true] {
                for suffix in [false, true] {
                    let mut request: FileQuery =
                        serde_json::from_value(serde_json::json!({"recursive":false,"limit":20}))
                            .unwrap();
                    request.sort = sort;
                    request.order = direction;
                    if explicit {
                        request.roots = vec![parent.to_string()];
                    }
                    if suffix {
                        request.filter.extensions = vec!["jpg".into()];
                    }
                    request.cursor = Some(cursor(
                        &platform,
                        &narrow,
                        "query",
                        &request,
                        &deep,
                        sort,
                        &direct_scan,
                    ));
                    request.cursor = platform
                        .file_query(&narrow.id, &narrow.generation, request.clone())
                        .await
                        .unwrap()
                        .next_cursor;
                    assert!(request.cursor.is_some());
                    capture(
                        &format!(
                            "query direct {sort:?} {direction:?} roots={explicit} suffix={suffix}"
                        ),
                        &narrow.generation,
                        "n",
                        platform.file_query(&narrow.id, &narrow.generation, request),
                        &mut failures,
                    )
                    .await;
                    cases += 1;
                }
            }
        }
    }
    let initial = platform
        .file_changes(
            &app.id,
            &app.generation,
            FileChangesRequest {
                cursor: None,
                limit: 200,
            },
        )
        .await
        .unwrap();
    sqlx::query("WITH RECURSIVE series(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM series WHERE x<5000) INSERT INTO app_file_changes(occurred_at,node_id,is_dir,structural,previous_ancestors,previous_path) SELECT ?,?,0,0,'[1]',NULL FROM series")
        .bind(super::now().0).bind(deep.id).execute(&db.pool).await.unwrap();
    let mut change_cursor = initial.next_cursor;
    for _ in 0..10 {
        change_cursor = platform
            .file_changes(
                &app.id,
                &app.generation,
                FileChangesRequest {
                    cursor: Some(change_cursor),
                    limit: 200,
                },
            )
            .await
            .unwrap()
            .next_cursor;
    }
    capture(
        "changes checkpoint after 2000 committed rows",
        &app.generation,
        "app_file_changes",
        platform.file_changes(
            &app.id,
            &app.generation,
            FileChangesRequest {
                cursor: Some(change_cursor),
                limit: 200,
            },
        ),
        &mut failures,
    )
    .await;
    cases += 1;
    sqlx::query("WITH RECURSIVE series(x) AS(SELECT 0 UNION ALL SELECT x+1 FROM series WHERE x<9999) INSERT INTO app_kv(app_id,key,value,revision,updated_at,bytes) SELECT ?,printf('%s%05d',CASE WHEN x<5000 THEN 'prefix-' ELSE 'z-other-' END,x),'0',printf('revision-%d',x),?,20 FROM series")
        .bind(&app.id).bind(super::now().0).execute(&db.pool).await.unwrap();
    for prefix in ["", "prefix-"] {
        let mut request = KvListRequest {
            prefix: prefix.into(),
            limit: 100,
            cursor: None,
        };
        for _ in 0..20 {
            request.cursor = platform
                .kv_list(&app.id, &app.generation, request.clone())
                .await
                .unwrap()
                .next_cursor;
        }
        capture(
            &format!("kv/list prefix={prefix}"),
            &app.generation,
            "app_kv",
            platform.kv_list(&app.id, &app.generation, request),
            &mut failures,
        )
        .await;
        cases += 1;
    }
    assert_eq!(cases, 135);
    assert!(
        failures.is_empty(),
        "深页索引回归失败：\n{}",
        failures.join("\n")
    );
}
