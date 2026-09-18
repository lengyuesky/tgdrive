//! 目录范围最小权限的行为验收：空范围全盘放行，设置后逐项收紧。
use super::*;

async fn scope_app(state: &AppState) -> String {
    let mut m = manifest();
    m.id = "books".into();
    m.api_version = 2;
    add_app(state, &m, None).await;
    let launch = launch_app(state, "books").await;
    launch["session"].as_str().unwrap().into()
}

#[tokio::test]
async fn empty_scope_keeps_full_drive_access() {
    let (state, media, _dir) = fixture().await;
    let session = scope_app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    seed_file(&state, &media, "/书/a.txt", b"abc", 2).await;
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.list",
            json!({"path":"/书"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.stat",
            json!({"path":"/书/a.txt"})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn scope_restricts_listing_stat_search_and_media() {
    let (state, media, _dir) = fixture().await;
    let session = scope_app(&state).await;
    state.vfs.mkdir("/图书").await.unwrap();
    state.vfs.mkdir("/秘密").await.unwrap();
    let inside = seed_file(&state, &media, "/图书/a.txt", b"abc", 2).await;
    let secret = seed_file(&state, &media, "/秘密/b.txt", b"xyz", 2).await;
    let (status, app) = call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"paths":["/图书"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{app}");
    assert_eq!(app["scope"], json!(["/图书"]));

    // 范围内放行
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.list",
            json!({"path":"/图书"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.stat",
            json!({"id":inside})
        )
        .await
        .0,
        StatusCode::OK
    );
    // 范围外拒绝：列表、元数据、搜索、媒体票据、收藏
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.list",
            json!({"path":"/秘密"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.stat",
            json!({"id":secret})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.searchPage",
            json!({"under":"/秘密"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc_call(&state, "books", &session, "files.search", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let secret_version = {
        let node = state.vfs.db.node(secret).await.unwrap();
        crate::apps::files::version(&node)
    };
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "media.url",
            json!({"id":secret,"content_version":secret_version})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // 票据在范围内签发后收紧范围：已签票据立即失效。
    let (_, stat) = rpc_call(
        &state,
        "books",
        &session,
        "files.stat",
        json!({"id":inside}),
    )
    .await;
    let inside_version = stat["content_version"].as_str().unwrap().to_string();
    let (status, grant) = rpc_call(
        &state,
        "books",
        &session,
        "media.url",
        json!({"id":inside,"content_version":inside_version}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"paths":["/图书"]}),
    )
    .await;
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(grant["url"].as_str().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"paths":["/其他"]}),
    )
    .await;
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri(grant["url"].as_str().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // 清空范围恢复全盘
    let (status, app) = call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"paths":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app["scope"], json!([]));
    assert_eq!(
        rpc_call(
            &state,
            "books",
            &session,
            "files.stat",
            json!({"id":secret})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn scope_patch_validates_paths_and_limit() {
    let (state, _, _dir) = fixture().await;
    scope_app(&state).await;
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/books/scope",
            json!({"paths":["/秘密/../../etc"]}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let many: Vec<String> = (0..33).map(|i| format!("/d{i}")).collect();
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/books/scope",
            json!({"paths":many}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // 未安装的应用返回 404
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/unknown/scope",
            json!({"paths":[]}),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
