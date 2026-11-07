//! 封面来源、真实取帧、缓存与生命周期的隔离集成验证。
use super::*;
use std::time::{Duration, Instant};

const LIBRARY: &str = "22222222222222222222222222222222";

fn png(color: [u8; 3]) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(320, 180, image::Rgb(color));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn noise_png() -> Vec<u8> {
    let mut seed = 0x51f0a391_u32;
    let image = image::RgbImage::from_fn(256, 128, |_, _| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        image::Rgb([seed as u8, (seed >> 8) as u8, (seed >> 16) as u8])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn video(filter: &str, seconds: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.mp4");
    let output = std::process::Command::new("ffmpeg")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            filter,
            "-t",
            seconds,
            "-an",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
        ])
        .arg(&path)
        .output()
        .expect("封面验证需要 ffmpeg");
    assert!(
        output.status.success(),
        "合成视频生成失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read(path).unwrap()
}

async fn setup(
    files: &[(&str, Vec<u8>)],
    kind: &str,
) -> (AppState, Arc<MemoryMedia>, tempfile::TempDir) {
    let (state, media, dir) = fixture().await;
    state.vfs.mkdir("/封面验证").await.unwrap();
    for (name, bytes) in files {
        seed_file(
            &state,
            &media,
            &format!("/封面验证/{name}"),
            bytes,
            256 * 1024,
        )
        .await;
    }
    add_app(&state, &fntv_manifest(), None).await;
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/fntv/scope",
            json!({"mode":"selected","paths":["/封面验证"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        manager(
            &state,
            "fntv.account.set",
            json!({"username":"local-test","password":"Synthetic-Only-Account"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let root = state.vfs.resolve("/封面验证").await.unwrap().id;
    assert_eq!(
        manager(
            &state,
            "fntv.libraries.set",
            json!({"libraries":[{"id":LIBRARY,"name":"隔离封面库","kind":kind,"root_id":root}]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    (state, media, dir)
}

async fn start(state: &AppState) -> String {
    assert_eq!(
        manager(state, "fntv.service.start", json!({})).await.0,
        StatusCode::OK
    );
    login(state, "Synthetic-Only-Account").await
}

async fn wait(state: &AppState, ready: u64, failed: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(18), async {
        loop {
            let (status, body) = manager(state, "fntv.service.status", json!({})).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let covers = &body["result"]["covers"];
            // RPC 的成功字段由通用夹具保留在 result 内。
            let covers = if covers.is_null() {
                &body["covers"]
            } else {
                covers
            };
            if covers["ready"].as_u64() == Some(ready)
                && covers["failed"].as_u64() == Some(failed)
                && covers["pending"] == 0
            {
                return covers.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("封面后台任务没有在预算内完成")
}

async fn list(state: &AppState, token: &str, ancestor: &str) -> Vec<Value> {
    let (status, body) = json_response(
        wire(
            state,
            "POST",
            "/v/api/v1/item/list",
            Some(token),
            json!({"ancestor_guid":ancestor,"page":1,"page_size":100}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]["list"].as_array().unwrap().clone()
}

async fn picture(state: &AppState, token: &str, path: &str) -> Vec<u8> {
    let response = wire(state, "GET", path, Some(token), Value::Null, &[]).await;
    assert_eq!(response.status(), StatusCode::OK, "封面地址 {path}");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

async fn saved_states(state: &AppState) -> Vec<Value> {
    state
        .vfs
        .db
        .cover_states("fntv")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, meta)| serde_json::from_str(&meta).unwrap())
        .collect()
}

#[tokio::test]
async fn black_intro_is_skipped_in_favor_of_a_later_nonblack_frame() {
    let bytes = video(
        "color=c=red:size=160x90:rate=25,drawbox=x=0:y=0:w=iw:h=ih:color=black:t=fill:enable='lt(t,2)'",
        "6",
    );
    let (state, _, _dir) = setup(&[("intro.mp4", bytes)], "movie").await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let image =
        image::load_from_memory(&picture(&state, &token, item["poster"].as_str().unwrap()).await)
            .unwrap()
            .to_rgb8();
    assert!(image.get_pixel(40, 40)[0] > 200 && image.get_pixel(40, 40)[1] < 40);
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn generated_cover_preserves_display_aspect_for_nonsquare_video_pixels() {
    let bytes = video("color=c=red:size=160x90:rate=25,setsar=2/1", "1");
    let (state, _, _dir) = setup(&[("aspect.mp4", bytes)], "movie").await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let image =
        image::load_from_memory(&picture(&state, &token, item["poster"].as_str().unwrap()).await)
            .unwrap();
    assert_eq!(image.width() * 9, image.height() * 32);
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn moving_poster_out_of_video_directory_stops_an_existing_cached_response() {
    let (state, _, _dir) = setup(
        &[
            ("clip.mp4", b"synthetic".to_vec()),
            ("poster.png", noise_png()),
        ],
        "movie",
    )
    .await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let response = wire(
        &state,
        "GET",
        item["poster"].as_str().unwrap(),
        Some(&token),
        Value::Null,
        &[],
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    state.vfs.mkdir("/封面验证/other").await.unwrap();
    state
        .vfs
        .rename("/封面验证/poster.png", "/封面验证/other/poster.png")
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_err());
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn invalid_specific_poster_falls_through_to_valid_original_image() {
    let original = png([20, 60, 220]);
    let (state, _, _dir) = setup(
        &[
            ("clip.mp4", b"not a real video".to_vec()),
            ("clip-poster.jpg", b"broken JPEG candidate".to_vec()),
            ("poster.png", original.clone()),
        ],
        "movie",
    )
    .await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    assert_eq!(
        picture(&state, &token, item["poster"].as_str().unwrap()).await,
        original
    );
    assert_eq!(item["poster"], item["posters"]);
    assert_eq!(saved_states(&state).await[0]["selected"]["kind"], "sidecar");
    let detail = json_response(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/item/{}", item["guid"].as_str().unwrap()),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(detail.1["data"]["poster"], item["poster"]);
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn telegram_thumbnail_avoids_original_video_and_updates_when_reference_changes() {
    let (state, media, _dir) = setup(
        &[(
            "clip.mp4",
            b"synthetic video not needed for thumbnail".to_vec(),
        )],
        "movie",
    )
    .await;
    let node = state.vfs.resolve("/封面验证/clip.mp4").await.unwrap();
    let version = node.content_version();
    let green = png([10, 210, 30]);
    media
        .files
        .lock()
        .unwrap()
        .insert("synthetic-thumb-green".into(), green.clone());
    sqlx::query(
        "UPDATE chunks SET tg_thumbnail_file_id='synthetic-thumb-green' WHERE node_id=? AND idx=0",
    )
    .bind(node.id)
    .execute(&state.vfs.db.pool)
    .await
    .unwrap();
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let old = list(&state, &token, LIBRARY).await.remove(0);
    let old_path = old["poster"].as_str().unwrap();
    assert_eq!(picture(&state, &token, old_path).await, green);
    assert!(
        media.ranges.lock().unwrap().is_empty(),
        "取 Telegram 小图不能读取原视频"
    );
    let response = wire(&state, "HEAD", old_path, Some(&token), Value::Null, &[]).await;
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        wire(
            &state,
            "GET",
            old_path,
            Some(&token),
            Value::Null,
            &[("if-none-match", &etag)]
        )
        .await
        .status(),
        StatusCode::NOT_MODIFIED
    );
    let red = png([220, 30, 10]);
    media
        .files
        .lock()
        .unwrap()
        .insert("synthetic-thumb-red".into(), red.clone());
    sqlx::query(
        "UPDATE chunks SET tg_thumbnail_file_id='synthetic-thumb-red' WHERE node_id=? AND idx=0",
    )
    .bind(node.id)
    .execute(&state.vfs.db.pool)
    .await
    .unwrap();
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    wait(&state, 1, 0).await;
    let new = list(&state, &token, LIBRARY).await.remove(0);
    assert_ne!(old["poster"], new["poster"]);
    assert_eq!(old["media_guid"], new["media_guid"]);
    assert_eq!(
        state.vfs.db.node(node.id).await.unwrap().content_version(),
        version
    );
    assert_eq!(
        picture(&state, &token, new["poster"].as_str().unwrap()).await,
        red
    );
    assert_eq!(
        wire(&state, "GET", old_path, Some(&token), Value::Null, &[])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(media.ranges.lock().unwrap().is_empty());
    for method in ["covers.get", "covers.put", "covers.delete", "covers.stats"] {
        assert_eq!(
            manager(&state, method, json!({"keys":["x"]})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn pure_video_gets_real_bounded_jpeg_and_refresh_reuses_it() {
    let mut bytes = video("testsrc2=size=320x180:rate=25", "2");
    // 一个合法的大 free box：视频超过读取预算，封面仍可从前部取得。
    let extra = 12 * 1024 * 1024_u32;
    bytes.extend_from_slice(&extra.to_be_bytes());
    bytes.extend_from_slice(b"free");
    bytes.resize(bytes.len() + extra as usize - 8, 0);
    let (state, media, _dir) = setup(&[("pure.mp4", bytes)], "movie").await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let path = item["poster"].as_str().unwrap();
    let jpeg = picture(&state, &token, path).await;
    assert!(jpeg.len() <= 256 * 1024 && jpeg.starts_with(&[0xff, 0xd8]));
    let image = image::load_from_memory(&jpeg).unwrap();
    assert!(image.width() <= 640 && image.height() <= 640);
    assert_eq!(image.width() * 9, image.height() * 16);
    assert_eq!(saved_states(&state).await[0]["selected"]["kind"], "frame");
    let ranges = media.ranges.lock().unwrap().clone();
    assert!(ranges.iter().map(|(_, length)| length).sum::<u64>() <= 8 * 1024 * 1024);
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    wait(&state, 1, 0).await;
    assert_eq!(picture(&state, &token, path).await, jpeg);
    assert_eq!(*media.ranges.lock().unwrap(), ranges);
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn single_frame_video_uses_a_position_inside_the_available_frame() {
    let bytes = video("color=c=red:size=160x90:rate=25", "0.04");
    let (state, _, _dir) = setup(&[("short.mp4", bytes)], "movie").await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let jpeg = picture(&state, &token, item["poster"].as_str().unwrap()).await;
    assert!(
        image::load_from_memory(&jpeg)
            .unwrap()
            .to_rgb8()
            .get_pixel(20, 20)[0]
            > 200
    );
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn black_video_records_terminal_failure_without_breaking_original_bytes() {
    let bytes = video("color=c=black:size=160x90:rate=25", "1");
    let (state, media, _dir) = setup(&[("black.mp4", bytes.clone())], "movie").await;
    let token = start(&state).await;
    wait(&state, 0, 1).await;
    let record = saved_states(&state).await.remove(0);
    assert_eq!(record["next_retry"], 0);
    assert_eq!(record["attempts"], 1);
    let before = media.ranges.lock().unwrap().len();
    for _ in 0..3 {
        let item = list(&state, &token, LIBRARY).await.remove(0);
        assert_eq!(item["poster"], "");
    }
    assert_eq!(media.ranges.lock().unwrap().len(), before);
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let response = wire(
        &state,
        "GET",
        &format!(
            "/v/api/v1/media/range/{}",
            item["media_guid"].as_str().unwrap()
        ),
        Some(&token),
        Value::Null,
        &[("range", "bytes=0-31")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(response.into_body(), 64).await.unwrap().as_ref(),
        &bytes[..32]
    );
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn foreground_stream_pauses_background_and_stop_cancels_readers() {
    let bytes = video("testsrc2=size=320x180:rate=25", "3");
    let (state, media, _dir) = setup(&[("priority.mp4", bytes)], "movie").await;
    // 先用原文件响应持有前台优先权，再让后台读取正常速度运行。
    let token = start(&state).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let response = wire(
        &state,
        "GET",
        &format!(
            "/v/api/v1/media/range/{}",
            item["media_guid"].as_str().unwrap()
        ),
        Some(&token),
        Value::Null,
        &[("range", "bytes=0-31")],
    )
    .await;
    let before = media.ranges.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        media.ranges.lock().unwrap().len(),
        before,
        "后台不能占用前台传输期间的读取资源"
    );
    drop(response);
    wait(&state, 1, 0).await;
    assert_eq!(
        manager(&state, "fntv.service.stop", json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(media.active.load(Ordering::SeqCst), 0);
    assert_eq!(state.vfs.db.cover_stats("fntv").await.unwrap().entries, 1);
}

#[tokio::test]
async fn cache_survives_service_restart_and_uninstall_removes_only_cached_data() {
    let (state, media, dir) = setup(
        &[
            ("clip.mp4", b"synthetic".to_vec()),
            ("poster.png", png([100, 180, 20])),
        ],
        "movie",
    )
    .await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let original = picture(&state, &token, item["poster"].as_str().unwrap()).await;
    let count = media.ranges.lock().unwrap().len();
    manager(&state, "fntv.service.stop", json!({})).await;
    let reopened = state_at(&dir.path().join("test.db"), media.clone()).await;
    let next = start(&reopened).await;
    wait(&reopened, 1, 0).await;
    assert_eq!(
        picture(&reopened, &next, item["poster"].as_str().unwrap()).await,
        original
    );
    assert_eq!(media.ranges.lock().unwrap().len(), count);
    assert_eq!(
        call(&reopened, "DELETE", "/api/apps/fntv", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(
        reopened
            .vfs
            .db
            .cover_states("fntv")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened.vfs.db.cover_stats("fntv").await.unwrap().entries,
        0
    );
    assert!(reopened.vfs.resolve("/封面验证/clip.mp4").await.is_ok());
    assert!(reopened.vfs.resolve("/封面验证/poster.png").await.is_ok());
}

#[tokio::test]
async fn slow_source_deadline_cleans_up_background_readers() {
    let bytes = video("testsrc2=size=320x180:rate=25", "3");
    let (state, media, _dir) = setup(&[("slow.mp4", bytes)], "movie").await;
    media.rate.store(1, Ordering::SeqCst);
    let began = Instant::now();
    let _token = start(&state).await;
    wait(&state, 0, 1).await;
    assert!(began.elapsed() < Duration::from_secs(14));
    tokio::time::timeout(Duration::from_secs(2), async {
        while media.active.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(media.active.load(Ordering::SeqCst), 0);
    let record = saved_states(&state).await.remove(0);
    assert!(!record["error"].as_str().unwrap().is_empty(), "{record}");
    assert!(
        record["next_retry"].as_i64().unwrap() > 0
            || record["error"].as_str().unwrap().contains("10 秒"),
        "{record}"
    );
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn stop_during_capture_preserves_pending_work_and_restart_resumes_it() {
    let (state, media, _dir) = setup(
        &[("stop.mp4", video("testsrc2=size=160x90:rate=25", "1"))],
        "movie",
    )
    .await;
    let token = start(&state).await;
    wait(&state, 1, 0).await;
    let item = list(&state, &token, LIBRARY).await.remove(0);
    let key = item["poster"]
        .as_str()
        .unwrap()
        .trim_start_matches("/tgdrive-poster/")
        .to_owned();
    state.vfs.db.cover_delete("fntv", &[key]).await.unwrap();
    media.rate.store(1, Ordering::SeqCst);
    manager(&state, "fntv.index.refresh", json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while media.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stopped = Instant::now();
    manager(&state, "fntv.service.stop", json!({})).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while media.active.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(stopped.elapsed() < Duration::from_secs(2));
    assert_eq!(saved_states(&state).await[0]["state"], "queued");
    assert_eq!(saved_states(&state).await[0]["attempts"], 0);
    media.rate.store(0, Ordering::SeqCst);
    start(&state).await;
    wait(&state, 1, 0).await;
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn groups_choose_first_available_real_episode_cover_and_revocation_stops_cached_bytes() {
    let original = noise_png();
    assert!(original.len() > 64 * 1024 && original.len() < 512 * 1024);
    let (state, _, _dir) = setup(
        &[
            (
                "Story S01E01.mp4",
                video("color=c=black:size=160x90:rate=25", "1"),
            ),
            ("Story S01E02.mp4", b"synthetic".to_vec()),
            ("Story S01E02.jpg", original.clone()),
        ],
        "tv",
    )
    .await;
    let token = start(&state).await;
    wait(&state, 1, 1).await;
    let show = list(&state, &token, LIBRARY).await.remove(0);
    let season = list(&state, &token, show["guid"].as_str().unwrap())
        .await
        .into_iter()
        .find(|item| item["type"] == "Season")
        .unwrap();
    let episodes = list(&state, &token, season["guid"].as_str().unwrap()).await;
    let first = episodes.iter().find(|e| e["episode_number"] == 1).unwrap();
    let second = episodes.iter().find(|e| e["episode_number"] == 2).unwrap();
    assert_eq!(first["poster"], "");
    assert_eq!(show["poster"], second["poster"]);
    assert_eq!(season["poster"], second["poster"]);
    assert_eq!(
        picture(&state, &token, show["poster"].as_str().unwrap()).await,
        original
    );
    let response = wire(
        &state,
        "GET",
        show["poster"].as_str().unwrap(),
        Some(&token),
        Value::Null,
        &[],
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    assert_eq!(stream.next().await.unwrap().unwrap().len(), 64 * 1024);
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/fntv/scope",
            json!({"mode":"none","paths":[]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(stream.next().await.unwrap().is_err());
    assert_eq!(
        wire(
            &state,
            "GET",
            show["poster"].as_str().unwrap(),
            Some(&token),
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn transient_thumbnail_failure_keeps_backoff_until_automatic_retry_succeeds() {
    let (state, media, _dir) = setup(&[("retry.mp4", b"synthetic".to_vec())], "movie").await;
    let node = state.vfs.resolve("/封面验证/retry.mp4").await.unwrap();
    media
        .files
        .lock()
        .unwrap()
        .insert("synthetic-retry-thumb".into(), noise_png());
    sqlx::query(
        "UPDATE chunks SET tg_thumbnail_file_id='synthetic-retry-thumb' WHERE node_id=? AND idx=0",
    )
    .bind(node.id)
    .execute(&state.vfs.db.pool)
    .await
    .unwrap();
    media.rate.store(1024, Ordering::SeqCst);
    let token = start(&state).await;
    wait(&state, 0, 1).await;
    let mut record = saved_states(&state).await.remove(0);
    assert_eq!(record["attempts"], 1);
    assert!((290..=300).contains(&(record["next_retry"].as_i64().unwrap() - crate::db::now_ts())));
    let before = media.ranges.lock().unwrap().len();
    list(&state, &token, LIBRARY).await;
    list(&state, &token, LIBRARY).await;
    assert_eq!(
        media.ranges.lock().unwrap().len(),
        before,
        "浏览不能跳过失败退避"
    );
    // 仅推进合成任务的到期状态，不改变宿主时钟或等待五分钟。
    record["next_retry"] = json!(crate::db::now_ts() - 1);
    media.rate.store(0, Ordering::SeqCst);
    let key = state.vfs.db.cover_states("fntv").await.unwrap()[0]
        .0
        .clone();
    state
        .vfs
        .db
        .cover_state_put(
            "fntv",
            &key,
            record["fingerprint"].as_str().unwrap(),
            &record.to_string(),
        )
        .await
        .unwrap();
    state.vfs.db.fntv.cover_wake.notify_one();
    wait(&state, 1, 0).await;
    assert_eq!(saved_states(&state).await[0]["attempts"], 1);
    assert_eq!(saved_states(&state).await[0]["next_retry"], 0);
    state.vfs.db.fntv.invalidate();
}

#[tokio::test]
async fn host_restart_resumes_pending_covers_without_restarting_service_manually() {
    let (state, media, dir) = setup(
        &[
            ("cached.mp4", b"synthetic".to_vec()),
            ("cached.jpg", png([20, 180, 220])),
        ],
        "movie",
    )
    .await;
    start(&state).await;
    wait(&state, 1, 0).await;
    media.rate.store(1, Ordering::SeqCst);
    seed_file(
        &state,
        &media,
        "/封面验证/pending.mp4",
        &video("color=c=red:size=160x90:rate=25", "1"),
        256 * 1024,
    )
    .await;
    manager(&state, "fntv.index.refresh", json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while media.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.vfs.db.fntv.invalidate();
    state.restore.shutdown.cancel();
    media.rate.store(0, Ordering::SeqCst);
    let reopened = state_at(&dir.path().join("test.db"), media.clone()).await;
    let _router = crate::apps::fntv::router(reopened.clone());
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let records = saved_states(&reopened).await;
            if records.len() == 2 && records.iter().all(|r| r["state"] == "ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("宿主恢复时应自动继续已有服务的待补封面");
    reopened.vfs.db.fntv.invalidate();
}
