//! 封面缓存能力只在临时数据库中验证：格式校验、按应用隔离、管理接口与卸载清理。
use super::*;

fn cover(len: usize) -> String {
    let mut bytes = vec![0u8; len.max(3)];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

async fn cover_app(state: &AppState, id: &str, api_version: u32) -> String {
    let mut m = manifest();
    m.id = id.into();
    m.api_version = api_version;
    add_app(state, &m, None).await;
    launch_app(state, id).await["session"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn covers_round_trip_validate_and_stay_per_app() {
    let (state, _media, _dir) = fixture().await;
    let session = cover_app(&state, "books", 2).await;
    let rpc =
        |method: &'static str, params: Value| rpc_call(&state, "books", &session, method, params);
    let data = cover(2048);
    let (status, put) = rpc(
        "covers.put",
        json!({"key":"unit:1","data":data,"meta":{"h":"abc","origin":"embedded"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(put["bytes"], 2048);
    assert_eq!(put["evicted"], 0);
    let (status, got) = rpc("covers.get", json!({"keys":["unit:1","unit:2","unit:1"]})).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    let covers = got["covers"].as_array().unwrap();
    assert_eq!(covers.len(), 1);
    assert_eq!(covers[0]["key"], "unit:1");
    assert_eq!(covers[0]["data"], data);
    assert_eq!(covers[0]["meta"]["h"], "abc");

    let png_header = format!("data:image/png;base64,{}", data.split_once(',').unwrap().1);
    let too_large = cover(256 * 1024 + 1);
    let many: Vec<String> = (0..17).map(|i| format!("unit:{i}")).collect();
    for (method, params) in [
        ("covers.put", json!({"key":"unit:1","data":png_header})),
        (
            "covers.put",
            json!({"key":"unit:1","data":"data:image/jpeg;base64,!!"}),
        ),
        (
            "covers.put",
            json!({"key":"unit:1","data":"https://example.com/a.jpg"}),
        ),
        (
            "covers.put",
            json!({"key":"unit:1","data":"data:image/svg+xml;base64,PHN2Zz4="}),
        ),
        ("covers.put", json!({"key":"unit:1","data":too_large})),
        (
            "covers.put",
            json!({"key":"unit:1","data":cover(64),"meta":{"x":"界".repeat(2000)}}),
        ),
        ("covers.put", json!({"key":"bad key","data":cover(64)})),
        ("covers.get", json!({"keys":[]})),
        ("covers.get", json!({"keys":many})),
        ("covers.get", json!({"keys":["unit:1"],"app_id":"comics"})),
    ] {
        let (status, body) = rpc(method, params.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{method} {params} → {body}"
        );
    }
    // 失败的写入不改变已有封面。
    assert_eq!(
        rpc("covers.get", json!({"keys":["unit:1"]})).await.1["covers"][0]["data"],
        data
    );

    let other = cover_app(&state, "comics", 2).await;
    let (_, empty) = rpc_call(
        &state,
        "comics",
        &other,
        "covers.get",
        json!({"keys":["unit:1"]}),
    )
    .await;
    assert_eq!(empty["covers"], json!([]));

    let (_, stats) = rpc("covers.stats", json!({})).await;
    assert_eq!(stats["entries"], 1);
    assert_eq!(stats["bytes"], 2048);
    assert_eq!(stats["limit_bytes"], 256 * 1024 * 1024);
    let (status, deleted) = rpc("covers.delete", json!({"keys":["unit:1","unit:9"]})).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], 1);
    assert_eq!(
        rpc("covers.get", json!({"keys":["unit:1"]})).await.1["covers"],
        json!([])
    );

    let legacy = cover_app(&state, "legacy", 1).await;
    let (status, _) = rpc_call(&state, "legacy", &legacy, "covers.stats", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 封面能力只经单独调用，不进入批量信封。
    let (status, _) = rpc(
        "rpc.batch",
        json!({"calls":[{"method":"covers.stats","params":{}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn covers_admin_clear_and_uninstall_remove_cache() {
    let (state, _media, _dir) = fixture().await;
    let session = cover_app(&state, "books", 2).await;
    for key in ["unit:1", "unit:2"] {
        let (status, body) = rpc_call(
            &state,
            "books",
            &session,
            "covers.put",
            json!({"key":key,"data":cover(100)}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, stats) = call(&state, "GET", "/api/apps/books/covers", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["entries"], 2);
    assert_eq!(stats["bytes"], 200);
    let (status, cleared) = call(&state, "DELETE", "/api/apps/books/covers", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["removed"], 2);
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 0);
    assert_eq!(
        call(&state, "GET", "/api/apps/missing/covers", json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // 停用后旧会话不能继续写入封面。
    rpc_call(
        &state,
        "books",
        &session,
        "covers.put",
        json!({"key":"unit:3","data":cover(100)}),
    )
    .await;
    call(
        &state,
        "POST",
        "/api/apps/books/state",
        json!({"enabled": false}),
    )
    .await;
    let (status, _) = rpc_call(
        &state,
        "books",
        &session,
        "covers.put",
        json!({"key":"unit:4","data":cover(100)}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 1);

    // 卸载时即使保留应用数据，封面也一并删除。
    let (status, body) = call(
        &state,
        "DELETE",
        "/api/apps/books?purge_data=false",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state.vfs.db.cover_stats("books").await.unwrap().entries, 0);
}
