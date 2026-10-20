//! 阅读索引的真实 RPC、迁移触发器和权限验收；不访问真实网盘。
use super::*;

async fn app(state: &AppState) -> String {
    let mut m = manifest();
    m.id = "books".into();
    m.api_version = 2;
    add_app(state, &m, None).await;
    launch_app(state, "books").await["session"]
        .as_str()
        .unwrap()
        .into()
}
async fn page(state: &AppState, session: &str, params: Value) -> (StatusCode, Value) {
    rpc_call(state, "books", session, "library.page", params).await
}

#[tokio::test]
async fn reading_index_recurses_deduplicates_and_rejects_stale_cursor() {
    let (state, media, _dir) = fixture().await;
    let session = app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    state.vfs.mkdir("/书/内").await.unwrap();
    let root = state.vfs.resolve("/书").await.unwrap().id;
    let child = state.vfs.resolve("/书/内").await.unwrap().id;
    let a = seed_file(&state, &media, "/书/A.txt", b"a", 2).await;
    let b = seed_file(&state, &media, "/书/内/b.EPUB", b"b", 2).await;
    seed_file(&state, &media, "/书/movie.mp4", b"v", 2).await;
    let params = json!({"roots":[root,child],"kind":"books","limit":1});
    let (status, first) = page(&state, &session, params.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["entries"][0]["file"]["id"], a);
    let mut next = params.clone();
    next["cursor"] = first["next_cursor"].clone();
    let (status, second) = page(&state, &session, next.clone()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["entries"][0]["file"]["id"], b);
    assert_eq!(second["entries"][0]["source_ids"], json!([root, child]));
    assert_eq!(second["has_more"], false);
    state.vfs.db.move_node(b, child, "c.pdf").await.unwrap();
    let (status, stale) = page(&state, &session, next).await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["code"], "reading_index_changed");
    let (_, refreshed) = page(
        &state,
        &session,
        json!({"roots":[root],"kind":"books","format":"pdf"}),
    )
    .await;
    assert_eq!(refreshed["entries"][0]["file"]["name"], "c.pdf");
    state.vfs.db.trash_node(b, "/书/内/c.pdf").await.unwrap();
    assert!(
        page(
            &state,
            &session,
            json!({"roots":[root],"kind":"books","format":"pdf"})
        )
        .await
        .1["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    state.vfs.db.trash_restore(b).await.unwrap();
    assert_eq!(
        page(
            &state,
            &session,
            json!({"roots":[root],"kind":"books","format":"pdf"})
        )
        .await
        .1["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn reading_index_images_and_permissions_and_filters() {
    let (state, media, _dir) = fixture().await;
    let session = app(&state).await;
    for path in ["/漫画", "/漫画/卷一", "/秘密"] {
        state.vfs.mkdir(path).await.unwrap();
    }
    let root = state.vfs.resolve("/漫画").await.unwrap().id;
    let volume = state.vfs.resolve("/漫画/卷一").await.unwrap().id;
    seed_file(&state, &media, "/漫画/卷一/01.JPG", b"i", 2).await;
    seed_file(&state, &media, "/漫画/卷一/02.png", b"i", 2).await;
    seed_file(&state, &media, "/漫画/.hidden.jpg", b"i", 2).await;
    seed_file(&state, &media, "/漫画/a.cbz", b"a", 2).await;
    seed_file(&state, &media, "/秘密/secret.pdf", b"s", 2).await;
    let (_, result) = page(
        &state,
        &session,
        json!({"roots":[root],"kind":"comics","format":"images"}),
    )
    .await;
    assert_eq!(result["entries"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(result["entries"][0]["file"]["id"], volume);
    let (_, empty) = page(
        &state,
        &session,
        json!({"roots":[root],"kind":"comics","q":"%' OR 1=1 --"}),
    )
    .await;
    assert_eq!(empty["entries"], json!([]));
    call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"mode":"selected","paths":["/漫画"]}),
    )
    .await;
    assert_eq!(
        page(&state, &session, json!({"roots":[1],"kind":"books"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        page(&state, &session, json!({"roots":[root],"kind":"comics"}))
            .await
            .0,
        StatusCode::OK
    );
    for invalid in [
        json!({"roots":[],"kind":"books"}),
        json!({"roots":[root],"kind":"books","format":"images"}),
        json!({"roots":[root],"kind":"books","limit":201}),
        json!({"roots":[root],"kind":"books","cursor":"bad"}),
    ] {
        assert_eq!(
            page(&state, &session, invalid).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn reading_index_pages_beyond_2000_without_loading_full_rows() {
    let (state, media, _dir) = fixture().await;
    let session = app(&state).await;
    let root = state.vfs.db.mkdir(1, "大库").await.unwrap().id;
    // 合成内存文件，索引分页本身不读取正文。
    for id in 0..2005 {
        seed_file(&state, &media, &format!("/大库/书{id:04}.txt"), b"x", 2).await;
    }
    let reads_before = media.ranges.lock().unwrap().len();
    let mut cursor = Value::Null;
    let mut ids = std::collections::HashSet::new();
    loop {
        let (status, result) = page(
            &state,
            &session,
            json!({"roots":[root],"kind":"books","limit":200,"cursor":cursor}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let entries = result["entries"].as_array().unwrap();
        assert!(entries.len() <= 200);
        for entry in entries {
            assert!(ids.insert(entry["file"]["id"].as_i64().unwrap()));
        }
        if result["has_more"] == false {
            break;
        }
        cursor = result["next_cursor"].clone();
    }
    assert_eq!(ids.len(), 2005);
    assert_eq!(media.ranges.lock().unwrap().len(), reads_before);
}

#[tokio::test]
async fn unified_reading_index_pages_all_formats_and_binds_kind_and_permissions() {
    let (state, media, _dir) = fixture().await;
    let mut manifest = manifest();
    manifest.id = "reader".into();
    manifest.api_version = 2;
    add_app(&state, &manifest, None).await;
    let session = launch_app(&state, "reader").await["session"]
        .as_str()
        .unwrap()
        .to_owned();
    for path in ["/混合", "/混合/卷话", "/混合/卷话/图片", "/秘密"] {
        state.vfs.mkdir(path).await.unwrap();
    }
    let root = state.vfs.resolve("/混合").await.unwrap().id;
    let child = state.vfs.resolve("/混合/卷话").await.unwrap().id;
    let pictures = state.vfs.resolve("/混合/卷话/图片").await.unwrap().id;
    let mut expected = std::collections::HashSet::new();
    for name in ["A.txt", "a.EPUB", "B.pdf", "b.CBZ", "C.zip"] {
        expected.insert(seed_file(&state, &media, &format!("/混合/卷话/{name}"), b"x", 2).await);
    }
    expected.insert(pictures);
    for name in ["1.jpg", "2.PNG"] {
        seed_file(&state, &media, &format!("/混合/卷话/图片/{name}"), b"x", 2).await;
    }
    seed_file(&state, &media, "/混合/视频.mp4", b"x", 2).await;
    seed_file(&state, &media, "/秘密/保密.pdf", b"x", 2).await;
    let reads_before = media.ranges.lock().unwrap().len();
    let base = json!({"roots":[root,child],"kind":"all","limit":2});
    let mut params = base.clone();
    let mut actual = std::collections::HashSet::new();
    let mut formats = std::collections::HashSet::new();
    let mut first_cursor = Value::Null;
    loop {
        let (status, result) =
            rpc_call(&state, "reader", &session, "library.page", params.clone()).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        for entry in result["entries"].as_array().unwrap() {
            assert!(
                actual.insert(entry["file"]["id"].as_i64().unwrap()),
                "不能因重叠来源重复返回"
            );
            formats.insert(entry["format"].as_str().unwrap().to_owned());
            assert_eq!(entry["source_ids"], json!([root, child]));
        }
        if result["has_more"] == false {
            break;
        }
        if first_cursor.is_null() {
            first_cursor = result["next_cursor"].clone();
        }
        params["cursor"] = result["next_cursor"].clone();
    }
    assert_eq!(actual, expected);
    assert_eq!(
        formats,
        ["txt", "epub", "pdf", "cbz", "zip", "images"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert_eq!(media.ranges.lock().unwrap().len(), reads_before);
    for format in ["txt", "epub", "pdf", "cbz", "zip", "images"] {
        let (_, result) = rpc_call(&state, "reader", &session, "library.page",
            json!({"roots":[root],"kind":"all","format":format,"q":if format == "images" { "图片" } else { "." }})).await;
        assert_eq!(result["entries"].as_array().unwrap().len(), 1, "{result}");
    }
    let mut wrong_kind = base.clone();
    wrong_kind["kind"] = json!("books");
    wrong_kind["cursor"] = first_cursor.clone();
    assert_eq!(
        rpc_call(&state, "reader", &session, "library.page", wrong_kind)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut stale = base.clone();
    stale["cursor"] = first_cursor;
    state
        .vfs
        .rename("/混合/卷话/A.txt", "/混合/卷话/已移动.txt")
        .await
        .unwrap();
    let (status, result) = rpc_call(&state, "reader", &session, "library.page", stale).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(result["code"], "reading_index_changed");
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/apps/reader/scope",
        json!({"mode":"selected","paths":["/混合/卷话"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rpc_call(&state, "reader", &session, "library.page", base)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc_call(
            &state,
            "reader",
            &session,
            "library.page",
            json!({"roots":[child],"kind":"all"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
