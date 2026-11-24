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

async fn get(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::builder().method(Method::GET).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
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

    // 资源请求没有 Origin，宿主只能推导出代理转发来的 http；启动地址必须按同一推导标明策略修订，
    // 页面文档才会带着写明宿主来源的策略被长期缓存。
    let policy = http::PagePolicy::new(Some("http://drive.invalid"));
    let (document, fragment) = launched.frame_url.split_once('#').unwrap();
    assert!(fragment.starts_with("tgdrive="));
    let bare = endpoint_path(
        "assets",
        &[
            ("app_id", &installed.manifest.id),
            ("sha256", &installed.package.sha256),
            ("path", "page/index.html"),
        ],
    );
    assert_eq!(document, format!("{bare}?policy={}", policy.revision));
    let host = [("host", "drive.invalid")];
    let pinned = get(&router, document, &host).await;
    assert_eq!(pinned.status(), StatusCode::OK);
    assert_eq!(pinned.headers()["content-security-policy"], policy.header);
    assert_eq!(
        pinned.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    let etag = pinned.headers()["etag"].to_str().unwrap().to_owned();
    let conditional = [("host", "drive.invalid"), ("if-none-match", etag.as_str())];
    assert_eq!(
        get(&router, document, &conditional).await.status(),
        StatusCode::NOT_MODIFIED
    );

    // 没有标明当前修订的页面文档（旧宿主留下的地址、过期修订、换了访问地址）每次完整返回且不入缓存：
    // 浏览器不会用 304 更新已缓存文档的策略头。
    let stale = format!("{bare}?policy=0000000000000000");
    for (uri, headers) in [
        (bare.as_str(), &conditional[..]),
        (stale.as_str(), &conditional[..]),
        (
            document,
            &[("host", "other.invalid"), ("if-none-match", etag.as_str())][..],
        ),
    ] {
        let response = get(&router, uri, headers).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri} {headers:?}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        let expected = http::PagePolicy::new(Some(&format!("http://{}", headers[0].1)));
        assert_eq!(
            response.headers()["content-security-policy"],
            expected.header
        );
    }

    // 脚本、样式等资源不携带生效的策略，仍按包摘要长期缓存并支持 304。
    let script = bare.replace("index.html", "app.js");
    let response = get(&router, &script, &host).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(
        get(
            &router,
            &script,
            &[("host", "drive.invalid"), ("if-none-match", etag.as_str())]
        )
        .await
        .status(),
        StatusCode::NOT_MODIFIED
    );
}

#[test]
fn page_policy_names_only_origins_the_policy_syntax_can_express() {
    // 替换依赖模板里恰好出现一次只含 'self' 的 connect-src。
    assert_eq!(PAGE_CSP.matches("connect-src 'self';").count(), 1);
    for origin in [
        "http://drive.invalid",
        "https://drive.invalid",
        "http://drive.invalid:8443",
        "http://192.0.2.7:3358",
        "https://xn--fiq228c.invalid",
    ] {
        let policy = http::PagePolicy::new(Some(origin));
        let value = policy.header.to_str().unwrap();
        assert!(
            value.contains(&format!("; connect-src 'self' {origin}; worker-src ")),
            "{value}"
        );
        assert_eq!(value.replacen(&format!(" {origin}"), "", 1), PAGE_CSP);
        assert_eq!(policy.revision.len(), 16);
        assert!(policy.revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            policy.revision,
            http::PagePolicy::new(Some(origin)).revision
        );
    }
    assert_ne!(
        http::PagePolicy::new(Some("http://drive.invalid")).revision,
        http::PagePolicy::new(Some("https://drive.invalid")).revision
    );
    // 策略语法写不出、或可能把策略切开的主机一律不写入，只保留 'self'。
    let fallback = http::PagePolicy::new(None);
    assert_eq!(fallback.header, PAGE_CSP);
    for origin in [
        "http://[::1]:8080",
        "http://my_host",
        "http://drive.invalid;script-src",
        "http://drive.invalid,default-src",
        "http://drive.invalid *",
        "http://*.invalid",
        "http://drive..invalid",
        "http://drive.invalid.",
        "http://drive.invalid:",
        "http://drive.invalid:port",
        "http://drive.invalid:123456",
        "http://drive.invalid/path",
        "http://DRIVE.invalid",
        "http://",
        "ftp://drive.invalid",
        "drive.invalid",
        "null",
        "",
    ] {
        let policy = http::PagePolicy::new(Some(origin));
        assert_eq!(policy.header, PAGE_CSP, "{origin}");
        assert_eq!(policy.revision, fallback.revision, "{origin}");
    }
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
