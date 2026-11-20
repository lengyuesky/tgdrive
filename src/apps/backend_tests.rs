//! 本机测试后端只进入 cfg(test)，使用真实 backend-sdk 与 Unix HTTP。
use super::tests::{Synthetic, fixture};
use super::*;
use std::sync::Arc;
use tgdrive_contract::{CONTRACT_VERSION, files::*, http::*, manifest::*, runtime::*};

pub(super) async fn backend_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<AppPlatform>,
    Arc<Synthetic>,
) {
    let (db_dir, original, fake) = fixture().await;
    let short = tempfile::Builder::new()
        .prefix("tg-backend-")
        .tempdir_in("/tmp")
        .unwrap();
    let platform = Arc::new(AppPlatform::with_repository(
        original.host.clone(),
        String::new(),
    ));
    let package_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let package = platform.package_root.join(package_sha).join("backend");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), package.join("app")).unwrap();
    let manifest: Manifest = serde_json::from_value(serde_json::json!({"id":"test-backend","name":"测试后端","version":"1.0.0","contract":{"major":1,"minor":0},"description":"只在测试里运行","author":"测试","files":{"read":true,"write":true},"page":{"entry":"page/index.html"},"backend":{"entry":"backend/app","run_mode":"on_demand","external_service":true}})).unwrap();
    sqlx::query("INSERT INTO app_installations(id,manifest,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at) VALUES(?,?,?, ?,1,0,?,?,'linux-arm64',?,1,?,?)")
        .bind(&manifest.id).bind(serde_json::to_string(&manifest).unwrap()).bind("generation-1").bind("state-1").bind(r#"{"mode":"all","access":"read_write"}"#).bind("scope-1").bind(package_sha).bind(now().0).bind(now().0).execute(&platform.host.vfs.db.pool).await.unwrap();
    (db_dir, short, platform, fake)
}

pub(super) async fn forwarded(
    run: &Arc<backend::Run>,
    path: &str,
) -> hyper::Response<axum::body::Body> {
    let request = hyper::Request::builder()
        .uri(path)
        .header("host", "tgdrive")
        .header(
            FORWARD_CONTEXT_HEADER,
            forward(run, ForwardCaller::Owner, ForwardEntry::Page),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    backend::request(run, request).await.unwrap()
}
pub(super) fn forward(run: &backend::Run, caller: ForwardCaller, entry: ForwardEntry) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&ForwardContext {
            contract: CONTRACT_VERSION,
            app_id: run.caller.app_id.clone(),
            caller,
            entry,
            request_id: "request-1".into(),
            original_uri: "/host-info".into(),
            public_origin: "http://localhost".into(),
            public_base_url: "http://localhost/admin/api/apps/v1/self/backend".into(),
            client_ip: None,
        })
        .unwrap(),
    )
}

#[tokio::test]
async fn sdk_backend_becomes_ready_calls_host_and_stops() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let response = forwarded(&run, "/host-info").await;
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(
        axum::body::Body::new(response.into_body()),
        MAX_JSON_RESPONSE_BYTES as usize,
    )
    .await
    .unwrap();
    let info: RuntimeInfo = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.caller, RuntimeCaller::Backend);
    assert_eq!(info.app.id, "test-backend");
    assert_eq!(
        info.limits.control_concurrency,
        BACKEND_CONTROL_CONCURRENCY as u16
    );
    assert!(!info.capabilities.iter().any(|cap| cap.starts_with("ui.")));
    assert_eq!(
        platform
            .installed("test-backend")
            .await
            .unwrap()
            .backend
            .unwrap()
            .state,
        BackendState::Ready
    );
    platform.backends.stop("test-backend").await.unwrap();
    assert!(run.caller.cancelled.is_cancelled());
    assert!(run.finished.is_cancelled());
}

#[tokio::test]
async fn concurrent_start_is_merged_and_environment_is_not_inherited() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let runs = futures_util::future::join_all(
        (0..8).map(|_| platform.backends.ensure(&platform, "test-backend")),
    )
    .await;
    let first = runs[0].as_ref().unwrap();
    assert!(
        runs.iter()
            .all(|run| run.as_ref().unwrap().caller.id == first.caller.id)
    );
    let response = forwarded(first, "/environment").await;
    let bytes = axum::body::to_bytes(axum::body::Body::new(response.into_body()), 64 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for key in value["env"].as_object().unwrap().keys() {
        // macOS 的进程运行库会自行补充这一非敏感变量，不是宿主环境继承。
        if cfg!(target_os = "macos") && key == "__CF_USER_TEXT_ENCODING" {
            continue;
        }
        assert!(
            [
                "PATH",
                "LANG",
                "LC_ALL",
                "HOME",
                "TMPDIR",
                "TGDRIVE_TEST_BACKEND_CONTEXT"
            ]
            .contains(&key.as_str()),
            "后端继承了非白名单环境变量 {key}"
        );
    }
    assert_eq!(value["uid"], unsafe { libc::geteuid() });
    platform.backends.stop("test-backend").await.unwrap();
}

