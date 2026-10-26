//! 主文件下载复用内存媒体夹具，验证 If-Range 不会混拼两个版本。
use super::*;
use axum::http::header;

async fn download(state: &AppState, headers: &[(&str, &str)]) -> axum::response::Response {
    let cookie = format!("tgdrive_token={}", state.auth.issue_token());
    let mut request = Request::builder()
        .uri("/api/fs/download?path=/test.txt&inline=1")
        .header(header::COOKIE, cookie);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    router(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[tokio::test]
async fn normal_download_if_range_etag_and_304_are_version_safe() {
    let (state, media, _dir) = fixture().await;
    let id = seed_file(&state, &media, "/test.txt", b"abcdef", 2).await;
    let version = state.vfs.db.node(id).await.unwrap().content_version();
    let etag = format!("\"{version}\"");
    let response = download(&state, &[("range", "bytes=2-4"), ("if-range", &etag)]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-4/6");
    assert_eq!(response.headers()[header::ETAG], etag);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"cde"
    );
    for stale in [
        "\"old-version\"",
        "W/\"old-version\"",
        "Wed, 21 Oct 2015 07:28:00 GMT",
    ] {
        let response = download(&state, &[("range", "bytes=2-4"), ("if-range", stale)]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"abcdef"
        );
    }
    let reads = media.ranges.lock().unwrap().len();
    let response = download(
        &state,
        &[("if-none-match", &format!("\"another\", W/{etag}"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(media.ranges.lock().unwrap().len(), reads);
    let response = download(&state, &[("range", "bytes=99-")]).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::ETAG], etag);
}
