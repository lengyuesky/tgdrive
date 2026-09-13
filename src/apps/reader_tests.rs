//! 阅读器能力只在临时数据库和内存媒体中验证。
use super::*;

async fn reader_app(state: &AppState) -> (InstalledApp, String) {
    let mut m = manifest();
    m.id = "books".into();
    m.api_version = 2;
    let app = add_app(state, &m, None).await;
    let launch = launch_app(state, "books").await;
    (app, launch["session"].as_str().unwrap().into())
}

#[tokio::test]
async fn reader_storage_is_scoped_conditional_and_survives_backup_uninstall() {
    let (state, media, dir) = fixture().await;
    let (app, session) = reader_app(&state).await;
    let call_data =
        |method: &'static str, params: Value| rpc_call(&state, "books", &session, method, params);
    let (status, first) = call_data(
        "storage.set",
        json!({"key":"progress:1","value":{"page":3},"expected_revision":null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let revision = first["revision"].as_str().unwrap();
    let (status, conflict) = call_data(
        "storage.set",
        json!({"key":"progress:1","value":{"page":0},"expected_revision":null}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "storage_conflict");
    assert_eq!(
        call_data(
            "storage.delete",
            json!({"key":"progress:1","expected_revision":"wrong"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, second) = call_data(
        "storage.set",
        json!({"key":"progress:1","value":{"page":7},"expected_revision":revision}),
    )
    .await;
    assert_ne!(second["revision"], first["revision"]);
    assert_eq!(
        call_data("storage.get", json!({"key":"progress:1","app_id":"shorts"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call_data(
            "storage.set",
            json!({"key":"large","value":"界".repeat(12000)})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call_data("storage.list", json!({"prefix":"progress:","limit":1}))
            .await
            .1["records"][0]["value"]["page"],
        7
    );
    let mut other = manifest();
    other.id = "comics".into();
    other.api_version = 2;
    add_app(&state, &other, None).await;
    let other_session = launch_app(&state, "comics").await;
    assert_eq!(
        rpc_call(
            &state,
            "comics",
            other_session["session"].as_str().unwrap(),
            "storage.get",
            json!({"key":"progress:1"})
        )
        .await
        .1,
        Value::Null
    );
    let restored_path = dir.path().join("restored.db");
    state
        .vfs
        .db
        .vacuum_into(restored_path.to_str().unwrap())
        .await
        .unwrap();
    let restored = state_at(&restored_path, media).await;
    assert_eq!(
        restored
            .vfs
            .db
            .app_data_get("books", "progress:1")
            .await
            .unwrap()
            .unwrap()
            .json()
            .unwrap()["value"]["page"],
        7
    );
    state.vfs.db.uninstall_app("books", false).await.unwrap();
    assert!(
        state
            .vfs
            .db
            .app_data_get("books", "progress:1")
            .await
            .unwrap()
            .is_some()
    );
    let (_, new_session) = reader_app(&state).await;
    assert!(
        state
            .vfs
            .db
            .app_data_write("books", &app.revision, "stale", Some("{}"), None)
            .await
            .is_err()
    );
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &new_session,
            "storage.get",
            json!({"key":"progress:1"})
        )
        .await
        .1["value"]["page"],
        7
    );
    state.vfs.db.uninstall_app("books", true).await.unwrap();
    assert!(
        state
            .vfs
            .db
            .app_data_get("books", "progress:1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .vfs
            .db
            .app_data_write("books", &app.revision, "late", Some("{}"), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reader_file_refs_preserve_rename_and_reject_hidden_replaced_or_invalid_inputs() {
    let (state, media, _dir) = fixture().await;
    let (_, session) = reader_app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    let id = seed_file(&state, &media, "/书/示例.txt", b"abcdef", 2).await;
    let (_, stat) = rpc_call(&state, "books", &session, "files.stat", json!({"id":id})).await;
    let version = stat["content_version"].as_str().unwrap();
    state
        .vfs
        .rename("/书/示例.txt", "/书/新名.txt")
        .await
        .unwrap();
    let (_, renamed) = rpc_call(&state, "books", &session, "files.stat", json!({"id":id})).await;
    assert_eq!(renamed["content_version"], version);
    assert_eq!(renamed["path"], "/书/新名.txt");
    let (status, grant) = rpc_call(
        &state,
        "books",
        &session,
        "media.url",
        json!({"id":id,"content_version":version}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(grant["url"].as_str().unwrap())
                .header(header::RANGE, "bytes=1-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        to_bytes(response.into_body(), 10).await.unwrap().as_ref(),
        b"bcde"
    );
    assert_eq!(*media.ranges.lock().unwrap(), [(1, 1), (0, 2), (0, 1)]);
    for params in [
        json!({"id":id,"path":"/书/新名.txt"}),
        json!({"id":-1}),
        json!({"path":"/../secret"}),
    ] {
        assert_eq!(
            rpc_call(&state, "books", &session, "files.stat", params)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "media.url",
            json!({"id":id,"content_version":"wrong"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    state.vfs.delete("/书").await.unwrap();
    assert_eq!(
        rpc_call(&state, "books", &session, "files.stat", json!({"id":id}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn reader_search_pages_beyond_2000_and_binds_scope_and_filters() {
    let (state, _, _dir) = fixture().await;
    let (_, session) = reader_app(&state).await;
    let folder = state.vfs.mkdir("/书库").await.unwrap();
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", state.rt.env.db_path))
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for i in 0..2010 {
        sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,0,0,'ready',1,1)")
            .bind(folder.id).bind(format!("书-{i:04}.txt")).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let mut cursor = Value::Null;
    let mut ids = std::collections::HashSet::new();
    loop {
        let (status, page) = rpc_call(
            &state,
            "books",
            &session,
            "files.searchPage",
            json!({"under":"/书库","extensions":["TXT"],"limit":500,"cursor":cursor}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for item in page["results"].as_array().unwrap() {
            assert!(ids.insert(item["id"].as_i64().unwrap()));
        }
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert_eq!(
            rpc_call(
                &state,
                "books",
                &session,
                "files.searchPage",
                json!({"under":"/","extensions":["txt"],"limit":500,"cursor":cursor})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(ids.len(), 2010);
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.list",
            json!({"path":"/书库","limit":501})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    state.vfs.delete("/书库").await.unwrap();
    assert!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.searchPage",
            json!({"under":"/","extensions":["txt"]})
        )
        .await
        .1["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reader_v2_methods_require_declared_api_and_permissions() {
    let (state, _, _dir) = fixture().await;
    // 内置短视频现在声明 v2；此用例必须显式构造 v1，不能随发行包版本漂移。
    let mut legacy = manifest();
    legacy.api_version = 1;
    add_app(&state, &legacy, None).await;
    let launch = launch_app(&state, "shorts").await;
    assert_eq!(
        rpc_call(
            &state,
            "shorts",
            launch["session"].as_str().unwrap(),
            "files.stat",
            json!({"path":"/"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut m = manifest();
    m.api_version = 2;
    m.id = "restricted".into();
    m.permissions.clear();
    add_app(&state, &m, None).await;
    let launch = launch_app(&state, "restricted").await;
    for (method, params) in [
        ("files.stat", json!({"path":"/"})),
        ("files.list", json!({"path":"/"})),
        ("files.searchPage", json!({})),
    ] {
        assert_eq!(
            rpc_call(
                &state,
                "restricted",
                launch["session"].as_str().unwrap(),
                method,
                params
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}
