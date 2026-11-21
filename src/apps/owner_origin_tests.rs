//! 浏览器经 HTTP 反向代理访问 HTTPS 应用中心时的安装与来源隔离回归。
use super::tests::fixture;
use super::*;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use std::net::SocketAddr;
use std::sync::Arc;
use tgdrive_contract::{http::*, runtime::*};
use tower::ServiceExt;

async fn owner_request(
    router: &Router,
    name: &str,
    parameters: &[(&str, &str)],
    cookie: Option<&str>,
    body: Vec<u8>,
    headers: &[(&str, &str)],
    peer: Option<SocketAddr>,
) -> Response {
    let spec = ENDPOINTS.iter().find(|spec| spec.name == name).unwrap();
    let mut request = Request::builder()
        .method(Method::from_bytes(spec.methods[0].as_bytes()).unwrap())
        .uri(endpoint_path(name, parameters));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body)).unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    router.clone().oneshot(request).await.unwrap()
}

fn owner_cookie(platform: &AppPlatform) -> String {
    platform
        .host
        .auth
        .cookie_header(&platform.host.auth.issue_token())
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn json<T: serde::de::DeserializeOwned>(response: Response) -> T {
    serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_JSON_RESPONSE_BYTES as usize)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn https_owner_can_upload_preview_install_and_launch_through_http_proxy() {
    let (_dir, platform, _) = fixture().await;
    let cookie = owner_cookie(&platform);
    let router = http::router_platform(platform.clone());
    let mut headers = vec![
        ("host", "drive.invalid"),
        ("origin", "https://drive.invalid"),
        ("sec-fetch-site", "same-origin"),
        ("content-type", "application/zip"),
    ];
    let upload = owner_request(
        &router,
        "center.package.upload",
        &[],
        Some(&cookie),
        package::tests::page_package("1.0.0"),
        &headers,
        None,
    )
    .await;
    assert_eq!(upload.status(), StatusCode::CREATED);
    let upload: PackageUploadReceipt = json(upload).await;
    *headers.last_mut().unwrap() = ("content-type", "application/json");
    let preview = owner_request(
        &router,
        "center.install.preview",
        &[],
        Some(&cookie),
        serde_json::to_vec(&InstallPreviewRequest {
            source: InstallSource::Uploaded {
                package_id: upload.package_id,
            },
        })
        .unwrap(),
        &headers,
        None,
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview: InstallPreview = json(preview).await;
    let installed = owner_request(
        &router,
        "center.install",
        &[],
        Some(&cookie),
        serde_json::to_vec(&InstallRequest {
            preview_id: preview.preview_id,
        })
        .unwrap(),
        &headers,
        None,
    )
    .await;
    assert_eq!(installed.status(), StatusCode::OK);
    let installed: InstalledApp = json(installed).await;
    let launched = owner_request(
        &router,
        "center.app.launch",
        &[("app_id", &installed.manifest.id)],
        Some(&cookie),
        br#"{"open_node_id":null}"#.to_vec(),
        &headers,
        None,
    )
    .await;
    assert_eq!(launched.status(), StatusCode::OK);
    let launched: LaunchPageResponse = json(launched).await;
    assert_eq!(launched.bootstrap.host_origin, "https://drive.invalid");
    assert_eq!(
        launched.bootstrap.api_base,
        format!("https://drive.invalid{SELF_API_BASE}")
    );
}

#[tokio::test]
async fn owner_gate_rejects_cross_origin_and_untrusted_forwarded_hosts() {
    let (_dir, platform, _) = fixture().await;
    let cookie = owner_cookie(&platform);
    let router = http::router_platform(platform);
    for headers in [
        vec![("origin", "https://other.invalid")],
        vec![
            ("origin", "http://drive.invalid"),
            ("sec-fetch-site", "cross-site"),
        ],
        vec![
            ("origin", "http://drive.invalid"),
            ("sec-fetch-site", "same-site"),
        ],
        vec![
            ("origin", "https://other.invalid"),
            ("sec-fetch-site", "same-origin"),
            ("x-forwarded-host", "other.invalid"),
            ("x-forwarded-proto", "https"),
        ],
        vec![
            ("origin", "https://drive.invalid"),
            ("sec-fetch-site", "none"),
        ],
        vec![
            ("origin", "https://drive.invalid:8443"),
            ("sec-fetch-site", "same-origin"),
        ],
        vec![
            ("origin", "https://drive.invalid/path"),
            ("sec-fetch-site", "same-origin"),
        ],
    ] {
        let mut headers = headers;
        headers.push(("host", "drive.invalid"));
        headers.push(("content-type", "application/json"));
        let response = owner_request(
            &router,
            "center.catalog.refresh",
            &[],
            Some(&cookie),
            br#"{"force":true}"#.to_vec(),
            &headers,
            Some("192.0.2.1:1234".parse().unwrap()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
    }
    for (cookie, origin) in [
        (None, "https://drive.invalid"),
        (Some(cookie.as_str()), "null"),
    ] {
        let response = owner_request(
            &router,
            "center.catalog.refresh",
            &[],
            cookie,
            br#"{"force":true}"#.to_vec(),
            &[
                ("host", "drive.invalid"),
                ("origin", origin),
                ("sec-fetch-site", "same-origin"),
                ("content-type", "application/json"),
            ],
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn configured_proxy_and_legacy_direct_requests_keep_working() {
    let (_dir, mut platform, _) = fixture().await;
    Arc::get_mut(&mut Arc::get_mut(&mut platform).unwrap().host.rt)
        .and_then(|runtime| Arc::get_mut(&mut runtime.env))
        .unwrap()
        .trusted_proxies = crate::security::parse_trusted_proxies("192.0.2.2").unwrap();
    let cookie = owner_cookie(&platform);
    let router = http::router_platform(platform);
    for (headers, peer) in [
        (
            vec![
                ("host", "drive.invalid"),
                ("origin", "http://drive.invalid"),
            ],
            None,
        ),
        (vec![("host", "drive.invalid")], None),
        (
            vec![
                ("host", "drive.invalid:8443"),
                ("origin", "https://drive.invalid:8443"),
                ("sec-fetch-site", "same-origin"),
            ],
            None,
        ),
        (
            vec![
                ("host", "tgdrive:8080"),
                ("origin", "https://drive.invalid"),
                ("x-forwarded-host", "drive.invalid"),
                ("x-forwarded-proto", "https"),
            ],
            Some("192.0.2.2:1234".parse().unwrap()),
        ),
    ] {
        let mut headers = headers;
        headers.push(("content-type", "application/json"));
        let response = owner_request(
            &router,
            "center.catalog.refresh",
            &[],
            Some(&cookie),
            br#"{"force":true}"#.to_vec(),
            &headers,
            peer,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{headers:?}");
    }
}
