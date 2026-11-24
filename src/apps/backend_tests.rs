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
pub(super) async fn runtime_info(run: &Arc<backend::Run>) -> RuntimeInfo {
    let response = forwarded(run, "/host-info").await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), MAX_JSON_RESPONSE_BYTES as usize)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let info: RuntimeInfo = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(info.caller, RuntimeCaller::Backend);
    let limits = info.limits.backend.as_ref().unwrap();
    // 同时核验真实 HTTP 输出，避免反序列化丢弃旧字段后掩盖契约残留。
    assert_eq!(
        value["limits"]["backend"],
        serde_json::json!({
            "memory_bytes": limits.memory_bytes,
            "cpu_millis": limits.cpu_millis,
            "enforced": {"memory": limits.enforced.memory, "cpu": limits.enforced.cpu},
        })
    );
    info
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
async fn backend_crash_across_second_boundary_waits_a_full_backoff() {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let request = hyper::Request::builder()
        .uri("/exit")
        .header("host", "tgdrive")
        .header(
            FORWARD_CONTEXT_HEADER,
            forward(&run, ForwardCaller::Owner, ForwardEntry::Page),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let wall_clock = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    // 时间窗仅用于稳定复现旧实现；调度错过窗口时仍继续核验完整的单调退避。
    let crash_second = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let wall = wall_clock();
            if (400..550).contains(&wall.subsec_millis()) {
                break wall.as_secs();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| wall_clock().as_secs());
    let crashed = Instant::now();
    assert!(backend::request(&run, request).await.is_err());
    tokio::time::timeout(Duration::from_secs(5), run.finished.cancelled())
        .await
        .unwrap();
    assert_eq!(
        platform
            .backends
            .status("test-backend", RunMode::OnDemand)
            .state,
        BackendState::Failed
    );
    // 跨秒调用让旧实现的整秒截止点过期；回收已跨秒或系统时钟跳变都不影响断言。
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while wall_clock().as_secs() <= crash_second {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let replacement = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let elapsed = crashed.elapsed();
    assert_ne!(run.caller.id, replacement.caller.id);
    platform.backends.stop("test-backend").await.unwrap();
    println!("退避回归：跨秒重启，崩溃到新轮次就绪 {elapsed:.3?}");
    assert!(
        elapsed >= Duration::from_secs(1),
        "跨秒重启的首次退避不足一秒：{elapsed:?}"
    );
}

fn oversized_private_directories(platform: &AppPlatform) -> u64 {
    let data = platform.data_root().join("test-backend");
    let bytes = 8 * 1024 * 1024 * 1024;
    for name in ["state", "cache/work", "tmp"] {
        let directory = data.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::File::create(directory.join("large-private-file"))
            .unwrap()
            .set_len(bytes)
            .unwrap();
        // 生产统计使用 st_size 而非已分配块数，稀疏文件也必须计入全部逻辑长度。
        assert_eq!(
            super::safe_fs::Directory::open(&directory, false)
                .unwrap()
                .usage(None)
                .unwrap(),
            bytes
        );
    }
    bytes
}

#[tokio::test]
async fn oversized_private_directories_do_not_block_start_or_stop_a_running_backend() {
    let (_db, _data, platform, _) = backend_fixture().await;
    oversized_private_directories(&platform);
    backend::start(&platform);
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    // 监督循环每秒一次，跨过两个完整间隔后仍须保持同一运行轮次且可响应请求。
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert!(!run.caller.cancelled.is_cancelled());
    assert!(!run.finished.is_cancelled());
    let info = runtime_info(&run).await;
    let status = info.backend.unwrap();
    assert_eq!(status.state, BackendState::Ready);
    assert_eq!(status.run_id.as_deref(), Some(run.caller.id.as_str()));
    assert_eq!(
        platform.backends.run("test-backend").unwrap().caller.id,
        run.caller.id
    );
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn backend_private_usage_does_not_reduce_object_cache_or_mix_into_cache_usage() {
    use tgdrive_contract::data::CachePutMetadata;
    let (_db, _data, platform, _) = backend_fixture().await;
    let private_bytes = oversized_private_directories(&platform);
    let run = platform
        .backends
        .ensure(&platform, "test-backend")
        .await
        .unwrap();
    let info = runtime_info(&run).await;
    assert_eq!(info.limits.cache_byte_limit.0, DEFAULT_CACHE_BYTES);
    let page = super::tests::install_page(&platform).await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let page = store::installation(&mut connection, &page.manifest.id)
        .await
        .unwrap();
    let page_limits =
        super::runtime::limits(&page, platform.shared_limits().await.unwrap().values()).unwrap();
    drop(connection);
    assert_eq!(info.limits.cache_byte_limit, page_limits.cache_byte_limit);
    assert!(page_limits.backend.is_none());
    let before = platform
        .data_usage("test-backend", &run.caller.generation)
        .await
        .unwrap();
    assert_eq!(before.cache_bytes.0, 0);
    assert!(before.database_bytes.unwrap().0 >= private_bytes);
    let body = bytes::Bytes::from(vec![1; MAX_CACHE_OBJECT_BYTES as usize]);
    for key in ["first", "second"] {
        platform
            .cache_put(
                &run.caller,
                key,
                "application/octet-stream".into(),
                CachePutMetadata::default(),
                body.clone(),
            )
            .await
            .unwrap();
    }
    // 私有目录早已超过旧配额，写入新对象仍不能拒绝或淘汰前一个对象。
    for key in ["first", "second"] {
        assert_eq!(
            platform
                .cache_read(&run.caller, key, true, None)
                .await
                .unwrap()
                .bytes,
            body
        );
    }
    let data = std::path::Path::new(&run.data_dir);
    let directory_bytes = |name| {
        std::fs::read_dir(data.join(name))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>()
    };
    let usage = platform
        .data_usage("test-backend", &run.caller.generation)
        .await
        .unwrap();
    assert_eq!(usage.cache_byte_limit.0, DEFAULT_CACHE_BYTES);
    assert_eq!(usage.cache_bytes.0, directory_bytes("cache/objects"));
    assert!(usage.cache_bytes.0 >= 2 * MAX_CACHE_OBJECT_BYTES);
    assert!(usage.cache_bytes.0 < DEFAULT_CACHE_BYTES);
    assert_eq!(usage.database_bytes.unwrap().0, directory_bytes("state"));
    assert!(
        serde_json::to_value(usage)
            .unwrap()
            .get("database_byte_limit")
            .is_none()
    );
    assert!(!run.caller.cancelled.is_cancelled());
    platform.backends.stop("test-backend").await.unwrap();
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
