//! 数据面票据、ETag 再验证、共享字节缓存与事件流的端到端验收。
use super::*;
use axum::http::header;
use futures_util::StreamExt as _;

async fn media_app(state: &AppState) -> String {
    let mut m = manifest();
    m.id = "books".into();
    m.api_version = 2;
    add_app(state, &m, None).await;
    let launch = launch_app(state, "books").await;
    launch["session"].as_str().unwrap().into()
}

async fn fetch(
    state: &AppState,
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router(state)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn bytes_ticket_streams_ranges_with_etag_revalidation_and_shared_cache() {
    let (state, media, _dir) = fixture().await;
    let session = media_app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    let id = seed_file(&state, &media, "/书/示例.txt", b"abcdef", 2).await;
    let (_, stat) = rpc_call(&state, "books", &session, "files.stat", json!({"id":id})).await;
    let version = stat["content_version"].as_str().unwrap().to_string();
    let (status, grant) = rpc_call(
        &state,
        "books",
        &session,
        "media.url",
        json!({"id":id,"content_version":version,"kind":"bytes"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    let url = grant["url"].as_str().unwrap();
    assert!(grant["expires_at"].as_i64().is_some(), "票据应带过期时间");

    let response = fetch(&state, url, "GET", &[("range", "bytes=1-4")]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let etag = response
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(etag, format!("\"{version}\""));
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "private, no-cache"
    );
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(body.as_ref(), b"bcde");

    // 相同 Range 第二次命中共享字节缓存：底层媒体不应再产生新读取。
    let reads_after_first = media.ranges.lock().unwrap().len();
    let response = fetch(&state, url, "GET", &[("range", "bytes=1-4")]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(body.as_ref(), b"bcde");
    assert_eq!(
        media.ranges.lock().unwrap().len(),
        reads_after_first,
        "缓存命中不应触达底层媒体"
    );

    // If-None-Match 命中 → 304 再验证，不返回内容。
    let response = fetch(&state, url, "GET", &[("if-none-match", etag.as_str())]).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap(),
        etag
    );

    // 内容版本变化的票据失效由 media.url 的 RPC 层校验（wrong content_version → 409），此处不再重复。
}

#[tokio::test]
async fn large_byte_ranges_bypass_the_byte_cache_and_preview_always_streams() {
    let (state, media, _dir) = fixture().await;
    let session = media_app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    let payload: Vec<u8> = (0..5 * 512 * 1024).map(|i| (i % 251) as u8).collect();
    let id = seed_file(&state, &media, "/书/大文件.bin", &payload, 512 * 1024).await;
    let (_, stat) = rpc_call(&state, "books", &session, "files.stat", json!({"id":id})).await;
    let version = stat["content_version"].as_str().unwrap().to_string();
    let (status, grant) = rpc_call(
        &state,
        "books",
        &session,
        "media.url",
        json!({"id":id,"content_version":version,"kind":"bytes"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let url = grant["url"].as_str().unwrap();
    let big = format!("bytes=0-{}", 2 * 1024 * 1024); // 超过 2 MiB 缓存上限
    let response = fetch(&state, url, "GET", &[("range", big.as_str())]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let first = {
        let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body.len(), 2 * 1024 * 1024 + 1);
        media.ranges.lock().unwrap().len()
    };
    let response = fetch(&state, url, "GET", &[("range", big.as_str())]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let _ = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert!(
        media.ranges.lock().unwrap().len() > first,
        "超限范围不走缓存，每次都触达底层媒体"
    );
    // 预览面向视频/图片元素：即使小范围也必须流式透传，不为缓存整段缓冲。
    let (status, preview_grant) = rpc_call(
        &state,
        "books",
        &session,
        "media.url",
        json!({"id":id,"content_version":version,"kind":"preview"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let preview_url = preview_grant["url"].as_str().unwrap();
    let reads_before = media.ranges.lock().unwrap().len();
    let response = fetch(&state, preview_url, "GET", &[("range", "bytes=0-1023")]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let _ = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(
        media.ranges.lock().unwrap().len() > reads_before,
        "preview 每次都触达底层媒体，保持渐进播放语义"
    );
}

#[tokio::test]
async fn sandbox_csp_allows_same_origin_connect_and_preflight_answers_options() {
    let (state, _, _dir) = fixture().await;
    let app = {
        let mut m = manifest();
        m.id = "books".into();
        add_app(&state, &m, None).await
    };
    let response = fetch(
        &state,
        &format!("/api/apps/runtime/books/{}/index.html", app.revision),
        "GET",
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        csp.contains("connect-src 'self'"),
        "CSP 应放行同源连接: {csp}"
    );
    assert!(!csp.contains("connect-src 'none'"));

    let response = fetch(&state, "/api/apps/media/whatever", "OPTIONS", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .unwrap(),
        "GET, OPTIONS"
    );
}

#[tokio::test]
async fn sandbox_csp_spells_out_serving_origin_for_webkit() {
    let (state, _, _dir) = fixture().await;
    let app = {
        let mut m = manifest();
        m.id = "books".into();
        add_app(&state, &m, None).await
    };
    let url = format!("/api/apps/runtime/books/{}/index.html", app.revision);
    // WebKit 在 CSP sandbox（不透明源）下不按服务源匹配 connect-src 'self'，
    // fetch 会被拒绝；服务端必须把请求自身的源显式写进 connect-src。
    let with_host = fetch(&state, &url, "GET", &[("host", "drive.example.com")]).await;
    assert_eq!(with_host.status(), StatusCode::OK);
    let csp = with_host
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        csp.contains("connect-src 'self' http://drive.example.com https://drive.example.com;"),
        "CSP 应把服务源显式写进 connect-src: {csp}"
    );
    // 带 IPv6 字面量或端口的 Host 同样原样追加。
    let ipv6 = fetch(&state, &url, "GET", &[("host", "[::1]:8080")]).await;
    let csp = ipv6
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("http://[::1]:8080 https://[::1]:8080"));
    // 非法 Host（含空格、引号、分号等）不得注入 CSP，退回基础策略。
    let bad = fetch(&state, &url, "GET", &[("host", "evil.com x; script")]).await;
    let csp = bad
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!csp.contains("evil.com"), "非法 Host 不得进入 CSP: {csp}");
    // 无 Host 时退回基础策略，仍保留 'self' 供按规范实现的引擎使用。
    let none = fetch(&state, &url, "GET", &[]).await;
    let csp = none
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("connect-src 'self'; frame-src"));
}

#[tokio::test]
async fn sse_stream_broadcasts_storage_settings_and_files_events() {
    let (state, media, _dir) = fixture().await;
    let session = media_app(&state).await;
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header(header::AUTHORIZATION, format!("Basic {credentials}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream; charset=utf-8"
    );
    let mut stream = response.into_body().into_data_stream();

    let (status, _) = rpc_call(
        &state,
        "books",
        &session,
        "storage.set",
        json!({"key":"progress:1","value":{"page":1}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::app("app.scope", "books", None),
    );

    let mut seen = String::new();
    let collected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !(seen.contains("app.storage") && seen.contains("app.scope")) {
            let Some(chunk) = stream.next().await else {
                break;
            };
            seen.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
    })
    .await;
    assert!(collected.is_ok(), "事件流应在超时前送达: {seen}");
    assert!(seen.contains("event: app.storage"));
    assert!(seen.contains("\"key\":\"progress:1\""));
    assert!(seen.contains("event: app.scope"));
    assert_eq!(
        seen.matches("event: app.storage").count(),
        1,
        "单次写入只应广播一次"
    );
    let _ = media;
}

/// 文件事件在每条连接内合并去抖：批量变更只下发一条带并集路径的事件；
/// 其他事件不受待合并事件阻塞；路径未知或超过上限时退化为整体刷新。
#[tokio::test]
async fn sse_coalesces_file_event_bursts_and_merges_paths() {
    let (state, _media, _dir) = fixture().await;
    let credentials = base64::engine::general_purpose::STANDARD.encode("apps-test:apps-test-only");
    let response = router(&state)
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header(header::AUTHORIZATION, format!("Basic {credentials}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut seen = String::new();
    async fn read_until(
        stream: &mut (impl futures_util::Stream<Item = Result<bytes::Bytes, axum::Error>> + Unpin),
        seen: &mut String,
        expected: &str,
    ) {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !seen.contains(expected) {
                let Some(chunk) = stream.next().await else {
                    break;
                };
                seen.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
            }
        })
        .await;
        assert!(waited.is_ok(), "事件流应在超时前送达 {expected}: {seen}");
    }

    // 一次突发 50 条已知目录的事件（3 个目录反复出现）→ 合并为 1 条，路径为排序后的并集。
    let started = tokio::time::Instant::now();
    for i in 0..50 {
        crate::events::emit(
            &state.events,
            crate::events::HostEvent::files_at(vec![format!("/上传/{}", i % 3)]),
        );
    }
    // 突发中途到达的应用事件立即下发，不等文件事件静默。
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::app("app.scope", "books", None),
    );
    read_until(&mut stream, &mut seen, "event: app.scope").await;
    assert!(
        !seen.contains("event: files"),
        "文件事件不应先于静默期结束下发: {seen}"
    );
    read_until(&mut stream, &mut seen, "event: files").await;
    assert!(started.elapsed() < std::time::Duration::from_millis(1500));
    assert_eq!(seen.matches("event: files").count(), 1, "{seen}");
    assert!(
        seen.contains(r#""paths":["/上传/0","/上传/1","/上传/2"]"#),
        "{seen}"
    );

    // 混入范围未知的事件后，合并结果不带 paths（整体刷新）。
    seen.clear();
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::files_at(vec!["/a".into()]),
    );
    crate::events::emit(&state.events, crate::events::HostEvent::files());
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::files_at(vec!["/b".into()]),
    );
    read_until(&mut stream, &mut seen, "event: files").await;
    assert_eq!(seen.matches("event: files").count(), 1, "{seen}");
    assert!(!seen.contains("paths"), "{seen}");

    // 目录数超过上限同样退化为整体刷新。
    seen.clear();
    for i in 0..(crate::events::MAX_EVENT_PATHS + 1) {
        crate::events::emit(
            &state.events,
            crate::events::HostEvent::files_at(vec![format!("/d{i}")]),
        );
    }
    read_until(&mut stream, &mut seen, "event: files").await;
    assert_eq!(seen.matches("event: files").count(), 1, "{seen}");
    assert!(!seen.contains("paths"), "{seen}");
}

/// 文件接口按变更目录广播：上传完成、改名、复制、删除、恢复都带上受影响目录的父目录；
/// 彻底清理回收站不广播。
#[tokio::test]
async fn file_apis_emit_scoped_file_events() {
    let (state, media, _dir) = fixture().await;
    let mut receiver = state.events.subscribe();
    let next_paths = |receiver: &mut tokio::sync::broadcast::Receiver<crate::events::HostEvent>| {
        let event = receiver.try_recv().expect("应有文件事件");
        assert_eq!(event.kind, "files");
        event.paths
    };
    let (status, _) = call(
        &state,
        "POST",
        "/api/fs/mkdir",
        json!({"path": "/影片/2026/"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next_paths(&mut receiver), Some(vec!["/影片".into()]));

    seed_file(&state, &media, "/影片/2026/a.mp4", b"abcdef", 3).await;
    let (status, _) = call(
        &state,
        "POST",
        "/api/fs/rename",
        json!({"from": "/影片/2026/a.mp4", "to": "/影片/b.mp4"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        next_paths(&mut receiver),
        Some(vec!["/影片".into(), "/影片/2026".into()])
    );

    let (status, _) = call(
        &state,
        "POST",
        "/api/fs/copy",
        json!({"from": "/影片/b.mp4", "to": "/影片/2026/c.mp4"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next_paths(&mut receiver), Some(vec!["/影片/2026".into()]));

    let (status, body) = call(
        &state,
        "POST",
        "/api/fs/delete",
        json!({"paths": ["/影片/b.mp4", "/影片/2026/c.mp4", "/不存在"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        next_paths(&mut receiver),
        Some(vec!["/影片".into(), "/影片/2026".into()])
    );

    let (status, body) = call(&state, "POST", "/api/trash/restore", json!({"all": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["restored"].as_array().unwrap().len(), 2);
    assert_eq!(
        next_paths(&mut receiver),
        Some(vec!["/影片".into(), "/影片/2026".into()])
    );

    let (status, _) = call(
        &state,
        "POST",
        "/api/fs/delete",
        json!({"paths": ["/影片/2026/c.mp4"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next_paths(&mut receiver), Some(vec!["/影片/2026".into()]));
    let (status, body) = call(&state, "POST", "/api/trash/purge", json!({"all": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        receiver.try_recv().is_err(),
        "清理回收站不改变可见树，不应广播"
    );
}

/// 批量信封：一次会话校验执行多项能力，逐项独立返回，权限与目录范围逐项生效；
/// 界面、字节读取与嵌套批量不可进入批量，数量与接口版本受限。
#[tokio::test]
async fn rpc_batch_executes_calls_independently_with_per_call_errors() {
    let (state, media, _dir) = fixture().await;
    let session = media_app(&state).await;
    state.vfs.mkdir("/书").await.unwrap();
    let inside = seed_file(&state, &media, "/书/a.txt", b"abc", 2).await;
    let (status, body) = rpc_call(
        &state,
        "books",
        &session,
        "rpc.batch",
        json!({"calls": [
            {"method": "files.stat", "params": {"id": inside}},
            {"method": "files.stat", "params": {"id": 424242}},
            {"method": "app.ping"},
            {"method": "storage.get", "params": {"key": "missing"}},
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["result"]["path"], "/书/a.txt");
    assert_eq!(results[1]["status"], 404);
    assert!(results[1]["error"].is_string());
    assert_eq!(results[2]["result"]["ok"], true);
    assert!(results[3]["result"].is_null());

    // 目录范围逐项生效：范围外的节点在批量里同样 403。
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/apps/books/scope",
        json!({"paths": ["/别处"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = rpc_call(
        &state,
        "books",
        &session,
        "rpc.batch",
        json!({"calls": [{"method": "files.stat", "params": {"id": inside}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], 403);

    for calls in [
        json!([]),
        json!([{"method": "ui.close"}]),
        json!([{"method": "files.readRange", "params": {}}]),
        json!([{"method": "rpc.batch", "params": {"calls": []}}]),
        json!(vec![json!({"method": "app.ping"}); 17]),
    ] {
        let (status, body) = rpc_call(
            &state,
            "books",
            &session,
            "rpc.batch",
            json!({"calls": calls}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{calls} => {body}");
    }

    // v1 应用没有批量能力。
    let mut legacy = manifest();
    legacy.id = "legacy".into();
    legacy.api_version = 1;
    add_app(&state, &legacy, None).await;
    let launch = launch_app(&state, "legacy").await;
    let (status, _) = rpc_call(
        &state,
        "legacy",
        launch["session"].as_str().unwrap(),
        "rpc.batch",
        json!({"calls": [{"method": "app.ping"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 已安装记录缓存：重复读取不再查库，且每种写入（启停、范围、更新、卸载）后立即可见。
#[tokio::test]
async fn installed_record_cache_reflects_every_write() {
    let (state, _media, _dir) = fixture().await;
    let first = add_app(&state, &manifest(), None).await;
    let db = &state.vfs.db;
    let cached = db.app("shorts").await.unwrap().unwrap();
    assert_eq!(cached.revision, first.revision);
    db.set_app_state("shorts", Some(false), None).await.unwrap();
    let disabled = db.app("shorts").await.unwrap().unwrap();
    assert!(!disabled.enabled);
    assert_ne!(disabled.revision, first.revision, "停用应轮换修订");
    db.set_app_scope("shorts", r#"["/x"]"#).await.unwrap();
    assert_eq!(
        db.app("shorts").await.unwrap().unwrap().scope_paths,
        r#"["/x"]"#
    );
    let mut newer = manifest();
    newer.version = "1.0.1".into();
    let updated = add_app(&state, &newer, Some(disabled.revision.clone())).await;
    assert_eq!(
        db.app("shorts").await.unwrap().unwrap().digest,
        updated.digest
    );
    db.uninstall_app("shorts", false).await.unwrap();
    assert!(db.app("shorts").await.unwrap().is_none());
    assert!(matches!(
        installed(&state, "shorts").await,
        Err(AppError::NotFound)
    ));
}