pub(super) async fn test_file(
    platform: &Arc<AppPlatform>,
    fake: &Synthetic,
    bytes: &[u8],
) -> ContentRef {
    let id = super::tests::file(platform, fake, 1, "input.bin", bytes).await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let app = store::installation(&mut connection, "test-backend")
        .await
        .unwrap();
    let node = store::reference(
        &mut connection,
        &app,
        &NodeReference::Id {
            id: id.to_string(),
            content_version: None,
        },
    )
    .await
    .unwrap();
    ContentRef {
        id: node.id,
        content_version: node.content_version.unwrap(),
    }
}

#[tokio::test]
async fn local_open_range_and_immediate_seek_share_only_pulled_blocks() {
    use http_body_util::BodyExt;
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let file = test_file(&platform, &fake, &vec![7; 2 * 1024 * 1024]).await;
    let ticket = platform
        .local_url(
            &run.caller,
            LocalFileUrlRequest {
                file,
                ttl_seconds: None,
                byte_budget: Some((1024 * 1024).into()),
                request_budget: Some(4),
                lease_id: None,
            },
        )
        .await
        .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("range", "bytes=0-".parse().unwrap());
    let mut first = platform
        .local_bytes(&ticket.id, &headers, false)
        .await
        .unwrap();
    assert_eq!(first.status(), 206);
    assert_eq!(first.headers()["content-length"], "1048576");
    assert_eq!(
        first
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .len(),
        64 * 1024
    );
    headers.insert("range", "bytes=1500000-1500031".parse().unwrap());
    let second = platform
        .local_bytes(&ticket.id, &headers, false)
        .await
        .unwrap();
    assert_eq!(second.status(), 206);
    assert_eq!(
        axum::body::to_bytes(second.into_body(), 64)
            .await
            .unwrap()
            .len(),
        32
    );
    // 前一个响应尚未关闭且还声明剩余字节；撤销仍必须中止它，不依赖读取租约。
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    assert!(first.body_mut().frame().await.unwrap().is_err());
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn loopback_tcp_can_seek_before_previous_response_is_closed() {
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let file = test_file(&platform, &fake, &vec![8; 24 * 1024 * 1024]).await;
    let ticket = platform
        .local_url(
            &run.caller,
            LocalFileUrlRequest {
                file,
                ttl_seconds: None,
                byte_budget: None,
                request_budget: None,
                lease_id: None,
            },
        )
        .await
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut first = client
        .get(&ticket.url)
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 206);
    assert!(!first.chunk().await.unwrap().unwrap().is_empty());
    let second = client
        .get(&ticket.url)
        .header("Range", "bytes=20000000-20000031")
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 206);
    assert_eq!(second.bytes().await.unwrap().len(), 32);
    drop(first);
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn ffprobe_and_ffmpeg_share_the_same_local_address_budget() {
    let (_db, data, platform, fake) = backend_fixture().await;
    let executable = |name: &str| {
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
            .iter()
            .map(|directory| std::path::Path::new(directory).join(name))
            .find(|path| path.is_file())
            .expect("验收环境需要真实 ffmpeg/ffprobe")
    };
    let input = data.path().join("fixture.mp4");
    let output = tokio::process::Command::new(executable("ffmpeg"))
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:size=32x32:rate=5",
            "-t",
            "0.4",
            "-c:v",
            "mpeg4",
            "-movflags",
            "+faststart",
        ])
        .arg(&input)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contents = std::fs::read(input).unwrap();
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let file = test_file(&platform, &fake, &contents).await;
    let ticket = platform
        .local_url(
            &run.caller,
            LocalFileUrlRequest {
                file,
                ttl_seconds: None,
                byte_budget: Some((1024 * 1024).into()),
                request_budget: None,
                lease_id: None,
            },
        )
        .await
        .unwrap();
    let mut probe = tokio::process::Command::new(executable("ffprobe"));
    probe
        .args(["-v", "error", "-show_format", "-show_streams"])
        .arg(&ticket.url)
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), probe.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut decode = tokio::process::Command::new(executable("ffmpeg"));
    decode
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(&ticket.url)
        .args(["-f", "null", "-"])
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), decode.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn fixture_backend_main() {
    use tgdrive_backend_sdk::{BackendServer, StartContext, UnixHostClient};
    let Some(path) = std::env::var_os("TGDRIVE_TEST_BACKEND_CONTEXT") else {
        return;
    };
    let context = StartContext::parse(&std::fs::read(path).unwrap(), CONTRACT_VERSION).unwrap();
    let database = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&context.context.database_path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS fixture(value TEXT)")
        .execute(&database)
        .await
        .unwrap();
    sqlx::query("INSERT INTO fixture VALUES('在线快照测试')")
        .execute(&database)
        .await
        .unwrap();
    let host = Arc::new(UnixHostClient::new(&context));
    BackendServer::bind(context)
        .unwrap()
        .run(
            move |request| fixture_route(host.clone(), request),
            |_| async {},
        )
        .await
        .unwrap();
    database.close().await;
}

