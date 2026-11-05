//! 飞牛服务真实边界测试：临时 SQLite、内存媒体，不读取生产配置或连接 Telegram。
use super::*;
use axum::http::header;
use futures_util::StreamExt as _;

fn fntv_manifest() -> Manifest {
    let mut m = manifest();
    m.id = "fntv".into();
    m.name = "隔离飞牛服务".into();
    m.min_host_version = "0.3.5".into();
    m.permissions = vec![
        "files.read".into(),
        "media.read".into(),
        "fntv.service".into(),
    ];
    m.settings.clear();
    m
}

async fn manager(state: &AppState, method: &str, params: Value) -> (StatusCode, Value) {
    let launch = launch_app(state, "fntv").await;
    rpc_call(
        state,
        "fntv",
        launch["session"].as_str().unwrap(),
        method,
        params,
    )
    .await
}

async fn wire(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    value: Value,
    extra: &[(&str, &str)],
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, token);
    }
    for (key, value) in extra {
        request = request.header(*key, *value);
    }
    router(state)
        .oneshot(
            request
                .body(if value.is_null() {
                    Body::empty()
                } else {
                    Body::from(serde_json::to_vec(&value).unwrap())
                })
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn json_response(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
async fn login(state: &AppState, password: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = crate::engine::upload::to_hex(&Sha256::digest(password.as_bytes()));
    let (status, body) = json_response(
        wire(
            state,
            "POST",
            "/v/api/v2/user/loginByPassword",
            None,
            json!({"username":"local-test","password":digest}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]["token"].as_str().unwrap().into()
}

async fn configured(
    bytes: &[u8],
) -> (AppState, Arc<MemoryMedia>, tempfile::TempDir, String, Value) {
    let (state, media, directory) = fixture().await;
    state.vfs.mkdir("/影视").await.unwrap();
    state.vfs.mkdir("/其它").await.unwrap();
    let node = seed_file(&state, &media, "/影视/隔离.mp4", bytes, 256 * 1024).await;
    add_app(&state, &fntv_manifest(), None).await;
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/apps/fntv/scope",
        json!({"mode":"selected","paths":["/影视","/其它"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
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
    let root = state.vfs.resolve("/影视").await.unwrap().id;
    assert_eq!(manager(&state,"fntv.libraries.set",json!({"libraries":[{"id":"11111111111111111111111111111111","name":"隔离电影","kind":"movie","root_id":root}]})).await.0,StatusCode::OK);
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        manager(&state, "fntv.service.start", json!({})).await.0,
        StatusCode::OK
    );
    let token = login(&state, "Synthetic-Only-Account").await;
    let (status, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({"ancestor_guid":"11111111111111111111111111111111","page":1,"page_size":50}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let item = list["data"]["list"][0].clone();
    assert!(item["guid"].is_string());
    // 单元测试的媒体描述夹具，不作为官方播放或真实媒体探测证据。
    let version = state.vfs.db.node(node).await.unwrap().content_version();
    let info = json!({"version":version,"wrapper":"mp4","duration":300.0,"reserved_bytes":1024,"upstream_reserved_bytes":1024,"policy_version":1,
        "streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"color_transfer":"bt709"},{"index":1,"codec_type":"audio","codec_name":"aac","channels":2,"sample_rate":"48000"}]});
    let app = installed(&state, "fntv").await.unwrap();
    state
        .vfs
        .db
        .app_data_write(
            "fntv",
            &app.revision,
            &format!("fntv:probe:{}", item["media_guid"].as_str().unwrap()),
            Some(&info.to_string()),
            None,
        )
        .await
        .unwrap();
    (state, media, directory, token, item)
}

async fn start_play(state: &AppState, token: &str, item: &Value) {
    assert_eq!(
        json_response(
            wire(
                state,
                "POST",
                "/v/api/v1/stream",
                Some(token),
                json!({"item_guid":item["guid"],"media_guid":item["media_guid"]}),
                &[]
            )
            .await
        )
        .await
        .0,
        StatusCode::OK
    );
    let response = wire(
        state,
        "GET",
        &format!(
            "/v/api/v1/media/range/{}",
            item["media_guid"].as_str().unwrap()
        ),
        Some(token),
        Value::Null,
        &[("range", "bytes=0-5")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    to_bytes(response.into_body(), 1024).await.unwrap();
}
async fn record(state: &AppState, token: &str, item: &Value, link: &str, ts: f64) {
    let (status,body)=json_response(wire(state,"POST","/v/api/v1/play/record",Some(token),json!({"item_guid":item["guid"],"media_guid":item["media_guid"],"play_link":link,"ts":ts,"subtitle_guid":"_no_display_"}),&[]).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
async fn position(state: &AppState, token: &str, item: &Value) -> f64 {
    let (status, body) = json_response(
        wire(
            state,
            "POST",
            "/v/api/v1/play/info",
            Some(token),
            json!({"item_guid":item["guid"],"media_guid":item["media_guid"]}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]["ts"].as_f64().unwrap()
}

#[tokio::test]
async fn fntv_installation_permission_and_secret_boundary() {
    let (state, _, _directory) = fixture().await;
    let (status, body) =
        json_response(wire(&state, "GET", "/trimcon", None, Value::Null, &[]).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["data"].is_null());
    let mut invalid = fntv_manifest();
    invalid.id = "other".into();
    assert!(invalid.validate().is_err());
    invalid = fntv_manifest();
    invalid.min_host_version = "0.3.4".into();
    assert!(invalid.validate().is_err());
    invalid = fntv_manifest();
    invalid.permissions.retain(|p| p != "media.read");
    assert!(invalid.validate().is_err());
    add_app(&state, &fntv_manifest(), None).await;
    assert_eq!(
        manager(&state, "fntv.service.start", json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        manager(&state, "storage.get", json!({"key":"fntv:config"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        manager(&state, "storage.list", json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            "/v/api/v1/sys/version",
            None,
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let (state, _, _directory, token, _) = configured(b"0123456789").await;
    let config = manager(&state, "fntv.config.get", json!({})).await.1;
    assert!(config.get("verifier").is_none());
    assert!(config.get("password").is_none());
    assert_eq!(
        wire(
            &state,
            "GET",
            "/api/admin/config",
            Some(&token),
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            "/v/api/v1/user/info",
            None,
            Value::Null,
            &[(
                "authx",
                "nonce=123456&timestamp=0&sign=00000000000000000000000000000000"
            )]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn fntv_original_ranges_head_and_library_limits() {
    let (state, media, _directory, token, item) = configured(b"0123456789").await;
    seed_file(&state, &media, "/其它/不在库内.mp4", b"secret", 6).await;
    manager(&state, "fntv.index.refresh", json!({})).await;
    let path = format!(
        "/v/api/v1/media/range/{}",
        item["media_guid"].as_str().unwrap()
    );
    let response = wire(
        &state,
        "GET",
        &path,
        Some(&token),
        Value::Null,
        &[("range", "bytes=2-6")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-6/10");
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"23456"
    );
    let response = wire(
        &state,
        "HEAD",
        &path,
        Some(&token),
        Value::Null,
        &[("range", "bytes=-3")],
    )
    .await;
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            &path,
            Some(&token),
            Value::Null,
            &[("range", "bytes=10-")]
        )
        .await
        .status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            &path,
            Some(&token),
            Value::Null,
            &[("if-none-match", &etag)]
        )
        .await
        .status(),
        StatusCode::NOT_MODIFIED
    );
    let response = wire(
        &state,
        "GET",
        &path,
        Some(&token),
        Value::Null,
        &[("range", "bytes=1-2"), ("if-range", "\"different\"")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"0123456789"
    );
    let (_, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({"page":1,"page_size":50}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(list["data"]["total"], 1);
}

#[tokio::test]
async fn fntv_new_play_wins_and_active_play_can_rewind() {
    let (state, _, _directory, a, item) = configured(b"0123456789").await;
    let b = login(&state, "Synthetic-Only-Account").await;
    let link_a = "00000000-0000-0000-0000-000000000001";
    let link_b = "00000000-0000-0000-0000-000000000002";
    start_play(&state, &a, &item).await;
    record(&state, &a, &item, link_a, 207.0).await;
    assert_eq!(position(&state, &b, &item).await, 207.0);
    record(&state, &a, &item, link_a, 180.0).await;
    assert_eq!(
        position(&state, &b, &item).await,
        180.0,
        "只查询信息不得夺取进度权威"
    );
    start_play(&state, &b, &item).await;
    record(&state, &b, &item, link_b, 40.0).await;
    record(&state, &a, &item, link_a, 250.0).await;
    assert_eq!(position(&state, &b, &item).await, 40.0);
    record(&state, &b, &item, link_b, 10.0).await;
    assert_eq!(position(&state, &a, &item).await, 10.0);
    assert_eq!(
        wire(
            &state,
            "POST",
            "/v/api/v1/play/quit",
            Some(&b),
            json!({"play_link":link_b}),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    record(&state, &b, &item, link_b, 80.0).await;
    assert_eq!(position(&state, &a, &item).await, 10.0);
    start_play(&state, &b, &item).await;
    record(
        &state,
        &b,
        &item,
        "00000000-0000-0000-0000-000000000003",
        20.0,
    )
    .await;
    assert_eq!(position(&state, &b, &item).await, 20.0);
}

#[tokio::test]
async fn fntv_file_dates_are_milliseconds_and_play_positions_are_seconds() {
    let (state, _, _directory, token, item) = configured(b"0123456789").await;
    let node = state.vfs.resolve("/影视/隔离.mp4").await.unwrap();
    let (_, streams) = json_response(
        wire(
            &state,
            "GET",
            &format!(
                "/v/api/v1/stream/list/{}",
                item["media_guid"].as_str().unwrap()
            ),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(
        streams["data"]["files"][0]["timestamp"],
        node.modified_at * 1000
    );
    assert_eq!(
        streams["data"]["files"][0]["create_time"],
        node.modified_at * 1000
    );
    assert_eq!(streams["data"]["video_streams"][0]["duration"], 300);
    assert_eq!(
        streams["data"]["video_streams"][0]["create_time"],
        node.modified_at * 1000
    );
    start_play(&state, &token, &item).await;
    let link = "00000000-0000-0000-0000-000000000009";
    record(&state, &token, &item, link, 207.0).await;
    assert_eq!(position(&state, &token, &item).await, 207.0);
    let (_, detail) = json_response(
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
    assert_eq!(detail["data"]["duration"], 300);
    assert_eq!(detail["data"]["watched_ts"], 207.0);
    for invalid in [207_000.0, 302.001, -1.0] {
        let response = wire(&state, "POST", "/v/api/v1/play/record", Some(&token),
            json!({"item_guid":item["guid"],"media_guid":item["media_guid"],"play_link":link,"ts":invalid}), &[]).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            position(&state, &token, &item).await,
            207.0,
            "毫秒误报、超时长和负位置不能覆盖正确的秒位置"
        );
    }
}

#[tokio::test]
async fn fntv_library_type_change_rebuilds_organization_and_keeps_progress() {
    let (state, _, _directory, token, item) = configured(b"0123456789").await;
    state
        .vfs
        .rename("/影视/隔离.mp4", "/影视/合成 S01E01.mp4")
        .await
        .unwrap();
    manager(&state, "fntv.index.refresh", json!({})).await;
    start_play(&state, &token, &item).await;
    record(
        &state,
        &token,
        &item,
        "00000000-0000-0000-0000-000000000010",
        42.0,
    )
    .await;
    let root = state.vfs.resolve("/影视").await.unwrap().id;
    assert_eq!(
        manager(
            &state,
            "fntv.libraries.set",
            json!({"libraries":[{
                "id":"11111111111111111111111111111111","name":"改为剧集","kind":"tv","root_id":root
            }]})
        )
        .await
        .0,
        StatusCode::OK
    );
    manager(&state, "fntv.index.refresh", json!({})).await;
    let fresh = login(&state, "Synthetic-Only-Account").await;
    let (_, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&fresh),
            json!({"ancestor_guid":"11111111111111111111111111111111","page":1,"page_size":50}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(list["data"]["total"], 1);
    assert_eq!(list["data"]["list"][0]["type"], "TV");
    assert_eq!(
        position(&state, &fresh, &item).await,
        42.0,
        "整理方式变更不应改变原文件的观看记录"
    );
}

#[tokio::test]
async fn fntv_show_folders_are_not_seasons_and_folder_rename_refreshes_titles() {
    let (state, media, _directory, old, item) = configured(b"0123456789").await;
    for path in ["/影视/SeasonedAlpha", "/影视/SeasonedBeta"] {
        state.vfs.mkdir(path).await.unwrap();
    }
    state
        .vfs
        .rename("/影视/隔离.mp4", "/影视/SeasonedAlpha/S01E01.mp4")
        .await
        .unwrap();
    seed_file(
        &state,
        &media,
        "/影视/SeasonedBeta/S01E01.mp4",
        b"second synthetic",
        1024,
    )
    .await;
    manager(&state, "fntv.index.refresh", json!({})).await;
    start_play(&state, &old, &item).await;
    record(
        &state,
        &old,
        &item,
        "00000000-0000-0000-0000-000000000011",
        42.0,
    )
    .await;
    let root = state.vfs.resolve("/影视").await.unwrap().id;
    manager(
        &state,
        "fntv.libraries.set",
        json!({"libraries":[{
            "id":"11111111111111111111111111111111","name":"剧库","kind":"tv","root_id":root
        }]}),
    )
    .await;
    manager(&state, "fntv.index.refresh", json!({})).await;
    let token = login(&state, "Synthetic-Only-Account").await;
    let query = json!({"ancestor_guid":"11111111111111111111111111111111","page":1,"page_size":50});
    let (_, before) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            query.clone(),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(
        before["data"]["total"], 2,
        "以 Season 开头的剧名不是规范季目录"
    );
    let prior = before["data"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == "SeasonedAlpha")
        .unwrap();
    state
        .vfs
        .rename("/影视/SeasonedAlpha", "/影视/新剧名")
        .await
        .unwrap();
    manager(&state, "fntv.index.refresh", json!({})).await;
    let (_, after) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            query,
            &[],
        )
        .await,
    )
    .await;
    let renamed = after["data"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == "新剧名")
        .unwrap();
    assert_eq!(
        renamed["guid"], prior["guid"],
        "目录改名保留剧的身份，刷新目录来源标题"
    );
    assert_eq!(position(&state, &token, &item).await, 42.0);
}

#[tokio::test]
async fn fntv_rename_move_and_content_replacement_progress() {
    let (state, media, _directory, token, item) = configured(b"0123456789").await;
    let probe_key = format!("fntv:probe:{}", item["media_guid"].as_str().unwrap());
    let probe_fixture = state
        .vfs
        .db
        .app_data_get("fntv", &probe_key)
        .await
        .unwrap()
        .unwrap();
    start_play(&state, &token, &item).await;
    record(
        &state,
        &token,
        &item,
        "00000000-0000-0000-0000-000000000004",
        42.0,
    )
    .await;
    state.vfs.mkdir("/影视/子目录").await.unwrap();
    state
        .vfs
        .rename("/影视/隔离.mp4", "/影视/子目录/更名.mp4")
        .await
        .unwrap();
    manager(&state, "fntv.index.refresh", json!({})).await;
    assert_eq!(position(&state, &token, &item).await, 42.0);
    assert!(
        state
            .vfs
            .db
            .app_data_get("fntv", &probe_key)
            .await
            .unwrap()
            .is_some(),
        "同内容改名应保留真实媒体探测缓存"
    );
    state
        .vfs
        .rename("/影视/子目录/更名.mp4", "/其它/离库.mp4")
        .await
        .unwrap();
    manager(&state, "fntv.index.refresh", json!({})).await;
    assert!(
        state
            .vfs
            .db
            .app_data_get("fntv", &probe_key)
            .await
            .unwrap()
            .is_none(),
        "离库时释放可重建的探测缓存，保留观看记录"
    );
    assert_eq!(
        wire(
            &state,
            "POST",
            "/v/api/v1/play/info",
            Some(&token),
            json!({"item_guid":item["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        state
            .vfs
            .db
            .app_data_get(
                "fntv",
                &format!("fntv:progress:{}", item["media_guid"].as_str().unwrap())
            )
            .await
            .unwrap()
            .is_some()
    );
    state
        .vfs
        .rename("/其它/离库.mp4", "/影视/回库.mp4")
        .await
        .unwrap();
    manager(&state, "fntv.index.refresh", json!({})).await;
    // 身份测试只使用十字节占位媒体；回库后补回媒体描述夹具，不冒充真实探测。
    let app = installed(&state, "fntv").await.unwrap();
    state
        .vfs
        .db
        .app_data_write(
            "fntv",
            &app.revision,
            &probe_key,
            Some(&probe_fixture.value),
            None,
        )
        .await
        .unwrap();
    assert_eq!(position(&state, &token, &item).await, 42.0);
    seed_file(&state, &media, "/影视/回库.mp4", b"new content", 256 * 1024).await;
    manager(&state, "fntv.index.refresh", json!({})).await;
    assert!(
        state
            .vfs
            .db
            .app_data_get("fntv", &probe_key)
            .await
            .unwrap()
            .is_none(),
        "覆盖文件不保留旧内容的探测缓存"
    );
    assert_eq!(
        wire(
            &state,
            "POST",
            "/v/api/v1/play/info",
            Some(&token),
            json!({"item_guid":item["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let (_, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({"page":1,"page_size":50}),
            &[],
        )
        .await,
    )
    .await;
    assert_ne!(list["data"]["list"][0]["guid"], item["guid"]);
    assert_eq!(list["data"]["list"][0]["watched_ts"], 0.0);
}

#[tokio::test]
async fn fntv_lifecycle_interrupts_running_transfers_and_retains_data() {
    for operation in [
        "stop",
        "password",
        "scope",
        "disable",
        "update",
        "uninstall",
    ] {
        let (state, media, _directory, token, item) = configured(&vec![7; 512 * 1024]).await;
        let cinema = add_app(
            &state,
            &{
                let mut m = manifest();
                m.id = "cinema".into();
                m
            },
            None,
        )
        .await;
        state
            .vfs
            .db
            .app_data_write(
                "cinema",
                &cinema.revision,
                "progress:test",
                Some("{\"ts\":99}"),
                None,
            )
            .await
            .unwrap();
        media.slow.store(true, Ordering::SeqCst);
        let path = format!(
            "/v/api/v1/media/range/{}",
            item["media_guid"].as_str().unwrap()
        );
        let response = wire(&state, "GET", &path, Some(&token), Value::Null, &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        assert!(stream.next().await.unwrap().is_ok());
        match operation {
            "stop" => {
                assert_eq!(
                    manager(&state, "fntv.service.stop", json!({})).await.0,
                    StatusCode::OK
                );
            }
            "password" => {
                assert_eq!(
                    manager(
                        &state,
                        "fntv.account.set",
                        json!({"username":"local-test","password":"New-Synthetic-Only-Account"})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            "scope" => {
                assert_eq!(
                    call(
                        &state,
                        "PATCH",
                        "/api/apps/fntv/scope",
                        json!({"mode":"selected","paths":["/其它"]})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            "disable" => {
                assert_eq!(
                    call(
                        &state,
                        "POST",
                        "/api/apps/fntv/state",
                        json!({"enabled":false})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            "update" => {
                let old = installed(&state, "fntv").await.unwrap();
                let mut m = fntv_manifest();
                m.version = "1.0.1".into();
                let p = AppPackage::parse(archive(&m)).unwrap();
                let digest = p.digest.clone();
                install_package(
                    &state,
                    p,
                    InstallOptions {
                        digest,
                        expected_revision: Some(old.revision),
                        replace_legacy: false,
                        replacement_revision: None,
                    },
                    "import",
                )
                .await
                .unwrap();
            }
            "uninstall" => {
                assert_eq!(
                    call(&state, "DELETE", "/api/apps/fntv", json!({})).await.0,
                    StatusCode::OK
                );
            }
            _ => unreachable!(),
        }
        let next = tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
            .await
            .unwrap();
        assert!(
            next.is_none() || next.unwrap().is_err(),
            "{operation} 后仍输出视频字节"
        );
        drop(stream);
        let rejected = wire(&state, "GET", &path, Some(&token), Value::Null, &[])
            .await
            .status();
        assert!(!rejected.is_success(), "{operation} 未撤销旧登录");
        assert!(state.vfs.resolve("/影视/隔离.mp4").await.is_ok());
        assert_eq!(
            state
                .vfs
                .db
                .app_data_get("cinema", "progress:test")
                .await
                .unwrap()
                .unwrap()
                .value,
            "{\"ts\":99}"
        );
        if operation == "uninstall" {
            assert!(
                state
                    .vfs
                    .db
                    .app_data_get("fntv", "fntv:config")
                    .await
                    .unwrap()
                    .is_some()
            );
            add_app(&state, &fntv_manifest(), None).await;
            assert_eq!(
                manager(&state, "fntv.service.status", json!({})).await.1["running"],
                false
            );
            call(
                &state,
                "DELETE",
                "/api/apps/fntv?purge_data=true",
                json!({}),
            )
            .await;
            assert!(
                state
                    .vfs
                    .db
                    .app_data_get("fntv", "fntv:config")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(state.vfs.resolve("/影视/隔离.mp4").await.is_ok());
        }
    }
}

#[tokio::test]
async fn fntv_failed_update_keeps_running_and_recovery_revokes_old_login() {
    let (state, _, _directory, token, item) = configured(b"0123456789").await;
    let old = installed(&state, "fntv").await.unwrap();
    let package = AppPackage::parse(archive(&fntv_manifest())).unwrap();
    let digest = package.digest.clone();
    assert!(
        install_package(
            &state,
            package,
            InstallOptions {
                digest,
                expected_revision: Some(old.revision.clone()),
                replace_legacy: false,
                replacement_revision: None
            },
            "import"
        )
        .await
        .is_err()
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            "/v/api/v1/user/info",
            Some(&token),
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    start_play(&state, &token, &item).await;
    record(
        &state,
        &token,
        &item,
        "00000000-0000-0000-0000-000000000005",
        55.0,
    )
    .await;
    let mut manifest = fntv_manifest();
    manifest.version = "1.0.1".into();
    add_app(&state, &manifest, Some(old.revision)).await;
    assert_eq!(
        manager(&state, "fntv.service.status", json!({})).await.1["running"],
        true
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/apps/fntv/recovery",
            json!({"expected_revision":installed(&state,"fntv").await.unwrap().revision})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        !wire(
            &state,
            "GET",
            "/v/api/v1/user/info",
            Some(&token),
            Value::Null,
            &[]
        )
        .await
        .status()
        .is_success()
    );
    let fresh = login(&state, "Synthetic-Only-Account").await;
    assert_eq!(position(&state, &fresh, &item).await, 55.0);
}

#[tokio::test]
async fn fntv_favorites_watched_and_tv_are_independent() {
    let (state, media, _directory, token, item) = configured(b"0123456789").await;
    assert_eq!(
        wire(
            &state,
            "PUT",
            "/v/api/v1/item/favorite",
            Some(&token),
            json!({"item_guid":item["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/watched",
            Some(&token),
            json!({"item_guid":item["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    let (_, favorite) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/favorite/list",
            Some(&token),
            json!({"page":1,"page_size":22}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(favorite["data"]["total"], 1);
    assert_eq!(favorite["data"]["list"][0]["is_watched"], 1);
    assert!(
        !state.vfs.resolve("/影视/隔离.mp4").await.unwrap().favorite,
        "影视收藏不得写到网盘收藏"
    );
    assert_eq!(
        wire(
            &state,
            "DELETE",
            "/v/api/v1/item/favorite",
            Some(&token),
            json!({"item_guid":item["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    for path in [
        "/其它/剧集",
        "/其它/剧集/合成剧",
        "/其它/剧集/合成剧/Season01",
    ] {
        state.vfs.mkdir(path).await.unwrap();
    }
    for episode in [1, 2] {
        seed_file(
            &state,
            &media,
            &format!("/其它/剧集/合成剧/Season01/合成剧 S01E{episode:02}.mkv"),
            b"synthetic episode",
            1024,
        )
        .await;
    }
    seed_file(
        &state,
        &media,
        "/其它/剧集/合成剧/Season01/无法识别.mkv",
        b"synthetic unknown",
        1024,
    )
    .await;
    seed_file(&state,&media,"/其它/剧集/合成剧/Season01/合成剧 S01E01.nfo",b"<!DOCTYPE movie [<!ENTITY remote SYSTEM 'http://example.invalid/'>]><episodedetails><title>&remote;</title></episodedetails>",1024).await;
    let root = state.vfs.resolve("/其它/剧集").await.unwrap().id;
    let result=manager(&state,"fntv.libraries.set",json!({"libraries":[{"id":"22222222222222222222222222222222","name":"隔离剧集","kind":"tv","root_id":root}]})).await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    manager(&state, "fntv.index.refresh", json!({})).await;
    let token = login(&state, "Synthetic-Only-Account").await;
    let (_, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({"ancestor_guid":"22222222222222222222222222222222","tags":{"type":["TV"]}}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(list["data"]["total"], 2, "未识别视频仍应出现在电视剧库中");
    let show = list["data"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "TV")
        .unwrap();
    assert_eq!(
        wire(
            &state,
            "PUT",
            "/v/api/v1/item/favorite",
            Some(&token),
            json!({"item_guid":show["guid"]}),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    let (_, favorites) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/favorite/list",
            Some(&token),
            json!({}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(favorites["data"]["total"], 1);
    assert_eq!(favorites["data"]["list"][0]["type"], "TV");
    let (_, seasons) = json_response(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/season/list/{}", show["guid"].as_str().unwrap()),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(seasons["data"].as_array().unwrap().len(), 1);
    let (_, episodes) = json_response(
        wire(
            &state,
            "GET",
            &format!(
                "/v/api/v1/episode/list/{}",
                seasons["data"][0]["guid"].as_str().unwrap()
            ),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(episodes["data"].as_array().unwrap().len(), 2);
    assert_eq!(episodes["data"][0]["episode_number"], 1);
    assert_ne!(episodes["data"][0]["title"], "&remote;");
}

async fn clear_probe_cache(state: &AppState, item: &Value) {
    let key = format!("fntv:probe:{}", item["media_guid"].as_str().unwrap());
    let record = state
        .vfs
        .db
        .app_data_get("fntv", &key)
        .await
        .unwrap()
        .unwrap();
    let app = installed(state, "fntv").await.unwrap();
    state
        .vfs
        .db
        .app_data_write("fntv", &app.revision, &key, None, Some(&record.revision))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需显式指定并核验此前生成的合成 MP4/MKV 素材"]
async fn fntv_real_probe_reads_real_metadata_with_bounded_bytes_and_version_cache() {
    let assets = super::fntv_native::assets();
    for (name, wrapper) in [("sample.mp4", "mp4"), ("sample.mkv", "mkv")] {
        let bytes = std::fs::read(assets.join(name)).unwrap();
        let (state, media, _directory, token, item) = configured(&bytes).await;
        if wrapper == "mkv" {
            state
                .vfs
                .rename("/影视/隔离.mp4", "/影视/隔离.mkv")
                .await
                .unwrap();
            manager(&state, "fntv.index.refresh", json!({})).await;
        }
        clear_probe_cache(&state, &item).await;
        media.ranges.lock().unwrap().clear();
        let (status, body) = json_response(
            wire(
                &state,
                "GET",
                &format!("/v/api/v1/stream/list/{}", item["guid"].as_str().unwrap()),
                Some(&token),
                Value::Null,
                &[],
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["video_streams"][0]["codec_name"], "h264");
        assert_eq!(body["data"]["video_streams"][0]["wrapper"], wrapper);
        assert_eq!(body["data"]["video_streams"][0]["width"], 1920);
        assert_eq!(body["data"]["audio_streams"].as_array().unwrap().len(), 2);
        let duration = body["data"]["video_streams"][0]["duration"]
            .as_f64()
            .unwrap();
        assert!((299.0..301.0).contains(&duration));
        let ranges = media.ranges.lock().unwrap().clone();
        let requested: u64 = ranges.iter().map(|(_, length)| length).sum();
        assert!(
            requested > 0 && requested <= 8 * 1024 * 1024,
            "实际底层探测请求 {requested} 字节超过预算"
        );
        let _ = wire(
            &state,
            "GET",
            &format!("/v/api/v1/stream/list/{}", item["guid"].as_str().unwrap()),
            Some(&token),
            Value::Null,
            &[],
        )
        .await;
        assert_eq!(
            media.ranges.lock().unwrap().len(),
            ranges.len(),
            "同内容版本应复用解析结果"
        );
        println!(
            "FNTV_REAL_PROBE wrapper={wrapper} actual_range_bytes={requested} duration={duration} cache_hit=true"
        );
    }
}

#[tokio::test]
#[ignore = "需显式指定合成素材，验证真实探测进程的并发和取消"]
async fn fntv_real_probe_concurrency_stop_and_timeout_budget() {
    let assets = super::fntv_native::assets();
    let bytes = std::fs::read(assets.join("sample.mp4")).unwrap();
    let (state, media, _directory, token, item) = configured(&bytes).await;
    clear_probe_cache(&state, &item).await;
    media.slow.store(true, Ordering::SeqCst);
    media.ranges.lock().unwrap().clear();
    let path = format!("/v/api/v1/stream/list/{}", item["guid"].as_str().unwrap());
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let state = state.clone();
        let token = token.clone();
        let path = path.clone();
        jobs.push(tokio::spawn(async move {
            wire(&state, "GET", &path, Some(&token), Value::Null, &[]).await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.vfs.db.fntv.probe_slots.available_permits() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let rejected = wire(&state, "GET", &path, Some(&token), Value::Null, &[]).await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "超限探测不应排队"
    );
    assert_eq!(
        manager(&state, "fntv.service.stop", json!({})).await.0,
        StatusCode::OK
    );
    for job in jobs {
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(2), job)
                .await
                .unwrap()
                .unwrap()
                .status()
                .is_success()
        );
    }
    assert_eq!(state.vfs.db.fntv.probe_slots.available_permits(), 2);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while media.active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        state
            .vfs
            .db
            .app_data_get(
                "fntv",
                &format!("fntv:probe:{}", item["media_guid"].as_str().unwrap())
            )
            .await
            .unwrap()
            .is_none()
    );
    manager(&state, "fntv.service.start", json!({})).await;
    let token = login(&state, "Synthetic-Only-Account").await;
    media.ranges.lock().unwrap().clear();
    let started = std::time::Instant::now();
    let response = wire(&state, "GET", &path, Some(&token), Value::Null, &[]).await;
    assert!(!response.status().is_success());
    assert!(started.elapsed() <= std::time::Duration::from_secs(12));
    let requested: u64 = media
        .ranges
        .lock()
        .unwrap()
        .iter()
        .map(|(_, length)| length)
        .sum();
    assert!(requested <= 8 * 1024 * 1024);
    assert_eq!(state.vfs.db.fntv.probe_slots.available_permits(), 2);
    println!(
        "FNTV_REAL_PROBE_LIMIT concurrency=2 queue=0 stop_cancelled=true timeout_ms={} actual_range_bytes={requested}",
        started.elapsed().as_millis()
    );
}
