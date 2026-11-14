//! 真实 SQLite、内存 Telegram 与本机 MetaTube 夹具；不读取生产配置或网盘。
use super::*;
use crate::apps::scraper;
use axum::routing::get;
use tokio::io::AsyncReadExt as _;

pub(super) struct BackendFixture {
    pub(super) url: String,
    task: tokio::task::JoinHandle<()>,
    pub(super) fail_poster: Arc<AtomicBool>,
    details: Arc<AtomicUsize>,
}
impl Drop for BackendFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn backend_fixture() -> BackendFixture {
    let image = image::RgbImage::from_pixel(358, 538, image::Rgb([35, 140, 210]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&image)
        .unwrap();
    let jpeg = Arc::new(jpeg);
    let fail = Arc::new(AtomicBool::new(false));
    let details = Arc::new(AtomicUsize::new(0));
    let picture = jpeg.clone();
    let fails = fail.clone();
    let count = details.clone();
    let router = Router::new()
        .route("/v1/providers", get(|| async { Json(json!({"data":{"actor_providers":{},"movie_providers":{"Fixture":"https://fixture.invalid"}}})) }))
        .route("/v1/movies/search", get(|| async { Json(json!({"data":[{"provider":"Fixture","id":"abc123","number":"ABC-123","title":"星海远航"}]})) }))
        .route("/v1/movies/{provider}/{id}", get(move || {
            let count = count.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":{"provider":"Fixture","id":"abc123","number":"ABC-123","title":"星海远航","summary":"完整长简介。".repeat(5000),"actors":["演员甲","演员乙"],"maker":"测试厂商","label":"测试品牌","series":"测试系列","genres":["冒险"],"score":5,"release_date":"2025-06-01T00:00:00Z"}}))
            }
        }))
        .route("/v1/images/primary/{provider}/{id}", get(move || {
            let bytes = picture.clone(); let fails = fails.clone(); async move {
                if fails.load(Ordering::SeqCst) { (StatusCode::SERVICE_UNAVAILABLE, Vec::<u8>::new()).into_response() }
                else { ([(header::CONTENT_TYPE, "image/jpeg")], bytes.as_ref().clone()).into_response() }
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    BackendFixture {
        url,
        task,
        fail_poster: fail,
        details,
    }
}

fn scraper_manifest() -> Manifest {
    let mut m = manifest();
    m.id = scraper::APP_ID.into();
    m.name = "影视刮削测试".into();
    m.min_host_version = "0.3.10".into();
    m.permissions = vec![
        "files.read".into(),
        "media.read".into(),
        "scraper.service".into(),
    ];
    m.settings = vec![package::SettingField {
        key: "backend_url".into(),
        label: "后端".into(),
        description: String::new(),
        kind: "string".into(),
        default: Value::String("https://metatube-server.hf.space/".into()),
    }];
    m
}
async fn setup() -> (
    AppState,
    Arc<MemoryMedia>,
    tempfile::TempDir,
    InstalledApp,
    BackendFixture,
) {
    let (state, media, dir) = fixture().await;
    let app = add_app(&state, &scraper_manifest(), None).await;
    let backend = backend_fixture().await;
    invoke(
        &state,
        &app,
        "scraper.config.set",
        json!({"backend_url":backend.url}),
    )
    .await
    .unwrap();
    state.vfs.mkdir("/刮削测试").await.unwrap();
    (state, media, dir, app, backend)
}
async fn invoke(
    state: &AppState,
    app: &InstalledApp,
    method: &str,
    params: Value,
) -> AppResult<Value> {
    scraper::invoke(state, app, method, params).await
}
async fn reference(state: &AppState, id: i64) -> Value {
    let node = state.vfs.db.node(id).await.unwrap();
    json!({"id":id,"path":state.vfs.db.full_path_if_visible(id).await.unwrap().unwrap(),"content_version":node.content_version()})
}
fn candidate() -> Value {
    json!({"provider":"Fixture","id":"abc123","number":"ABC-123","title":"星海远航"})
}
async fn batch(state: &AppState, app: &InstalledApp) -> Value {
    let root = state.vfs.resolve("/刮削测试").await.unwrap();
    invoke(
        state,
        app,
        "scraper.batch.create",
        reference(state, root.id).await,
    )
    .await
    .unwrap()
}
async fn prepare(
    state: &AppState,
    app: &InstalledApp,
    batch: &Value,
    ids: &[i64],
    refresh: bool,
    poster: bool,
    shared: &[i64],
) -> Value {
    let mut files = Vec::new();
    for id in ids {
        files.push(reference(state, *id).await);
    }
    invoke(state, app, "scraper.preview", json!({"batch":batch,"files":files,"number":"ABC-123","candidate":candidate(),"refresh":refresh,"update_nfo":true,"update_poster":poster,"shared_ids":shared})).await.unwrap()
}
async fn start(state: &AppState, app: &InstalledApp, batch: &Value) {
    invoke(state, app, "scraper.batch.begin", batch.clone())
        .await
        .unwrap();
}
async fn next(state: &AppState, app: &InstalledApp, batch: &Value) -> Value {
    invoke(state, app, "scraper.next", batch.clone())
        .await
        .unwrap()
}
async fn run(state: &AppState, app: &InstalledApp, batch: &Value) {
    for _ in 0..30 {
        let step = next(state, app, batch).await;
        if step["done"] == true || step["paused"] == true {
            return;
        }
    }
    panic!("有界夹具步骤未结束");
}
async fn contents(state: &AppState, path: &str) -> Vec<u8> {
    let node = state.vfs.resolve(path).await.unwrap();
    let mut bytes = Vec::new();
    state
        .vfs
        .open_reader(&node, 0)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    bytes
}

#[tokio::test]
async fn manual_batch_moves_whole_group_and_writes_complete_sidecars() {
    let (state, media, _dir, app, backend) = setup().await;
    let mp4 = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.mp4",
        b"synthetic mp4",
        1024,
    )
    .await;
    let mkv = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.mkv",
        b"synthetic mkv",
        1024,
    )
    .await;
    let subtitle = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.zh.srt",
        b"synthetic captions",
        1024,
    )
    .await;
    let version = state.vfs.db.node(mp4).await.unwrap().content_version();
    let b = batch(&state, &app).await;
    assert_eq!(
        prepare(&state, &app, &b, &[mp4], false, true, &[]).await["ready"],
        false
    );
    let initial_queries = backend.details.load(Ordering::SeqCst);
    assert_eq!(
        prepare(&state, &app, &b, &[mp4, mkv], false, true, &[]).await["ready"],
        true
    );
    assert_eq!(
        prepare(&state, &app, &b, &[mp4, mkv], false, true, &[]).await["ready"],
        true
    );
    assert!(
        state.vfs.resolve("/刮削测试/ABC-123").await.is_err(),
        "预览不能创建目录"
    );
    start(&state, &app, &b).await;
    run(&state, &app, &b).await;
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mp4")
            .await
            .unwrap()
            .id,
        mp4
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mkv")
            .await
            .unwrap()
            .id,
        mkv
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.zh.srt")
            .await
            .unwrap()
            .id,
        subtitle
    );
    assert_eq!(
        state.vfs.db.node(mp4).await.unwrap().content_version(),
        version
    );
    let nfo = String::from_utf8(contents(&state, "/刮削测试/ABC-123/ABC-123.nfo").await).unwrap();
    assert_eq!(
        crate::apps::nfo::text(&nfo, "plot").unwrap(),
        "完整长简介。".repeat(5000)
    );
    assert_eq!(
        crate::apps::nfo::text(&nfo, "year").as_deref(),
        Some("2025")
    );
    let jpeg = contents(&state, "/刮削测试/ABC-123/ABC-123.jpg").await;
    let image = image::load_from_memory(&jpeg).unwrap();
    assert_eq!(image.width() * 3, image.height() * 2);
    assert!(jpeg.len() <= 2 * 1024 * 1024);
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 2);
    assert_eq!(backend.details.load(Ordering::SeqCst), initial_queries + 1);

    // 验证实际飞牛索引也能持久保存超过旧单项上限的完整简介。
    let mut fntv_manifest = manifest();
    fntv_manifest.id = "fntv".into();
    fntv_manifest.permissions = vec![
        "files.read".into(),
        "media.read".into(),
        "fntv.service".into(),
    ];
    fntv_manifest.min_host_version = "0.3.10".into();
    fntv_manifest.settings.clear();
    let fntv_app = add_app(&state, &fntv_manifest, None).await;
    let root = state.vfs.resolve("/刮削测试").await.unwrap();
    crate::apps::fntv::invoke(
        &state,
        &fntv_app,
        "fntv.account.set",
        json!({"username":"scraper-test","password":"Synthetic-Fntv-2026"}),
    )
    .await
    .unwrap();
    crate::apps::fntv::invoke(
        &state,
        &fntv_app,
        "fntv.libraries.set",
        json!({"libraries":[{"id":"","name":"刮削结果","kind":"movie","root_id":root.id}]}),
    )
    .await
    .unwrap();
    crate::apps::fntv::invoke(&state, &fntv_app, "fntv.service.start", json!({}))
        .await
        .unwrap();
    crate::apps::fntv::invoke(&state, &fntv_app, "fntv.index.refresh", json!({}))
        .await
        .unwrap();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM app_data WHERE app_id='fntv' AND key LIKE 'fntv:index:%'",
    )
    .fetch_all(&state.vfs.db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let item: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(item["title"], "星海远航");
        assert_eq!(item["year"], "2025");
        assert_eq!(item["overview"], "完整长简介。".repeat(5000));
    }
    crate::apps::fntv::invoke(&state, &fntv_app, "fntv.service.stop", json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn dropped_request_can_be_stopped_without_exposing_an_internal_upload_to_the_web() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.mp4",
        b"synthetic video",
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    prepare(&state, &app, &operation, &[video], false, true, &[]).await;
    start(&state, &app, &operation).await;
    next(&state, &app, &operation).await;
    media.upload_block.store(true, Ordering::SeqCst);
    let task_state = state.clone();
    let task_app = app.clone();
    let task_batch = operation.clone();
    let pending = tokio::spawn(async move { next(&task_state, &task_app, &task_batch).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        media.upload_started.notified(),
    )
    .await
    .unwrap();
    assert!(
        state
            .vfs
            .db
            .visible_upload_sessions()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.vfs.db.upload_sessions().await.unwrap().len(), 1);
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(
        invoke(&state, &app, "scraper.stop", operation.clone())
            .await
            .unwrap()["stopped"],
        true
    );
    let record = invoke(
        &state,
        &app,
        "scraper.record",
        json!({"id":operation["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(record["status"], "cancelled");
    assert_eq!(record["items"][0]["status"], "needs_verification");
    let upload = record["items"][0]["result"]["nfo"]["upload_id"]
        .as_str()
        .unwrap();
    assert!(state.vfs.db.complete_upload_session(upload).await.is_err());
    if media.upload_pending.load(Ordering::SeqCst) {
        media.upload_release.notify_one();
    }
    let verified = invoke(
        &state,
        &app,
        "scraper.verify",
        json!({"id":operation["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(verified["items"][0]["result"]["nfo"]["state"], "cancelled");
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn restored_inflight_records_discard_only_uncommitted_shadows_and_allow_a_new_scan() {
    let (state, media, dir, app, _backend) = setup().await;
    let video = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.mp4",
        b"synthetic video",
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    prepare(&state, &app, &operation, &[video], false, true, &[]).await;
    start(&state, &app, &operation).await;
    next(&state, &app, &operation).await;
    media.upload_block.store(true, Ordering::SeqCst);
    let task_state = state.clone();
    let task_app = app.clone();
    let task_batch = operation.clone();
    let pending = tokio::spawn(async move { next(&task_state, &task_app, &task_batch).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        media.upload_started.notified(),
    )
    .await
    .unwrap();
    let snapshot = dir.path().join("inflight-restored.db");
    state
        .vfs
        .db
        .vacuum_into(snapshot.to_str().unwrap())
        .await
        .unwrap();
    crate::backup::prepare_inbox_snapshot(snapshot.to_str().unwrap())
        .await
        .unwrap();
    let restored = state_at(&snapshot, media.clone()).await;
    let restored_app = installed(&restored, scraper::APP_ID).await.unwrap();
    assert!(restored.vfs.db.upload_sessions().await.unwrap().is_empty());
    assert_eq!(
        restored
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mp4")
            .await
            .unwrap()
            .id,
        video
    );
    let historical = invoke(
        &restored,
        &restored_app,
        "scraper.record",
        json!({"id":operation["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(historical["read_only"], true);
    assert_eq!(
        historical["items"][0]["result"]["nfo"]["state"],
        "cancelled"
    );
    media.upload_block.store(false, Ordering::SeqCst);
    media.upload_release.notify_one();
    pending.await.unwrap();
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_ok(),
        "备份清洗不得撤回原实例后来完成的文件"
    );
    let fresh = batch(&restored, &restored_app).await;
    assert_eq!(
        prepare(&restored, &restored_app, &fresh, &[video], false, true, &[]).await["ready"],
        true
    );
    start(&restored, &restored_app, &fresh).await;
    run(&restored, &restored_app, &fresh).await;
    assert!(
        restored
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn external_changes_after_preview_do_not_move_any_group_member() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    seed_file(&state, &media, "/刮削测试/ABC-123.srt", b"subtitles", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[video], false, true, &[]).await;
    state
        .vfs
        .rename("/刮削测试/ABC-123.srt", "/刮削测试/ABC-123.en.srt")
        .await
        .unwrap();
    start(&state, &app, &b).await;
    assert_eq!(next(&state, &app, &b).await["status"], "needs_user");
    assert_eq!(
        state.vfs.resolve("/刮削测试/ABC-123.mp4").await.unwrap().id,
        video
    );
    assert!(state.vfs.resolve("/刮削测试/ABC-123").await.is_err());
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rescraping_replaces_the_reviewed_uppercase_nfo_at_its_actual_name() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.NFO",
        "<movie><num>ABC-123</num><title>旧手工标题</title></movie>".as_bytes(),
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    let preview = prepare(&state, &app, &operation, &[video], true, false, &[]).await;
    assert_eq!(preview["nfo"]["name"], "ABC-123.NFO");
    start(&state, &app, &operation).await;
    run(&state, &app, &operation).await;
    let xml = String::from_utf8(contents(&state, "/刮削测试/ABC-123/ABC-123.NFO").await).unwrap();
    assert_eq!(
        crate::apps::nfo::text(&xml, "title").as_deref(),
        Some("星海远航")
    );
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn overlapping_subtitle_and_nfo_aliases_are_not_moved_without_an_explicit_choice() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let other = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.zh.mp4",
        b"other video",
        1024,
    )
    .await;
    let poster_video = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123-poster.mp4",
        b"poster version",
        1024,
    )
    .await;
    let captions = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.zh.srt",
        b"shared captions",
        1024,
    )
    .await;
    let other_nfo = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123-poster.nfo",
        b"<movie><num>ABC-123</num></movie>",
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    let preview = prepare(&state, &app, &operation, &[video], false, true, &[]).await;
    assert_eq!(preview["ready"], true);
    assert!(
        !preview["moving"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["id"] == captions || file["id"] == other_nfo)
    );
    start(&state, &app, &operation).await;
    next(&state, &app, &operation).await;
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123.zh.mp4")
            .await
            .unwrap()
            .id,
        other
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123-poster.mp4")
            .await
            .unwrap()
            .id,
        poster_video
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123.zh.srt")
            .await
            .unwrap()
            .id,
        captions
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123-poster.nfo")
            .await
            .unwrap()
            .id,
        other_nfo
    );
}

#[tokio::test]
async fn adding_a_video_that_shares_a_caption_after_preview_invalidates_the_old_plan() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.zh.srt",
        b"captions",
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    prepare(&state, &app, &operation, &[video], false, true, &[]).await;
    seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.zh.mp4",
        b"new version",
        1024,
    )
    .await;
    start(&state, &app, &operation).await;
    assert_eq!(next(&state, &app, &operation).await["status"], "needs_user");
    assert!(state.vfs.resolve("/刮削测试/ABC-123.mp4").await.is_ok());
    assert!(state.vfs.resolve("/刮削测试/ABC-123").await.is_err());
}

#[tokio::test]
async fn plans_cannot_create_a_poster_alias_that_changes_another_selected_versions_cover() {
    let (state, media, _dir, app, _backend) = setup().await;
    let first = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"one", 1024).await;
    let second = seed_file(&state, &media, "/刮削测试/ABC-123-poster.mp4", b"two", 1024).await;
    let operation = batch(&state, &app).await;
    assert_eq!(
        prepare(&state, &app, &operation, &[first], false, true, &[]).await["ready"],
        true
    );
    assert_eq!(
        prepare(&state, &app, &operation, &[second], false, true, &[]).await["ready"],
        true
    );
    assert!(
        invoke(&state, &app, "scraper.batch.begin", operation.clone())
            .await
            .is_err()
    );
    assert!(state.vfs.resolve("/刮削测试/ABC-123").await.is_err());
}

#[tokio::test]
async fn same_number_versions_share_metadata_and_reuse_own_directory() {
    let (state, media, _dir, app, backend) = setup().await;
    let first = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123-CD1.mp4",
        b"part one",
        1024,
    )
    .await;
    let second = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123-4K.mp4",
        b"version two",
        1024,
    )
    .await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[first], false, true, &[]).await;
    prepare(&state, &app, &b, &[second], false, true, &[]).await;
    assert_eq!(backend.details.load(Ordering::SeqCst), 1);
    start(&state, &app, &b).await;
    run(&state, &app, &b).await;
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123-CD1.mp4")
            .await
            .unwrap()
            .id,
        first
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123-4K.mp4")
            .await
            .unwrap()
            .id,
        second
    );
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn shared_files_move_once_for_multiple_approved_versions() {
    let (state, media, _dir, app, _backend) = setup().await;
    let first = seed_file(&state, &media, "/刮削测试/ABC-123-CD1.mp4", b"one", 1024).await;
    let second = seed_file(&state, &media, "/刮削测试/ABC-123-CD2.mp4", b"two", 1024).await;
    let nfo = seed_file(
        &state,
        &media,
        "/刮削测试/movie.nfo",
        b"<movie><num>ABC-123</num><title>existing shared</title></movie>",
        1024,
    )
    .await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[first], false, false, &[nfo]).await;
    prepare(&state, &app, &b, &[second], false, false, &[nfo]).await;
    start(&state, &app, &b).await;
    run(&state, &app, &b).await;
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123-CD2.mp4")
            .await
            .unwrap()
            .id,
        second
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/movie.nfo")
            .await
            .unwrap()
            .id,
        nfo
    );
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn backend_image_failure_pauses_batch_and_keeps_completed_organization() {
    let (state, media, _dir, app, backend) = setup().await;
    let first = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"one", 1024).await;
    let second = seed_file(&state, &media, "/刮削测试/ABC-123-4K.mp4", b"two", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[first], false, true, &[]).await;
    prepare(&state, &app, &b, &[second], false, true, &[]).await;
    backend.fail_poster.store(true, Ordering::SeqCst);
    start(&state, &app, &b).await;
    run(&state, &app, &b).await;
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mp4")
            .await
            .unwrap()
            .id,
        first
    );
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_ok()
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123-4K.mp4")
            .await
            .unwrap()
            .id,
        second
    );
    assert_eq!(
        invoke(&state, &app, "scraper.record", json!({"id":b["id"]}))
            .await
            .unwrap()["status"],
        "paused"
    );
}

#[tokio::test]
async fn one_active_batch_and_configuration_are_checked_in_database() {
    let (state, media, _dir, app, backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let first = batch(&state, &app).await;
    let second = batch(&state, &app).await;
    prepare(&state, &app, &first, &[video], false, true, &[]).await;
    prepare(&state, &app, &second, &[video], false, true, &[]).await;
    start(&state, &app, &first).await;
    assert!(
        invoke(&state, &app, "scraper.batch.begin", second)
            .await
            .is_err()
    );
    assert!(
        invoke(
            &state,
            &app,
            "scraper.config.set",
            json!({"backend_url":backend.url,"token":"new-token"})
        )
        .await
        .is_err()
    );
    assert!(
        state
            .vfs
            .db
            .patch_app_settings(
                scraper::APP_ID,
                &app.revision,
                "{\"backend_url\":\"https://changed.invalid/\"}"
            )
            .await
            .is_err()
    );
    invoke(&state, &app, "scraper.stop", first).await.unwrap();
}

#[tokio::test]
async fn overwrite_commit_cannot_replace_a_file_changed_after_preview() {
    let (state, media, _dir, app, _backend) = setup().await;
    state.vfs.mkdir("/刮削测试/ABC-123").await.unwrap();
    let video = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123/ABC-123.mp4",
        b"video",
        1024,
    )
    .await;
    seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123/ABC-123.nfo",
        b"<movie><title>old</title></movie>",
        1024,
    )
    .await;
    let updated = b"<movie><title>new manual edit</title></movie>";
    seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123/user-new.nfo",
        updated,
        1024,
    )
    .await;
    let b = batch(&state, &app).await;
    assert_eq!(
        prepare(&state, &app, &b, &[video], true, false, &[]).await["ready"],
        true
    );
    start(&state, &app, &b).await;
    next(&state, &app, &b).await;
    media.upload_block.store(true, Ordering::SeqCst);
    let executing = {
        let state = state.clone();
        let app = app.clone();
        let b = b.clone();
        tokio::spawn(async move { invoke(&state, &app, "scraper.next", b).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        media.upload_started.notified(),
    )
    .await
    .unwrap();
    state
        .vfs
        .rename("/刮削测试/ABC-123/ABC-123.nfo", "/刮削测试/ABC-123/old.nfo")
        .await
        .unwrap();
    state
        .vfs
        .rename(
            "/刮削测试/ABC-123/user-new.nfo",
            "/刮削测试/ABC-123/ABC-123.nfo",
        )
        .await
        .unwrap();
    media.upload_release.notify_one();
    let response = tokio::time::timeout(std::time::Duration::from_secs(15), executing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response["status"], "needs_user");
    assert_eq!(
        contents(&state, "/刮削测试/ABC-123/ABC-123.nfo").await,
        updated
    );
}

#[tokio::test]
async fn cancelling_inflight_upload_blocks_commit_and_requires_verification() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[video], false, true, &[]).await;
    start(&state, &app, &b).await;
    next(&state, &app, &b).await;
    media.upload_block.store(true, Ordering::SeqCst);
    let executing = {
        let state = state.clone();
        let app = app.clone();
        let b = b.clone();
        tokio::spawn(async move { invoke(&state, &app, "scraper.next", b).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        media.upload_started.notified(),
    )
    .await
    .unwrap();
    let stopping = invoke(&state, &app, "scraper.stop", b.clone())
        .await
        .unwrap();
    assert_eq!(stopping["stopped"], false);
    let response = tokio::time::timeout(std::time::Duration::from_secs(15), executing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response["paused"], true);
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_err()
    );
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mp4")
            .await
            .unwrap()
            .id,
        video
    );
    assert!(
        invoke(&state, &app, "scraper.next", b.clone())
            .await
            .is_err()
    );
    let verified = invoke(&state, &app, "scraper.verify", json!({"id":b["id"]}))
        .await
        .unwrap();
    assert_eq!(verified["items"][0]["status"], "needs_user");
}

