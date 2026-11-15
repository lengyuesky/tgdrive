//! 人员协议使用真实临时网盘与宿主路由，覆盖跨库、剧集继承及撤销资料引用。
use super::*;

fn portrait(color: [u8; 3]) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(120, 180, image::Rgb(color));
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode_image(&image)
        .unwrap();
    bytes
}

async fn actors(state: &AppState, token: &str, reference: &str) -> Value {
    let (status, body) = json_response(
        wire(
            state,
            "POST",
            &format!("/v/api/v1/person/list/{reference}"),
            Some(token),
            json!({"page":1,"page_size":200}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn detail(state: &AppState, token: &str, id: &str) -> Value {
    let (status, body) = json_response(
        wire(
            state,
            "GET",
            &format!("/v/api/v1/person/{id}"),
            Some(token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

#[tokio::test]
async fn actor_profiles_keep_roles_sort_works_and_drop_moved_metadata() {
    let (state, media, _directory, _token, _) = configured(b"synthetic").await;
    let old_bio = "完整人物简介。".repeat(4000);
    seed_file(
        &state,
        &media,
        "/影视/旧头像.jpg",
        &portrait([200, 20, 20]),
        65536,
    )
    .await;
    let old_nfo = seed_file(&state, &media, "/影视/隔离.nfo", format!("<movie><actor><name> 演员甲 </name><role>角色甲</role><order>2</order><thumb>旧头像.jpg</thumb><biography>{old_bio}</biography></actor><actor><name>演员乙</name><order>0</order></actor><actor><name>演员甲</name><role>角色乙</role></actor><actor><name> </name></actor></movie>").as_bytes(), 65536).await;
    let sequel = seed_file(&state, &media, "/其它/续集.mp4", b"synthetic", 65536).await;
    seed_file(
        &state,
        &media,
        "/其它/新头像.jpg",
        &portrait([20, 200, 20]),
        65536,
    )
    .await;
    let new_nfo = seed_file(&state, &media, "/其它/续集.nfo", "<movie><actor><name>演员甲</name><thumb>新头像.jpg</thumb><biography>较新人物简介</biography></actor></movie>".as_bytes(), 65536).await;
    sqlx::query("UPDATE nodes SET modified_at=100 WHERE id=?")
        .bind(old_nfo)
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET modified_at=200 WHERE id=?")
        .bind(new_nfo)
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET created_at=10 WHERE id=?")
        .bind(state.vfs.resolve("/影视/隔离.mp4").await.unwrap().id)
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET created_at=30 WHERE id=?")
        .bind(sequel)
        .execute(&state.vfs.db.pool)
        .await
        .unwrap();
    let root = state.vfs.resolve("/影视").await.unwrap().id;
    let other = state.vfs.resolve("/其它").await.unwrap().id;
    assert_eq!(manager(&state, "fntv.libraries.set", json!({"libraries":[
        {"id":"11111111111111111111111111111111","name":"电影","kind":"movie","root_id":root},
        {"id":"22222222222222222222222222222222","name":"重叠电影","kind":"movie","root_id":root},
        {"id":"33333333333333333333333333333333","name":"其它电影","kind":"movie","root_id":other}]})).await.0, StatusCode::OK);
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    let token = login(&state, "Synthetic-Only-Account").await;
    let (_, current) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({"ancestor_guid":"11111111111111111111111111111111"}),
            &[],
        )
        .await,
    )
    .await;
    let item = &current["data"]["list"][0];
    let cast = actors(&state, &token, item["guid"].as_str().unwrap()).await;
    assert_eq!(cast["total"], 2);
    assert_eq!(cast["list"][0]["name"], "演员乙");
    assert_eq!(cast["list"][0]["profile_path"], "");
    let person = &cast["list"][1];
    assert_eq!(person["name"], "演员甲");
    assert_eq!(person["role"], "角色甲 / 角色乙");
    let id = person["person_guid"].as_str().unwrap();
    let profile = detail(&state, &token, id).await;
    assert_eq!(profile["biography"], "较新人物简介");
    let (_, works) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/person/item/list",
            Some(&token),
            json!({"person_guid":id,"job":"Actor"}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(works["data"]["total"], 2);
    assert_eq!(works["data"]["list"][0]["file_name"], "续集.mp4");
    let (_, native_default) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/person/item/list",
            Some(&token),
            json!({"person_guid":id,"job":"Actor","sort_column":"update_time","sort_type":"DESC"}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(native_default["data"]["list"][0]["file_name"], "续集.mp4");
    let (_, alphabetical) = json_response(wire(&state, "POST", "/v/api/v1/person/item/list", Some(&token), json!({"person_guid":id,"job":"Actor","sort_column":"title","sort_type":"ASC","page":2,"page_size":1}), &[]).await).await;
    assert_eq!(alphabetical["data"]["total"], 2);
    assert_eq!(alphabetical["data"]["list"].as_array().unwrap().len(), 1);
    let image_path = format!("/v/api/v1/sys/img{}", profile["profile"].as_str().unwrap());
    let response = wire(&state, "GET", &image_path, Some(&token), Value::Null, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let pixel = image::load_from_memory(&bytes)
        .unwrap()
        .to_rgb8()
        .get_pixel(0, 0)
        .0;
    assert!(pixel[1] > pixel[0]);
    state.vfs.mkdir("/其它/存档").await.unwrap();
    let archive = state.vfs.resolve("/其它/存档").await.unwrap().id;
    state
        .vfs
        .db
        .move_node(new_nfo, archive, "续集.nfo")
        .await
        .unwrap();
    // 无需等索引刷新：已失去作品关联的 NFO 不能继续提供人物简介和作品关系。
    let fallback = detail(&state, &token, id).await;
    assert_eq!(fallback["biography"], old_bio);
    let (_, remaining) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/person/item/list",
            Some(&token),
            json!({"person_guid":id,"job":"Actor"}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(remaining["data"]["total"], 1);
    assert_eq!(
        wire(&state, "GET", &image_path, Some(&token), Value::Null, &[])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/person/{id}"),
            None,
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    state
        .vfs
        .db
        .move_node(old_nfo, root, "存档.nfo")
        .await
        .unwrap();
    assert_eq!(
        actors(&state, &token, item["guid"].as_str().unwrap()).await["total"],
        0
    );
    assert_eq!(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/person/{id}"),
            Some(&token),
            Value::Null,
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn tv_cast_uses_show_metadata_and_guest_works_collapse_to_the_show() {
    let (state, media, _directory, _, _) = configured(b"synthetic").await;
    for path in [
        "/其它/合成剧",
        "/其它/合成剧/Season 1",
        "/其它/合成剧/Season 2",
        "/其它/存档",
    ] {
        state.vfs.mkdir(path).await.unwrap();
    }
    for name in [
        "Season 1/合成剧.S01E01.mp4",
        "Season 1/合成剧.S01E02.mp4",
        "Season 2/合成剧.S02E01.mp4",
    ] {
        seed_file(
            &state,
            &media,
            &format!("/其它/合成剧/{name}"),
            b"synthetic",
            65536,
        )
        .await;
    }
    let show_nfo = seed_file(
        &state,
        &media,
        "/其它/合成剧/tvshow.nfo",
        "<tvshow><actor><name>剧级演员</name><role>主演</role></actor></tvshow>".as_bytes(),
        65536,
    )
    .await;
    seed_file(
        &state,
        &media,
        "/其它/合成剧/Season 1/合成剧.S01E01.nfo",
        "<episodedetails><actor><name>单集客串</name><role>客串</role></actor></episodedetails>"
            .as_bytes(),
        65536,
    )
    .await;
    seed_file(
        &state,
        &media,
        "/其它/合成剧/Season 2/合成剧.S02E01.nfo",
        "<episodedetails><actor><name>第二季演员</name></actor></episodedetails>".as_bytes(),
        65536,
    )
    .await;
    let root = state.vfs.resolve("/其它").await.unwrap().id;
    assert_eq!(manager(&state, "fntv.libraries.set", json!({"libraries":[{"id":"33333333333333333333333333333333","name":"电视剧","kind":"tv","root_id":root}]})).await.0, StatusCode::OK);
    assert_eq!(
        manager(&state, "fntv.index.refresh", json!({})).await.0,
        StatusCode::OK
    );
    let token = login(&state, "Synthetic-Only-Account").await;
    let (_, list) = json_response(
        wire(
            &state,
            "POST",
            "/v/api/v1/item/list",
            Some(&token),
            json!({}),
            &[],
        )
        .await,
    )
    .await;
    let show = list["data"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "TV")
        .unwrap();
    let show_id = show["guid"].as_str().unwrap();
    assert_eq!(
        actors(&state, &token, show_id).await["list"][0]["name"],
        "剧级演员"
    );
    let (_, seasons) = json_response(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/season/list/{show_id}"),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    let season_id = seasons["data"][0]["guid"].as_str().unwrap();
    assert_eq!(
        actors(&state, &token, season_id).await["list"][0]["name"],
        "剧级演员"
    );
    let (_, episodes) = json_response(
        wire(
            &state,
            "GET",
            &format!("/v/api/v1/episode/list/{season_id}"),
            Some(&token),
            Value::Null,
            &[],
        )
        .await,
    )
    .await;
    let first = episodes["data"][0]["guid"].as_str().unwrap();
    let second = episodes["data"][1]["guid"].as_str().unwrap();
    let guest = actors(&state, &token, first).await;
    assert_eq!(guest["list"][0]["name"], "单集客串");
    assert_eq!(
        actors(&state, &token, second).await["list"][0]["name"],
        "剧级演员"
    );
    let guest_id = guest["list"][0]["person_guid"].as_str().unwrap();
    let profile = detail(&state, &token, guest_id).await;
    assert_eq!(profile["actor_item_total"], 1);
    assert_eq!(profile["actor_item_list"][0]["type"], "TV");
    let archive = state.vfs.resolve("/其它/存档").await.unwrap().id;
    state
        .vfs
        .db
        .move_node(show_nfo, archive, "tvshow.nfo")
        .await
        .unwrap();
    let fallback = actors(&state, &token, show_id).await;
    let names: Vec<_> = fallback["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["单集客串", "第二季演员"]);
    assert_eq!(
        actors(&state, &token, season_id).await["list"][0]["name"],
        "单集客串"
    );
    assert_eq!(actors(&state, &token, second).await["total"], 0);
}