async fn fixture_route(
    host: Arc<tgdrive_backend_sdk::UnixHostClient>,
    request: tgdrive_backend_sdk::ForwardRequest,
) -> tgdrive_backend_sdk::Result<tgdrive_backend_sdk::HttpResponse> {
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use tgdrive_backend_sdk::{HostClient, json_response};
    let path = request.request.uri().path();
    match path {
        "/host-info" => json_response(200, &host.runtime_info().await?),
        "/environment" => {
            let values = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
            json_response(
                200,
                &serde_json::json!({"uid": unsafe { libc::geteuid() }, "gid": unsafe { libc::getegid() }, "env": values}),
            )
        }
        "/spawn" => {
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "apps::backend_tests::fixture_child_wait",
                    "--nocapture",
                ])
                .spawn()
                .unwrap();
            json_response(200, &serde_json::json!({"pid": child.id().unwrap()}))
        }
        "/exit" => std::process::exit(42),
        "/echo" | "/cookies" => {
            let cookies = path == "/cookies";
            let headers = request
                .request
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.to_string(), value.to_owned()))
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            let uri = request.request.uri().to_string();
            let body = tgdrive_backend_sdk::collect_bytes(
                Box::pin(
                    request
                        .request
                        .into_body()
                        .into_data_stream()
                        .map(|bytes| bytes.map_err(|_| tgdrive_backend_sdk::Error::Transport)),
                ),
                MAX_PROXY_BODY_BYTES,
            )
            .await?;
            let mut response = json_response(
                200,
                &serde_json::json!({"context": request.context, "headers": headers, "uri": uri, "body_bytes": body.len()}),
            )?;
            if cookies {
                for value in [
                    "tgdrive_token=forged; Path=/other",
                    "tgdrive_future=forged; Domain=localhost",
                    "app_session=private; Path=/admin; Domain=localhost; HttpOnly",
                ] {
                    response
                        .headers_mut()
                        .append("set-cookie", value.parse().unwrap());
                }
                response.headers_mut().insert(
                    "content-security-policy",
                    "default-src *; script-src *".parse().unwrap(),
                );
                response.headers_mut().insert(
                    "content-security-policy-report-only",
                    "report-uri https://outside.invalid".parse().unwrap(),
                );
                response
                    .headers_mut()
                    .insert("x-tgdrive-forged", "not-public".parse().unwrap());
            }
            Ok(response)
        }
        "/stream" => {
            let query = reqwest::Url::parse(&format!("http://localhost{}", request.request.uri()))
                .unwrap()
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let node = host
                .stat(NodeReference::Id {
                    id: query.get("id").unwrap().clone(),
                    content_version: None,
                })
                .await?;
            let window = query.get("offset").map(|offset| ByteWindow {
                offset: offset.parse::<u64>().unwrap().into(),
                length: query.get("length").unwrap().parse::<u64>().unwrap().into(),
            });
            let directive = StreamDirective {
                file: ContentRef {
                    id: node.id,
                    content_version: node.content_version.unwrap(),
                },
                window,
                range: if query.contains_key("ignore") {
                    RangePolicy::Ignore
                } else {
                    RangePolicy::Client
                },
                disposition: Disposition::Inline,
                filename: None,
                content_type: Some("application/octet-stream".into()),
                cache: StreamCachePolicy::PrivateRevalidate,
                lease_id: query.get("lease").cloned(),
                correlation_id: query.get("correlation").cloned(),
            };
            let mut response = tgdrive_backend_sdk::stream_response(
                request.request.method(),
                &directive,
                node.size.0,
            )?;
            response
                .headers_mut()
                .insert("x-example-protocol", "kept".parse().unwrap());
            Ok(response)
        }
        "/invalid-stream" => {
            let mut response = json_response(200, &serde_json::json!({}))?;
            response
                .headers_mut()
                .insert(STREAM_DIRECTIVE_HEADER, "v1;e30".parse().unwrap());
            Ok(response)
        }
        "/sse" => {
            let stream =
                futures_util::stream::once(async { Ok(bytes::Bytes::from_static(b": ready\n\n")) })
                    .chain(futures_util::stream::pending());
            Ok(hyper::Response::builder()
                .header("content-type", "text/event-stream")
                .body(tgdrive_backend_sdk::stream_body(Box::pin(stream)))
                .unwrap())
        }
        _ => json_response(200, &request.context),
    }
}

#[tokio::test]
#[ignore]
async fn fixture_child_wait() {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
}