#[tokio::test]
async fn deleting_history_cannot_remove_an_inflight_uploads_commit_fence() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[video], false, true, &[]).await;
    start(&state, &app, &b).await;
    next(&state, &app, &b).await;
    media.upload_block.store(true, Ordering::SeqCst);
    let executing = {
        let state = state.clone();
        let app = app.clone();
        let b = b.clone();
        tokio::spawn(async move { invoke(&state, &app, "scraper.next", b).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        media.upload_started.notified(),
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM app_scraper_batches WHERE id=?")
        .bind(b["id"].as_str().unwrap())
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    let guards: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_scraper_upload_guards")
        .fetch_one(&state.vfs.db.pool)
        .await
        .unwrap();
    assert_eq!(guards, 1, "删除记录不能将影子上传降为普通覆盖上传");
    media.upload_release.notify_one();
    let response = tokio::time::timeout(std::time::Duration::from_secs(15), executing)
        .await
        .unwrap()
        .unwrap();
    assert!(response.is_err());
    assert!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.nfo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn token_is_private_and_restored_binding_cannot_reuse_it() {
    let (state, media, dir, app, backend) = setup().await;
    let token = "fixture-private-scraper-secret";
    let config = invoke(
        &state,
        &app,
        "scraper.config.set",
        json!({"backend_url":backend.url,"token":token}),
    )
    .await
    .unwrap();
    assert_eq!(config["token_configured"], true);
    assert!(!config.to_string().contains(token));
    assert!(
        !state
            .vfs
            .db
            .app_settings(scraper::APP_ID)
            .await
            .unwrap()
            .contains(token)
    );
    let video = seed_file(
        &state,
        &media,
        "/刮削测试/ABC-123.mp4",
        b"synthetic video",
        1024,
    )
    .await;
    let operation = batch(&state, &app).await;
    prepare(&state, &app, &operation, &[video], false, true, &[]).await;
    start(&state, &app, &operation).await;
    next(&state, &app, &operation).await;
    let snapshot = dir.path().join("restored.db");
    state
        .vfs
        .db
        .vacuum_into(snapshot.to_str().unwrap())
        .await
        .unwrap();
    let bytes = tokio::fs::read(&snapshot).await.unwrap();
    assert!(
        !bytes
            .windows(token.len())
            .any(|window| window == token.as_bytes())
    );
    crate::backup::prepare_inbox_snapshot(snapshot.to_str().unwrap())
        .await
        .unwrap();
    let secret = format!("{}.scraper-token", state.rt.env.db_path);
    tokio::fs::copy(&secret, format!("{}.scraper-token", snapshot.display()))
        .await
        .unwrap();
    let restored = state_at(&snapshot, media).await;
    let restored_app = installed(&restored, scraper::APP_ID).await.unwrap();
    assert_eq!(
        invoke(&restored, &restored_app, "scraper.config.get", json!({}))
            .await
            .unwrap()["token_configured"],
        false
    );
    let record = invoke(
        &restored,
        &restored_app,
        "scraper.record",
        json!({"id":operation["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(record["read_only"], true);
    assert_eq!(record["status"], "interrupted");
    assert!(
        invoke(
            &restored,
            &restored_app,
            "scraper.verify",
            json!({"id":operation["id"]})
        )
        .await
        .is_err()
    );
    state
        .vfs
        .db
        .uninstall_app(scraper::APP_ID, true)
        .await
        .unwrap();
    assert!(!std::path::Path::new(&secret).exists());
}

#[tokio::test]
async fn expiry_and_restart_never_resume_a_batch_or_delete_its_files() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[video], false, true, &[]).await;
    start(&state, &app, &b).await;
    next(&state, &app, &b).await;
    sqlx::query("UPDATE app_scraper_batches SET generation='previous-process' WHERE id=?")
        .bind(b["id"].as_str().unwrap())
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    invoke(&state, &app, "scraper.records", json!({}))
        .await
        .unwrap();
    assert_eq!(
        invoke(&state, &app, "scraper.record", json!({"id":b["id"]}))
            .await
            .unwrap()["status"],
        "interrupted"
    );
    assert!(
        invoke(&state, &app, "scraper.next", b.clone())
            .await
            .is_err()
    );
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 0);
    sqlx::query("UPDATE app_scraper_batches SET updated_at=? WHERE id=?")
        .bind(crate::db::now_ts() - 31 * 86400)
        .bind(b["id"].as_str().unwrap())
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    let records = invoke(&state, &app, "scraper.records", json!({}))
        .await
        .unwrap();
    assert_eq!(records["records"].as_array().unwrap().len(), 0);
    assert_eq!(
        state
            .vfs
            .resolve("/刮削测试/ABC-123/ABC-123.mp4")
            .await
            .unwrap()
            .id,
        video
    );
}

#[tokio::test]
async fn history_pagination_keeps_batches_created_in_the_same_second() {
    let (state, _media, _dir, app, _backend) = setup().await;
    let mut expected = std::collections::HashSet::new();
    for _ in 0..4 {
        let b = batch(&state, &app).await;
        expected.insert(b["id"].as_str().unwrap().to_string());
    }
    sqlx::query("UPDATE app_scraper_batches SET created_at=100")
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    let mut cursor = Value::Null;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..4 {
        let page = invoke(
            &state,
            &app,
            "scraper.records",
            json!({"limit":1,"cursor":cursor}),
        )
        .await
        .unwrap();
        seen.insert(page["records"][0]["id"].as_str().unwrap().to_string());
        cursor = page["next_cursor"].clone();
    }
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn revoked_directory_scope_blocks_next_step_without_moving_original_video() {
    let (state, media, _dir, app, _backend) = setup().await;
    let video = seed_file(&state, &media, "/刮削测试/ABC-123.mp4", b"video", 1024).await;
    let b = batch(&state, &app).await;
    prepare(&state, &app, &b, &[video], false, true, &[]).await;
    start(&state, &app, &b).await;
    state
        .vfs
        .db
        .set_app_scope_mode(scraper::APP_ID, "none", "[]", &app.revision)
        .await
        .unwrap();
    assert!(invoke(&state, &app, "scraper.next", b).await.is_err());
    assert_eq!(
        state.vfs.resolve("/刮削测试/ABC-123.mp4").await.unwrap().id,
        video
    );
    assert_eq!(media.upload_count.load(Ordering::SeqCst), 0);
}
