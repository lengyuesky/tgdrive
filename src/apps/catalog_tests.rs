//! 目录原始历史必须跨刷新、撤回与宿主重启保留，未来版本独立跳过。
use super::catalog::CatalogClient;
use super::tests::fixture;
use crate::proxy::{ProxyConfig, ProxySnapshot};
use serde_json::json;
use std::time::Duration;
use tgdrive_contract::{CONTRACT_VERSION, error_code, http::*, validation::*};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn catalog(future_digest: &str, include_future: bool) -> String {
    let repository = "example/apps";
    let known = json!({"manifest":{"id":"known-page","name":"测试","version":"1.0.0","description":"测试","author":"测试","contract":{"major":1,"minor":0},"page":{"entry":"page/index.html"},"future_optional":{"kept":true}},"packages":{"web":{"url":package_release_url(repository,"known-page","1.0.0","web"),"sha256":"a".repeat(64),"size":"100"}}});
    let future = json!({"manifest":{"id":"future-page","version":"2.0.0","contract":{"major":2,"minor":0},"future_shape":["不能按旧清单解析"]},"packages":{"linux-future64":{"url":package_release_url(repository,"future-page","2.0.0","linux-future64"),"sha256":future_digest,"size":(MAX_PACKAGE_BYTES+1).to_string()}}});
    let entries = if include_future {
        vec![known, future]
    } else {
        vec![known]
    };
    serde_json::to_string(
        &json!({"format":{"major":1,"minor":1},"repository":repository,"entries":entries}),
    )
    .unwrap()
}

#[tokio::test]
async fn future_entries_are_skipped_but_their_immutable_history_survives_withdrawal() {
    let (_dir, platform, _) = fixture().await;
    let cache = tempfile::tempdir().unwrap();
    let caps = super::runtime::capabilities();
    let client = CatalogClient::new(
        platform.host.vfs.db.clone(),
        "example/apps".into(),
        caps.clone(),
        cache.path().join("first"),
        platform.host.vfs.tg.proxy_client(),
    )
    .await
    .unwrap();
    let raw = catalog(&"a".repeat(64), true);
    let parsed =
        parse_catalog_for_host(raw.as_bytes(), "example/apps", CONTRACT_VERSION, &caps).unwrap();
    assert_eq!(parsed.catalog.entries.len(), 1);
    assert_eq!(parsed.incompatible.len(), 1);
    assert_eq!(
        parsed.catalog.entries[0].manifest.extra["future_optional"],
        json!({"kept":true})
    );
    client.persist_history(&parsed, &raw).await.unwrap();
    let raw = catalog(&"a".repeat(64), false);
    let parsed =
        parse_catalog_for_host(raw.as_bytes(), "example/apps", CONTRACT_VERSION, &caps).unwrap();
    client.persist_history(&parsed, &raw).await.unwrap();
    let restarted = CatalogClient::new(
        platform.host.vfs.db.clone(),
        "example/apps".into(),
        caps.clone(),
        cache.path().join("restart"),
        platform.host.vfs.tg.proxy_client(),
    )
    .await
    .unwrap();
    let raw = catalog(&"b".repeat(64), true);
    let parsed =
        parse_catalog_for_host(raw.as_bytes(), "example/apps", CONTRACT_VERSION, &caps).unwrap();
    assert_eq!(
        restarted
            .persist_history(&parsed, &raw)
            .await
            .unwrap_err()
            .code,
        error_code::PACKAGE_CHANGED
    );
}

#[tokio::test]
async fn catalog_network_uses_updated_proxy_and_keeps_redirects_explicit() {
    let (_dir, platform, _) = fixture().await;
    let cache = tempfile::tempdir().unwrap();
    let network = platform.host.vfs.tg.proxy_client();
    let catalog = CatalogClient::new(
        platform.host.vfs.db.clone(),
        "example/apps".into(),
        super::runtime::capabilities(),
        cache.path().into(),
        network.clone(),
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        network.install(
            ProxySnapshot::new(
                ProxyConfig::new(true, &format!("http://{}", proxy.local_addr().unwrap())).unwrap(),
            )
            .unwrap(),
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            assert!(request.starts_with(b"GET http://catalog.invalid/catalog.json HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://other.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (_snapshot, client) = catalog.http_client().unwrap();
        let response = client
            .get("http://catalog.invalid/catalog.json")
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
