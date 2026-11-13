//! 添加日期排序的协议回归：日期、标题和修改顺序故意不同，全部使用隔离数据。
use super::*;

const LIBRARY: &str = "33333333333333333333333333333333";
const MOVIES: &[(&str, i64, i64)] = &[
    ("A 旧片.mp4", 1000, 6000),
    ("B 最新.mp4", 4000, 4500),
    ("C 中间.mp4", 3000, 5000),
    ("D 同日.mp4", 2000, 9000),
    ("E 同日.mp4", 2000, 9000),
    ("另一目录/A 旧片.mp4", 1000, 8000),
];

async fn setup(
    files: &[(&str, i64, i64)],
    kind: &str,
) -> (AppState, Arc<MemoryMedia>, tempfile::TempDir, String) {
    let (state, media, directory) = fixture().await;
    state.vfs.mkdir("/排序").await.unwrap();
    state.vfs.mkdir("/库外").await.unwrap();
    for (name, created_at, modified_at) in files {
        let parts: Vec<_> = name.split('/').collect();
        let mut parent = "/排序".to_owned();
        for part in &parts[..parts.len() - 1] {
            parent.push('/');
            parent.push_str(part);
            if state.vfs.resolve(&parent).await.is_err() {
                state.vfs.mkdir(&parent).await.unwrap();
            }
        }
        let node = seed_file(
            &state,
            &media,
            &format!("/排序/{name}"),
            b"synthetic sorting fixture",
            1024,
        )
        .await;
        // 首次索引前固定时间，避免修改无哈希节点的创建时间后改变内容身份。
        sqlx::query("UPDATE nodes SET created_at = ?, modified_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(modified_at)
            .bind(node)
            .execute(&state.vfs.db.pool)
            .await
            .unwrap();
    }
    add_app(&state, &fntv_manifest(), None).await;
    assert_eq!(
        call(
            &state,
            "PATCH",
            "/api/apps/fntv/scope",
            json!({"mode":"selected","paths":["/排序"]})
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
    let root = state.vfs.resolve("/排序").await.unwrap().id;
    assert_eq!(
        manager(
            &state,
            "fntv.libraries.set",
            json!({"libraries":[{"id":LIBRARY,"name":"添加日期验证","kind":kind,"root_id":root}]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        manager(&state, "fntv.service.start", json!({})).await.0,
        StatusCode::OK
    );
    let token = login(&state, "Synthetic-Only-Account").await;
    (state, media, directory, token)
}

fn query(ancestor: &str, direction: &str, page: u64, page_size: u64) -> Value {
    json!({"ancestor_guid":ancestor,"sort_column":"create_time","sort_type":direction,
        "page":page,"page_size":page_size})
}

async fn list(state: &AppState, token: &str, endpoint: &str, query: Value) -> Value {
    let (status, body) =
        json_response(wire(state, "POST", endpoint, Some(token), query, &[]).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn children(
    state: &AppState,
    token: &str,
    ancestor: &str,
    direction: &str,
    kind: &str,
) -> Value {
    let mut input = query(ancestor, direction, 1, 50);
    input["tags"] = json!({"type":[kind]});
    list(state, token, "/v/api/v1/item/list", input).await
}

fn titles(data: &Value) -> Vec<&str> {
    data["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["title"].as_str().unwrap())
        .collect()
}

fn ids(data: &Value) -> Vec<String> {
    data["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["guid"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn fntv_added_date_sort_precedes_pagination_and_has_stable_ties() {
    let (state, _, _directory, token) = setup(MOVIES, "movie").await;
    for (direction, expected) in [
        (
            "DESC",
            vec!["B 最新", "C 中间", "D 同日", "E 同日", "A 旧片", "A 旧片"],
        ),
        (
            "ASC",
            vec!["A 旧片", "A 旧片", "D 同日", "E 同日", "C 中间", "B 最新"],
        ),
    ] {
        let complete = list(
            &state,
            &token,
            "/v/api/v1/item/list",
            query(LIBRARY, direction, 1, 50),
        )
        .await;
        assert_eq!(complete["total"], 6);
        assert_eq!(titles(&complete), expected);
        let mut paged = Vec::new();
        for page in 1..=4 {
            let part = list(
                &state,
                &token,
                "/v/api/v1/item/list",
                query(LIBRARY, direction, page, 2),
            )
            .await;
            assert_eq!(part["total"], 6);
            assert_eq!(
                part["list"].as_array().unwrap().len(),
                if page == 4 { 0 } else { 2 }
            );
            paged.extend(ids(&part));
        }
        assert_eq!(paged, ids(&complete));
        assert_eq!(
            paged.iter().collect::<std::collections::HashSet<_>>().len(),
            6
        );
        let old: Vec<_> = complete["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["title"] == "A 旧片")
            .map(|row| row["guid"].as_str().unwrap())
            .collect();
        assert!(old[0] < old[1], "同日期同标题仍须稳定排序");
        for row in complete["list"].as_array().unwrap() {
            let expected = MOVIES
                .iter()
                .find(|(name, _, _)| name.rsplit('/').next() == row["file_name"].as_str())
                .unwrap()
                .1;
            assert_eq!(row["create_time"], expected * 1000);
        }
    }
    let default = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        json!({"ancestor_guid":LIBRARY}),
    )
    .await;
    assert_eq!(
        titles(&default),
        ["A 旧片", "A 旧片", "B 最新", "C 中间", "D 同日", "E 同日"]
    );
    let unknown = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        json!({
            "ancestor_guid":LIBRARY,"sort_column":"unknown","sort_type":"DESC"
        }),
    )
    .await;
    assert_eq!(ids(&unknown), ids(&default));
    let reverse_title = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        json!({
            "ancestor_guid":LIBRARY,"sort_column":"sort_title","sort_type":"DESC"
        }),
    )
    .await;
    assert_eq!(
        titles(&reverse_title),
        ["E 同日", "D 同日", "C 中间", "B 最新", "A 旧片", "A 旧片"]
    );
}

#[tokio::test]
async fn fntv_added_date_sort_applies_to_favorites_and_filtered_pages() {
    let (state, _, _directory, token) = setup(MOVIES, "movie").await;
    let all = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    for title in ["A 旧片", "B 最新", "E 同日"] {
        let row = all["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["title"] == title)
            .unwrap();
        let response = wire(
            &state,
            "PUT",
            "/v/api/v1/item/favorite",
            Some(&token),
            json!({"item_guid":row["guid"]}),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    for (direction, expected) in [
        ("DESC", vec!["B 最新", "E 同日", "A 旧片"]),
        ("ASC", vec!["A 旧片", "E 同日", "B 最新"]),
    ] {
        let favorites = list(
            &state,
            &token,
            "/v/api/v1/favorite/list",
            query("", direction, 1, 2),
        )
        .await;
        assert_eq!(favorites["total"], 3);
        assert_eq!(titles(&favorites), expected[..2]);
        let last = list(
            &state,
            &token,
            "/v/api/v1/favorite/list",
            query("", direction, 2, 2),
        )
        .await;
        assert_eq!(titles(&last), expected[2..]);
    }
    let mut filtered = query(LIBRARY, "DESC", 1, 1);
    filtered["q"] = json!("同日");
    filtered["tags"] = json!({"type":["Movie"]});
    let first = list(&state, &token, "/v/api/v1/item/list", filtered.clone()).await;
    assert_eq!(first["total"], 2);
    assert_eq!(titles(&first), ["D 同日"]);
    filtered["page"] = json!(2);
    let second = list(&state, &token, "/v/api/v1/item/list", filtered.clone()).await;
    assert_eq!(titles(&second), ["E 同日"]);
    filtered["tags"] = json!({"type":["TV"]});
    let empty = list(&state, &token, "/v/api/v1/item/list", filtered).await;
    assert_eq!(empty["total"], 0);
    assert!(titles(&empty).is_empty());
}

#[tokio::test]
async fn fntv_added_date_survives_rename_move_and_refresh() {
    let (state, _, _directory, token) = setup(MOVIES, "movie").await;
    let before = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    let target = before["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == "C 中间")
        .unwrap();
    let target_guid = target["guid"].clone();
    state
        .vfs
        .rename("/排序/C 中间.mp4", "/排序/另一目录/Z 改名.mp4")
        .await
        .unwrap();
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    let after = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    assert_eq!(ids(&after), ids(&before));
    let renamed = &after["list"][1];
    assert_eq!(renamed["guid"], target_guid);
    assert_eq!(renamed["title"], "Z 改名");
    assert_eq!(renamed["create_time"], 3_000_000);
    assert!(renamed["update_time"].as_i64().unwrap() > 9_000_000);
}

#[tokio::test]
async fn fntv_added_date_repairs_legacy_index_before_and_after_refresh() {
    let (state, _, _directory, token) = setup(MOVIES, "movie").await;
    // 阻止后台扫描抢先修复旧格式，单独验证升级后的首次列表读取。
    let guard = state.vfs.db.fntv.index_lock.lock().await;
    sqlx::query("UPDATE app_data SET value = json_set(json_remove(value, '$.added_at'), '$.timestamp', 9999999999999) WHERE app_id = 'fntv' AND key LIKE 'fntv:index:%'")
        .execute(&state.vfs.db.pool).await.unwrap();
    let legacy = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    assert_eq!(
        titles(&legacy),
        ["B 最新", "C 中间", "D 同日", "E 同日", "A 旧片", "A 旧片"]
    );
    assert_eq!(legacy["list"][0]["create_time"], 4_000_000);
    assert_eq!(legacy["list"][0]["update_time"], 4_500_000);
    drop(guard);
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM app_data WHERE app_id = 'fntv' AND key LIKE 'fntv:index:%'",
    )
    .fetch_all(&state.vfs.db.pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 6);
    for raw in stored {
        let row: Value = serde_json::from_str(&raw).unwrap();
        let node = state
            .vfs
            .db
            .node(row["node_id"].as_i64().unwrap())
            .await
            .unwrap();
        assert_eq!(row["added_at"], node.created_at * 1000);
        assert_eq!(row["timestamp"], node.modified_at * 1000);
    }
}

#[tokio::test]
async fn fntv_added_date_for_shows_and_seasons_tracks_latest_visible_episode() {
    let (state, _, _directory, token) = setup(
        &[
            ("A 对照/Season01/A 对照 S01E01.mp4", 500, 2000),
            ("B 更新/Season01/B 更新 S01E01.mp4", 100, 6000),
            ("B 更新/Season01/B 更新 S01E02.mp4", 900, 1200),
            ("B 更新/Season02/B 更新 S02E01.mp4", 700, 1100),
        ],
        "tv",
    )
    .await;
    let shows = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    assert_eq!(titles(&shows), ["B 更新", "A 对照"]);
    assert_eq!(shows["list"][0]["create_time"], 900_000);
    let show_guid = shows["list"][0]["guid"].as_str().unwrap();
    let seasons = children(&state, &token, show_guid, "ASC", "Season").await;
    assert_eq!(titles(&seasons), ["第 2 季", "第 1 季"]);
    assert_eq!(seasons["list"][0]["create_time"], 700_000);
    assert_eq!(seasons["list"][1]["create_time"], 900_000);
    let season_guid = seasons["list"][1]["guid"].as_str().unwrap();
    let episodes = children(&state, &token, season_guid, "DESC", "Episode").await;
    assert_eq!(episodes["list"][0]["episode_number"], 2);
    assert_eq!(episodes["list"][1]["episode_number"], 1);
    for (guid, expected) in [(show_guid, 900_000), (season_guid, 900_000)] {
        let (status, detail) = json_response(
            wire(
                &state,
                "GET",
                &format!("/v/api/v1/item/{guid}"),
                Some(&token),
                Value::Null,
                &[],
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["data"]["create_time"], expected);
    }
    // 保留旧索引，证明移出媒体库的最新集不会继续影响日期或泄露其存在。
    let _guard = state.vfs.db.fntv.index_lock.lock().await;
    state
        .vfs
        .rename(
            "/排序/B 更新/Season01/B 更新 S01E02.mp4",
            "/库外/移出的第二集.mp4",
        )
        .await
        .unwrap();
    let current = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    assert_eq!(current["list"][0]["create_time"], 700_000);
    let current_seasons = children(&state, &token, show_guid, "DESC", "Season").await;
    assert_eq!(titles(&current_seasons), ["第 2 季", "第 1 季"]);
    assert_eq!(current_seasons["list"][1]["create_time"], 100_000);
    state
        .vfs
        .rename(
            "/排序/B 更新/Season02/B 更新 S02E01.mp4",
            "/库外/移出的第二季.mp4",
        )
        .await
        .unwrap();
    let current = list(
        &state,
        &token,
        "/v/api/v1/item/list",
        query(LIBRARY, "DESC", 1, 50),
    )
    .await;
    assert_eq!(titles(&current), ["A 对照", "B 更新"]);
    assert_eq!(current["list"][1]["create_time"], 100_000);
}
