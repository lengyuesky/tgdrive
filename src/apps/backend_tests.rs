//! 本机测试后端只进入 cfg(test)，使用真实 backend-sdk 与 Unix HTTP。
use super::tests::{Synthetic, fixture};
use super::*;
use std::sync::Arc;
use tgdrive_contract::{CONTRACT_VERSION, files::*, http::*, manifest::*, runtime::*};

pub(super) async fn backend_fixture() -> (tempfile::TempDir, tempfile::TempDir, Arc<AppPlatform>, Arc<Synthetic>) {
    let (db_dir, original, fake) = fixture().await;
    let short = tempfile::Builder::new().prefix("tg-backend-").tempdir_in("/tmp").unwrap();
    let mut platform = AppPlatform::with_repository(original.host.clone(), String::new());
    platform.root = short.path().join("apps");
    platform.package_root = platform.root.join("packages");
    platform.download_root = platform.root.join("downloads");
    let platform = Arc::new(platform);
    let package = platform.package_root.join("test-package").join("backend");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), package.join("app")).unwrap();
    let manifest: Manifest = serde_json::from_value(serde_json::json!({"id":"test-backend","name":"测试后端","version":"1.0.0","contract":{"major":1,"minor":0},"description":"只在测试里运行","author":"测试","files":{"read":true,"write":true},"page":{"entry":"page/index.html"},"backend":{"entry":"backend/app","run_mode":"on_demand","external_service":true}})).unwrap();
    sqlx::query("INSERT INTO app_installations(id,manifest,generation,state_revision,enabled,pinned,scope,scope_revision,target,package_sha256,package_size,installed_at,updated_at) VALUES(?,?,?, ?,1,0,?,?,'linux-arm64','test-package',1,?,?)")
        .bind(&manifest.id).bind(serde_json::to_string(&manifest).unwrap()).bind("generation-1").bind("state-1").bind(r#"{"mode":"all","access":"read_write"}"#).bind("scope-1").bind(now().0).bind(now().0).execute(&platform.host.vfs.db.pool).await.unwrap();
    (db_dir, short, platform, fake)
}

pub(super) async fn forwarded(run: &Arc<backend::Run>, path: &str) -> hyper::Response<hyper::body::Incoming> {
    let request = hyper::Request::builder().uri(path).header("host", "tgdrive")
        .header(FORWARD_CONTEXT_HEADER, forward(run, ForwardCaller::Owner, ForwardEntry::Page))
        .body(axum::body::Body::empty()).unwrap();
    backend::request(run, request).await.unwrap()
}
pub(super) fn forward(run: &backend::Run, caller: ForwardCaller, entry: ForwardEntry) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&ForwardContext {
        contract: CONTRACT_VERSION, app_id: run.caller.app_id.clone(), caller, entry, request_id: "request-1".into(),
        original_uri: "/host-info".into(), public_origin: "http://localhost".into(), public_base_url: "http://localhost/admin/api/apps/v1/self/backend".into(), client_ip: None,
    }).unwrap())
}

#[tokio::test]
async fn sdk_backend_becomes_ready_calls_host_and_stops() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let run = platform.backends.ensure(&platform, "test-backend").await.unwrap();
    let response = forwarded(&run, "/host-info").await;
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(axum::body::Body::new(response.into_body()), MAX_JSON_RESPONSE_BYTES as usize).await.unwrap();
    let info: RuntimeInfo = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.caller, RuntimeCaller::Backend);
    assert_eq!(info.app.id, "test-backend");
    assert_eq!(info.limits.control_concurrency, BACKEND_CONTROL_CONCURRENCY as u16);
    assert!(!info.capabilities.iter().any(|cap| cap.starts_with("ui.")));
    assert_eq!(platform.installed("test-backend").await.unwrap().backend.unwrap().state, BackendState::Ready);
    platform.backends.stop("test-backend").await.unwrap();
    assert!(run.caller.cancelled.is_cancelled());
    assert!(run.finished.is_cancelled());
}

