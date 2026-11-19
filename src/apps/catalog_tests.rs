//! 目录原始历史必须跨刷新、撤回与宿主重启保留，未来版本独立跳过。
use super::catalog::CatalogClient;
use super::tests::fixture;
use serde_json::json;
use tgdrive_contract::{CONTRACT_VERSION, error_code, http::*, validation::*};

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