#[tokio::test]
async fn concurrent_start_is_merged_and_environment_is_not_inherited() {
    let (_db, _data, platform, _) = backend_fixture().await;
    let runs = futures_util::future::join_all((0..8).map(|_| platform.backends.ensure(&platform, "test-backend"))).await;
    let first = runs[0].as_ref().unwrap();
    assert!(runs.iter().all(|run| run.as_ref().unwrap().caller.id == first.caller.id));
    let response = forwarded(first, "/environment").await;
    let bytes = axum::body::to_bytes(axum::body::Body::new(response.into_body()), 64 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for key in value["env"].as_object().unwrap().keys() {
        // macOS 的进程运行库会自行补充这一非敏感变量，不是宿主环境继承。
        if cfg!(target_os = "macos") && key == "__CF_USER_TEXT_ENCODING" { continue; }
        assert!(["PATH", "LANG", "LC_ALL", "HOME", "TMPDIR", "TGDRIVE_TEST_BACKEND_CONTEXT"].contains(&key.as_str()), "后端继承了非白名单环境变量 {key}");
    }
    assert_eq!(value["uid"], unsafe { libc::geteuid() });
    platform.backends.stop("test-backend").await.unwrap();
}

async fn test_file(platform: &Arc<AppPlatform>, fake: &Synthetic, bytes: &[u8]) -> ContentRef {
    let id = super::tests::file(platform, fake, 1, "input.bin", bytes).await;
    let mut connection = platform.host.vfs.db.pool.acquire().await.unwrap();
    let app = store::installation(&mut connection, "test-backend").await.unwrap();
    let node = store::reference(&mut connection, &app, &NodeReference::Id { id: id.to_string(), content_version: None }).await.unwrap();
    ContentRef { id: node.id, content_version: node.content_version.unwrap() }
}

#[tokio::test]
async fn local_open_range_and_immediate_seek_share_only_pulled_blocks() {
    use http_body_util::BodyExt;
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform.backends.ensure(&platform, "test-backend").await.unwrap();
    let file = test_file(&platform, &fake, &vec![7; 2 * 1024 * 1024]).await;
    let ticket = platform.local_url(&run.caller, LocalFileUrlRequest { file, ttl_seconds: None, byte_budget: Some((1024 * 1024).into()), request_budget: Some(4), lease_id: None }).await.unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("range", "bytes=0-".parse().unwrap());
    let mut first = platform.local_bytes(&ticket.id, &headers, false).await.unwrap();
    assert_eq!(first.status(), 206);
    assert_eq!(first.headers()["content-length"], "1048576");
    assert_eq!(first.body_mut().frame().await.unwrap().unwrap().into_data().unwrap().len(), 64 * 1024);
    headers.insert("range", "bytes=1500000-1500031".parse().unwrap());
    let second = platform.local_bytes(&ticket.id, &headers, false).await.unwrap();
    assert_eq!(second.status(), 206);
    assert_eq!(axum::body::to_bytes(second.into_body(), 64).await.unwrap().len(), 32);
    // 前一个响应尚未关闭且还声明剩余字节；撤销仍必须中止它，不依赖读取租约。
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    assert!(first.body_mut().frame().await.unwrap().is_err());
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn loopback_tcp_can_seek_before_previous_response_is_closed() {
    let (_db, _data, platform, fake) = backend_fixture().await;
    let run = platform.backends.ensure(&platform, "test-backend").await.unwrap();
    let file = test_file(&platform, &fake, &vec![8; 24 * 1024 * 1024]).await;
    let ticket = platform.local_url(&run.caller, LocalFileUrlRequest { file, ttl_seconds: None, byte_budget: None, request_budget: None, lease_id: None }).await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut first = client.get(&ticket.url).header("Range", "bytes=0-").send().await.unwrap();
    assert_eq!(first.status(), 206);
    assert!(!first.chunk().await.unwrap().unwrap().is_empty());
    let second = client.get(&ticket.url).header("Range", "bytes=20000000-20000031").send().await.unwrap();
    assert_eq!(second.status(), 206);
    assert_eq!(second.bytes().await.unwrap().len(), 32);
    drop(first);
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
async fn ffprobe_and_ffmpeg_share_the_same_local_address_budget() {
    let (_db, data, platform, fake) = backend_fixture().await;
    let executable = |name: &str| ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].iter()
        .map(|directory| std::path::Path::new(directory).join(name)).find(|path| path.is_file()).expect("验收环境需要真实 ffmpeg/ffprobe");
    let input = data.path().join("fixture.mp4");
    let output = tokio::process::Command::new(executable("ffmpeg"))
        .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=black:size=32x32:rate=5", "-t", "0.4", "-c:v", "mpeg4", "-movflags", "+faststart"])
        .arg(&input).output().await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let contents = std::fs::read(input).unwrap();
    let run = platform.backends.ensure(&platform, "test-backend").await.unwrap();
    let file = test_file(&platform, &fake, &contents).await;
    let ticket = platform.local_url(&run.caller, LocalFileUrlRequest { file, ttl_seconds: None, byte_budget: Some((1024 * 1024).into()), request_budget: None, lease_id: None }).await.unwrap();
    let mut probe = tokio::process::Command::new(executable("ffprobe"));
    probe.args(["-v", "error", "-show_format", "-show_streams"]).arg(&ticket.url).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), probe.output()).await.unwrap().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mut decode = tokio::process::Command::new(executable("ffmpeg"));
    decode.args(["-nostdin", "-v", "error", "-i"]).arg(&ticket.url).args(["-f", "null", "-"]).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), decode.output()).await.unwrap().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    platform.revoke_local_url(&run.caller, &ticket.id).unwrap();
    platform.backends.stop("test-backend").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn fixture_backend_main() {
    use tgdrive_backend_sdk::{BackendServer, HostClient, StartContext, UnixHostClient, json_response};
    let Some(path) = std::env::var_os("TGDRIVE_TEST_BACKEND_CONTEXT") else { return; };
    let context = StartContext::parse(&std::fs::read(path).unwrap(), CONTRACT_VERSION).unwrap();
    let host = Arc::new(UnixHostClient::new(&context));
    BackendServer::bind(context).unwrap().run(move |request| { let host = host.clone(); async move {
        match request.request.uri().path() {
            "/host-info" => json_response(200, &host.runtime_info().await?),
            "/environment" => { let values = std::env::vars().collect::<std::collections::BTreeMap<_, _>>(); json_response(200, &serde_json::json!({"uid": unsafe { libc::geteuid() }, "gid": unsafe { libc::getegid() }, "env": values})) },
            "/spawn" => { let child = tokio::process::Command::new("/bin/sleep").arg("30").spawn().unwrap(); json_response(200, &serde_json::json!({"pid": child.id().unwrap()})) },
            "/exit" => std::process::exit(42),
            _ => json_response(200, &request.context),
        }
    } }, |_| async {}).await.unwrap();
}
