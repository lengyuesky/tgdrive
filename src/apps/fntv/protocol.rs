//! 指定官方客户端已验证的协议组合；所有身份都映射到本服务的授权文件。

use super::{
    Context, authorized, data, guid,
    index::{self, Entry, Source},
    probe::{self, MediaInfo},
};
use crate::{
    api::AppState,
    db::Node,
    error::{AppError, AppResult},
};
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const BODY_LIMIT: usize = 16 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Progress {
    pub ts: f64,
    pub audio_guid: String,
    pub subtitle_guid: String,
    pub direct_link_audio_index: i64,
    pub updated_at: i64,
}

fn envelope(data: Value) -> Response {
    Json(json!({"code":0,"msg":"","data":data})).into_response()
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .filter(|h| h.len() <= 135)
    {
        return Some(value.strip_prefix("Bearer ").unwrap_or(value).into());
    }
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .filter(|h| h.len() <= 4096)
        .and_then(|h| {
            h.split(';')
                .map(str::trim)
                .find_map(|p| p.strip_prefix("Trim-MC-token="))
        })
        .map(str::to_owned)
}

pub(super) async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let result = handle_inner(&state, request).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            let status = match error {
                AppError::NotFound => StatusCode::NOT_FOUND,
                AppError::Forbidden(_) | AppError::SessionExpired => StatusCode::UNAUTHORIZED,
                _ => error.status(),
            };
            let (message, _) = error.public_parts();
            (
                status,
                Json(json!({"code":status.as_u16(),"msg":message,"data":null})),
            )
                .into_response()
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn handle_inner(state: &AppState, request: Request) -> AppResult<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let mut context = super::context(state, true).await?;
    index::start_worker(state);
    let read = method == Method::GET || method == Method::HEAD;
    if path == "/trimcon" && read {
        return Ok(envelope(json!({})));
    }
    if path == "/v/api/v1/sys/version" && read {
        // 沿用指定客户端已接受的协商字段，不把协议样例当作本宿主的产品版本。
        return Ok(envelope(
            json!({"version":"0.9.7","mediasrvVersion":"0.8.35"}),
        ));
    }
    if path == "/v/api/v1/sys/config" && read {
        return Ok(envelope(json!({"initialized":true})));
    }
    if path == "/v/api/v2/user/loginByPassword" {
        if method != Method::POST {
            return Err(AppError::BadRequest("登录接口需要 POST".into()));
        }
        state.vfs.db.fntv.login_budget()?;
        let bytes = to_bytes(request.into_body(), BODY_LIMIT)
            .await
            .map_err(|_| AppError::BadRequest("登录请求过大或无效".into()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::BadRequest("登录请求不是有效 JSON".into()))?;
        let username = string(&value, "username");
        let password = string(&value, "password");
        if username != context.config.username
            || password.len() != 64
            || !password.bytes().all(|b| b.is_ascii_hexdigit())
            || context.config.verifier.is_empty()
            || super::roots(state, &context).await?.is_empty()
        {
            return Err(AppError::Forbidden("影视连接账号或密码无效".into()));
        }
        let verifier = context.config.verifier.clone();
        let candidate = password.to_ascii_lowercase();
        let valid =
            tokio::task::spawn_blocking(move || crate::password::verify(&verifier, &candidate))
                .await
                .map_err(|e| AppError::Internal(e.into()))?;
        if !valid {
            return Err(AppError::Forbidden("影视连接账号或密码无效".into()));
        }
        let token = state.vfs.db.fntv.login(&context.lease)?;
        return Ok(envelope(json!({"token":token})));
    }
    let token =
        token(&headers).ok_or_else(|| AppError::Forbidden("请先登录影视连接账号".into()))?;
    context.lease = state.vfs.db.fntv.authenticate(&token, &context.lease)?;
    let all = index::entries(state).await?;
    let mut entries = Vec::new();
    let direct_reference = path.strip_prefix("/v/api/v1/media/range/");
    let playback_reference = path.strip_prefix("/v/api/v1/stream/list/");
    for entry in all {
        if direct_reference.is_some_and(|id| entry.guid != id && entry.media_guid != id) {
            continue;
        }
        if playback_reference.is_some_and(|id| !matches_playback_reference(&entry, id)) {
            continue;
        }
        if authorized(state, &context, entry.node_id, &entry.version)
            .await
            .is_ok()
        {
            entries.push(entry);
        }
    }
    context.lease.check()?;
    let entries = super::covers::decorate(state, entries).await?;
    if read {
        return read_request(state, &context, &token, &path, &headers, method, &entries).await;
    }
    if method != Method::POST
        && !(matches!(method, Method::PUT | Method::DELETE)
            && matches!(
                path.as_str(),
                "/v/api/v1/item/favorite" | "/v/api/v1/item/watched"
            ))
    {
        return Err(AppError::BadRequest("不支持的协议请求方法".into()));
    }
    let bytes = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| AppError::BadRequest("协议请求超过 16 KiB 或无效".into()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::BadRequest("协议请求不是有效 JSON".into()))?;
    if !value.is_object() {
        return Err(AppError::BadRequest("协议请求必须是对象".into()));
    }
    post_request(state, &context, &token, &path, method, &value, &entries).await
}

async fn progress(state: &AppState, entry: &Entry) -> AppResult<Progress> {
    Ok(data::get(
        &state.vfs.db,
        &format!("fntv:progress:{}", entry.media_guid),
    )
    .await?
    .unwrap_or_default())
}
async fn favorite(state: &AppState, entry: &Entry) -> AppResult<bool> {
    Ok(data::get(
        &state.vfs.db,
        &format!("fntv:favorite:{}", entry.media_guid),
    )
    .await?
    .unwrap_or(false))
}
fn resolution(info: &MediaInfo) -> String {
    info.video()
        .and_then(|v| v["height"].as_u64())
        .map(|h| h.to_string())
        .unwrap_or_default()
}
fn color(info: &MediaInfo) -> String {
    match info.video().and_then(|v| v["color_transfer"].as_str()) {
        Some("smpte2084") => "HDR10".into(),
        Some("arib-std-b67") => "HLG".into(),
        Some("bt709" | "smpte170m" | "iec61966-2-1") => "SDR".into(),
        _ => String::new(),
    }
}
fn track_guid(entry: &Entry, stream: &Value) -> String {
    guid(&["track", &entry.media_guid, &stream["index"].to_string()])
}
fn subtitle_guid(source: &Source) -> String {
    source.guid("subtitle")
}
fn poster(entry: &Entry) -> String {
    entry
        .cover
        .as_ref()
        .map(|s| format!("/tgdrive-poster/{s}"))
        .unwrap_or_default()
}
fn library_info(library: &super::Library, entries: &[Entry]) -> Value {
    let posters = entries
        .iter()
        .filter(|entry| entry.libraries.contains(&library.id))
        .map(poster)
        .filter(|poster| !poster.is_empty())
        .take(4)
        .collect::<Vec<_>>();
    json!({"guid":library.id,"title":library.name,
        "category":if library.kind=="movie" {"Movie"} else {"TV"},
        "view_type":0,"posters":posters})
}
fn season_guid(entry: &Entry) -> String {
    guid(&["season", &entry.show_guid, &entry.season.to_string()])
}
fn entry_for<'a>(entries: &'a [Entry], reference: &str) -> Option<&'a Entry> {
    entries
        .iter()
        .find(|e| e.guid == reference || e.media_guid == reference)
}
fn matches_playback_reference(entry: &Entry, reference: &str) -> bool {
    entry.guid == reference
        || entry.media_guid == reference
        || !reference.is_empty()
            && entry.kind == "Episode"
            && (entry.show_guid == reference || season_guid(entry) == reference)
}
fn playback_entry<'a>(entries: &'a [Entry], reference: &str) -> Option<&'a Entry> {
    entry_for(entries, reference).or_else(|| {
        // 季、剧的播放入口使用分组身份，实际播放仍定位到当前授权的剧集文件。
        entries
            .iter()
            .filter(|entry| matches_playback_reference(entry, reference))
            .min_by(|a, b| {
                a.season
                    .cmp(&b.season)
                    .then_with(|| a.episode.cmp(&b.episode))
                    .then_with(|| a.guid.cmp(&b.guid))
            })
    })
}

async fn item(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    info: Option<&MediaInfo>,
) -> AppResult<Value> {
    let progress = progress(state, entry).await?;
    let favorite = favorite(state, entry).await?;
    let watched =
        match data::get::<bool>(&state.vfs.db, &format!("fntv:watched:{}", entry.media_guid))
            .await?
        {
            Some(value) => value,
            None if entry.kind == "Episode" => data::get::<bool>(
                &state.vfs.db,
                &format!("fntv:group-watched:{}", season_guid(entry)),
            )
            .await?
            .or(data::get::<bool>(
                &state.vfs.db,
                &format!("fntv:group-watched:{}", entry.show_guid),
            )
            .await?)
            .unwrap_or(false),
            None => false,
        };
    let duration = info.map(|m| m.duration.round() as u64).unwrap_or(0);
    let resolution = info.map(resolution).filter(|s| !s.is_empty());
    let color = info.map(color).filter(|s| !s.is_empty());
    let library = context
        .config
        .libraries
        .iter()
        .find(|l| entry.libraries.contains(&l.id));
    let library_id = library.map(|l| l.id.as_str()).unwrap_or("");
    let parent = if entry.kind == "Episode" {
        season_guid(entry)
    } else {
        library_id.into()
    };
    let poster = poster(entry);
    let mut value = json!({"guid":entry.guid,"title":entry.title,"type":entry.kind,"lan":"zh-CN","tv_title":entry.show_title,"parent_title":"",
        "original_title":"","imdb_id":"","trim_id":"","poster":poster,"posters":poster,"backdrops":"","vote_average":"0","genres":[],
        "release_date":entry.year,"overview":entry.overview,"is_favorite":u8::from(favorite),"is_watched":u8::from(watched),"watched":u8::from(watched),"watched_ts":progress.ts as u64,
        "season_number":entry.season,"episode_number":entry.episode,"number_of_seasons":0,"number_of_episodes":0,"local_number_of_episodes":0,"local_number_of_seasons":0});
    value.as_object_mut().unwrap().extend(json!({
        "can_play":1,"play_error":"","parent_guid":parent,"grand_guid":entry.show_guid,"ancestor_guid":library_id,
        "ancestor_name":library.map(|l|l.name.as_str()).unwrap_or(""),"ancestor_category":library.map(|l|if l.kind=="movie" { "Movie" } else { "TV" }).unwrap_or(""),
        "play_item_guid":entry.guid,"media_guid":entry.media_guid,"duration":duration,"logic_type":0,"single_child_guid":"","file_name":entry.filename,
        "create_time":entry.timestamp,"update_time":entry.timestamp,"first_air_date":"","last_air_date":"","air_date":"","status":"","douban_id":0,
        "poster_list":if poster.is_empty(){Vec::<String>::new()}else{vec![poster]},"media_stream":{"resolutions":resolution.into_iter().collect::<Vec<_>>(),"audio_type":null,"color_range_type":color.into_iter().collect::<Vec<_>>()}}).as_object().unwrap().clone());
    Ok(value)
}

fn file(entry: &Entry) -> Value {
    json!({"guid":entry.media_guid,"path":entry.path,"file_name":entry.filename,"size":entry.size,"timestamp":entry.timestamp,
        "type":1,"can_play":1,"play_error":"","create_time":entry.timestamp,"update_time":entry.timestamp,"file_birth_time":entry.timestamp,"progress_thumb_hash_dir":""})
}

fn streams(entry: &Entry, info: &MediaInfo) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    let mut subtitles = Vec::new();
    for stream in &info.streams {
        let mut row = stream.as_object().cloned().unwrap_or_default();
        for (key, value) in [
            ("media_guid", json!(entry.media_guid)),
            ("guid", json!(track_guid(entry, stream))),
            ("duration", json!(info.duration.round() as u64)),
            ("create_time", json!(entry.timestamp)),
            ("update_time", json!(entry.timestamp)),
            (
                "title",
                json!(stream["tags"]["title"].as_str().unwrap_or("")),
            ),
        ] {
            row.insert(key.into(), value);
        }
        row.insert(
            "bps".into(),
            json!(
                stream["bit_rate"]
                    .as_str()
                    .or_else(|| stream["tags"]["BPS"].as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0)
            ),
        );
        match stream["codec_type"].as_str() {
            Some("video") => {
                if stream["disposition"]["attached_pic"] == 1 {
                    continue;
                }
                row.insert("resolution_type".into(), json!(resolution(info)));
                row.insert("color_range_type".into(), json!(color(info)));
                row.insert("wrapper".into(), json!(info.wrapper));
                row.insert(
                    "progressive".into(),
                    json!(u8::from(stream["field_order"] == "progressive")),
                );
                row.insert(
                    "level".into(),
                    json!(
                        stream["level"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                    ),
                );
                let side = stream["side_data_list"].as_array();
                let rotation = side
                    .and_then(|l| l.iter().find_map(|s| s["rotation"].as_i64()))
                    .unwrap_or(0);
                let dv = side
                    .and_then(|l| l.iter().find_map(|s| s["dv_profile"].as_u64()))
                    .unwrap_or(0);
                row.insert("dv_profile".into(), json!(dv));
                row.insert("rotation".into(), json!(rotation));
                row.insert("ext1".into(), json!(0));
                row.insert("is_bluray".into(), json!(false));
                videos.push(Value::Object(row));
            }
            Some("audio") => {
                row.insert(
                    "audio_type".into(),
                    json!(if stream["channels"] == 2 {
                        "Stereo"
                    } else {
                        ""
                    }),
                );
                row.insert(
                    "language".into(),
                    json!(stream["tags"]["language"].as_str().unwrap_or("und")),
                );
                row.insert(
                    "is_default".into(),
                    json!(stream["disposition"]["default"].as_i64().unwrap_or(0)),
                );
                row.insert("is_fake".into(), json!(false));
                audios.push(Value::Object(row));
            }
            Some("subtitle") => {
                row.insert(
                    "language".into(),
                    json!(stream["tags"]["language"].as_str().unwrap_or("und")),
                );
                row.insert(
                    "is_default".into(),
                    json!(stream["disposition"]["default"].as_i64().unwrap_or(0)),
                );
                row.insert(
                    "forced".into(),
                    json!(stream["disposition"]["forced"].as_i64().unwrap_or(0)),
                );
                row.insert("is_external".into(), json!(0));
                row.insert(
                    "is_bitmap".into(),
                    json!(u8::from(matches!(
                        stream["codec_name"].as_str(),
                        Some("hdmv_pgs_subtitle" | "dvd_subtitle")
                    ))),
                );
                subtitles.push(Value::Object(row));
            }
            _ => {}
        }
    }
    for (index, source) in entry.subtitles.iter().enumerate() {
        let ext = index::extension(&source.name);
        subtitles.push(json!({"media_guid":entry.media_guid,"guid":subtitle_guid(source),"title":source.name,
            "codec_name":if ext=="srt" { "subrip" } else { ext.as_str() },"codec_type":"subtitle","language":"und","forced":0,"index":index,
            "is_default":0,"is_external":1,"format":ext,"trim_id":"","source_id":"","Source":"","create_time":entry.timestamp,
            "update_time":entry.timestamp,"extra_file":0,"is_bitmap":0,"file_size":source.size}));
    }
    (videos, audios, subtitles)
}

async fn read_request(
    state: &AppState,
    context: &Context,
    token: &str,
    path: &str,
    headers: &HeaderMap,
    method: Method,
    entries: &[Entry],
) -> AppResult<Response> {
    match path {
        "/v/api/v1/user/info" => {
            return Ok(envelope(
                json!({"guid":guid(&["user",&context.config.username]),"username":context.config.username,"is_admin":0}),
            ));
        }
        "/v/api/v1/mediadb/list" => {
            let libraries = super::roots(state, context).await?;
            return Ok(envelope(json!(
                libraries
                    .into_iter()
                    .map(|(library, _)| library_info(&library, entries))
                    .collect::<Vec<_>>()
            )));
        }
        "/v/api/v1/mediadb/sum" => {
            let movies = entries.iter().filter(|e| e.kind == "Movie").count();
            let tv = entries
                .iter()
                .filter(|e| e.kind == "Episode")
                .map(|e| &e.show_guid)
                .collect::<std::collections::HashSet<_>>()
                .len();
            let other = entries.iter().filter(|e| e.kind == "Video").count();
            let mut value = json!({"total":movies+tv+other,"movie":movies,"tv":tv,"live":0,"other":other,"Movie":movies,"TV":tv,"Video":other,"Directory":0});
            for l in &context.config.libraries {
                value[&l.id] = json!(
                    entries
                        .iter()
                        .filter(|e| e.libraries.contains(&l.id))
                        .count()
                );
            }
            return Ok(envelope(value));
        }
        "/v/api/v1/play/list" => {
            let mut list = Vec::new();
            for e in entries {
                let p = progress(state, e).await?;
                if p.updated_at > 0 {
                    let info = probe::cached(state, e).await?;
                    let mut row = item(state, context, e, info.as_ref()).await?;
                    row["ts"] = json!(p.ts as u64);
                    list.push((p.updated_at, row));
                }
            }
            list.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
            return Ok(envelope(json!(
                list.into_iter()
                    .take(100)
                    .map(|(_, v)| v)
                    .collect::<Vec<_>>()
            )));
        }
        "/v/api/v1/tag/genres"
        | "/v/api/v1/tag/iso3166"
        | "/v/api/v1/tag/iso6392"
        | "/v/api/v1/tag/iso6391" => return Ok(envelope(json!([]))),
        "/v/api/v1/tag/list" => {
            return Ok(envelope(
                json!({"genres":[],"resolutions":[],"color_range":[],"audio_type":[],"locate":[],"decades":[],"recognition_status":[]}),
            ));
        }
        _ => {}
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/mdb/") {
        let library = super::roots(state, context)
            .await?
            .into_iter()
            .find(|(library, _)| library.id == reference)
            .ok_or(AppError::NotFound)?;
        return Ok(envelope(library_info(&library.0, entries)));
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/item/") {
        if let Some((library, _)) = super::roots(state, context)
            .await?
            .into_iter()
            .find(|(library, _)| library.id == reference)
        {
            let info = library_info(&library, entries);
            return Ok(envelope(
                json!({"guid":library.id,"title":library.name,"type":"Directory",
                "ancestor_guid":library.id,"ancestor_name":library.name,"ancestor_category":info["category"],
                "parent_guid":"","grand_guid":"","can_play":0,"play_error":"","play_item_guid":"",
                "media_guid":"","single_child_guid":"","file_name":"","duration":0,"logic_type":0,
                "is_favorite":0,"is_watched":0,"watched":0,"watched_ts":0,"poster":"","posters":"",
                "vote_average":"0",
                "poster_list":info["posters"],"tv_title":"","parent_title":"","genres":[],"overview":""}),
            ));
        }
        if let Some(entry) = entry_for(entries, reference) {
            let info = probe::cached(state, entry).await?;
            return Ok(envelope(item(state, context, entry, info.as_ref()).await?));
        }
        let groups = group_items(state, context, entries, reference).await?;
        if let Some(item) = groups.into_iter().find(|i| i["guid"] == reference) {
            return Ok(envelope(item));
        }
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/season/list/") {
        let seasons = group_items(state, context, entries, reference)
            .await?
            .into_iter()
            .filter(|i| i["type"] == "Season")
            .collect::<Vec<_>>();
        return Ok(envelope(json!(seasons)));
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/episode/list/") {
        let mut selected = entries
            .iter()
            .filter(|e| e.kind == "Episode" && season_guid(e) == reference)
            .collect::<Vec<_>>();
        selected.sort_by_key(|e| e.episode);
        let mut list = Vec::new();
        for e in selected {
            let info = probe::cached(state, e).await?;
            list.push(item(state, context, e, info.as_ref()).await?);
        }
        return Ok(envelope(json!(list)));
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/stream/list/") {
        let entry = playback_entry(entries, reference).ok_or(AppError::NotFound)?;
        let info = probe::media(state, context, entry).await?;
        let (video, audio, subtitle) = streams(entry, &info);
        return Ok(envelope(
            json!({"files":[file(entry)],"video_streams":video,"audio_streams":audio,"subtitle_streams":subtitle}),
        ));
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/media/range/") {
        let foreground = super::covers::foreground(state);
        let entry = entry_for(entries, reference).ok_or(AppError::NotFound)?;
        let (node, _) = authorized(state, context, entry.node_id, &entry.version).await?;
        let response =
            stream_response(state, context, &node, headers, method.clone(), None).await?;
        if method == Method::GET
            && matches!(
                response.status(),
                StatusCode::OK | StatusCode::PARTIAL_CONTENT
            )
        {
            let _guard = state.vfs.db.fntv.play_lock.lock().await;
            if !state
                .vfs
                .db
                .fntv
                .play(token, &entry.media_guid)
                .is_some_and(|p| p.read && !p.ended)
            {
                let serial = data::activate_play(&state.vfs.db, context, &entry.media_guid).await?;
                state
                    .vfs
                    .db
                    .fntv
                    .activate(token, &entry.media_guid, serial, &context.lease)?;
            }
        }
        return Ok(response.map(|body| super::covers::foreground_body(body, foreground)));
    }
    if let Some(reference) = path.strip_prefix("/v/api/v1/subtitle/dl/") {
        for entry in entries {
            if let Some(source) = entry
                .subtitles
                .iter()
                .find(|s| subtitle_guid(s) == reference)
            {
                return source_response(
                    state,
                    context,
                    entry,
                    source,
                    headers,
                    method,
                    Some(match index::extension(&source.name).as_str() {
                        "srt" => "application/x-subrip; charset=utf-8",
                        "vtt" => "text/vtt; charset=utf-8",
                        _ => "text/plain; charset=utf-8",
                    }),
                )
                .await;
            }
        }
    }
    let reference = path
        .strip_prefix("/v/api/v1/sys/img/tgdrive-poster/")
        .or_else(|| path.strip_prefix("/tgdrive-poster/"));
    if let Some(reference) = reference {
        for entry in entries {
            if entry.cover.as_deref() == Some(reference) {
                return super::covers::response(state, context, entry, reference, headers, method)
                    .await;
            }
        }
    }
    Err(AppError::NotFound)
}

async fn source_response(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    source: &Source,
    headers: &HeaderMap,
    method: Method,
    mime: Option<&str>,
) -> AppResult<Response> {
    let (video, _) = authorized(state, context, entry.node_id, &entry.version).await?;
    let (node, _) = authorized(state, context, source.id, &source.version).await?;
    if node.parent_id != video.parent_id {
        return Err(AppError::Forbidden("附件已移出视频所在目录".into()));
    }
    stream_response(state, context, &node, headers, method, mime).await
}

async fn post_request(
    state: &AppState,
    context: &Context,
    token: &str,
    path: &str,
    method: Method,
    value: &Value,
    entries: &[Entry],
) -> AppResult<Response> {
    if path == "/v/api/v1/user/logout" {
        state.vfs.db.fntv.logout(token);
        return Ok(envelope(json!(true)));
    }
    if matches!(path, "/v/api/v1/user/getData" | "/v/api/v1/user/setData") {
        let key = string(value, "key");
        let library = string(value, "mdb_guid");
        if key.is_empty()
            || key.len() > 128
            || (!library.is_empty() && !context.config.libraries.iter().any(|l| l.id == library))
        {
            return Err(AppError::BadRequest("偏好设置键或媒体库无效".into()));
        }
        let storage_key = format!("fntv:preference:{}", guid(&[library, key]));
        if path.ends_with("getData") {
            let stored: Option<String> = data::get(&state.vfs.db, &storage_key).await?;
            return Ok(stored
                .map(|v| envelope(json!({"key":key,"mdb_guid":library,"value":v})))
                .unwrap_or_else(|| Json(json!({"code":-6,"msg":"","data":null})).into_response()));
        }
        let preference = value
            .get("value")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 8192)
            .ok_or_else(|| AppError::BadRequest("偏好设置需为不超过 8 KiB 的字符串".into()))?;
        data::save(&state.vfs.db, context, &storage_key, &preference).await?;
        return Ok(envelope(json!(true)));
    }
    if path == "/v/api/v1/item/list" {
        return item_list(state, context, value, entries).await;
    }
    if path == "/v/api/v1/favorite/list" {
        let ancestor = string(value, "ancestor_guid");
        let mut list = Vec::new();
        for entry in entries {
            if (ancestor.is_empty()
                || entry.libraries.iter().any(|l| l == ancestor)
                || entry.show_guid == ancestor
                || season_guid(entry) == ancestor)
                && favorite(state, entry).await?
            {
                let info = probe::cached(state, entry).await?;
                list.push(item(state, context, entry, info.as_ref()).await?);
            }
        }
        let groups = group_items(state, context, entries, ancestor).await?;
        for group in groups {
            if group["is_favorite"] == 1 {
                list.push(group.clone());
            }
            if group["type"] == "TV" {
                for season in group_items(
                    state,
                    context,
                    entries,
                    group["guid"].as_str().unwrap_or(""),
                )
                .await?
                .into_iter()
                .filter(|i| i["type"] == "Season" && i["is_favorite"] == 1)
                {
                    list.push(season);
                }
            }
        }
        return page_rows(context, value, list);
    }
    if path == "/v/api/v1/play/quit" {
        let link = string(value, "play_link");
        if link.len() > 128 {
            return Err(AppError::BadRequest("播放标识过长".into()));
        }
        let _guard = state.vfs.db.fntv.play_lock.lock().await;
        state.vfs.db.fntv.quit(token, link);
        return Ok(envelope(json!(true)));
    }
    let reference = if !string(value, "media_guid").is_empty() {
        string(value, "media_guid")
    } else {
        string(value, "item_guid")
    };
    if matches!(path, "/v/api/v1/item/favorite" | "/v/api/v1/item/watched") {
        let is_favorite = path.ends_with("favorite");
        if is_favorite && method != Method::PUT && method != Method::DELETE {
            return Err(AppError::BadRequest("收藏操作需要 PUT 或 DELETE".into()));
        }
        let key = if let Some(entry) = entry_for(entries, reference) {
            format!(
                "fntv:{}:{}",
                if is_favorite { "favorite" } else { "watched" },
                entry.media_guid
            )
        } else if group_items(state, context, entries, reference)
            .await?
            .iter()
            .any(|i| i["guid"] == reference)
        {
            format!(
                "fntv:group-{}:{reference}",
                if is_favorite { "favorite" } else { "watched" }
            )
        } else {
            return Err(AppError::NotFound);
        };
        let enabled = method != Method::DELETE;
        data::save(&state.vfs.db, context, &key, &enabled).await?;
        return Ok(envelope(json!(true)));
    }
    let entry = if path == "/v/api/v1/play/info" {
        playback_entry(entries, reference)
    } else {
        entry_for(entries, reference)
    }
    .ok_or(AppError::NotFound)?;
    let info = probe::media(state, context, entry).await?;
    let (videos, audios, subtitles) = streams(entry, &info);
    match path {
        "/v/api/v1/play/info" => {
            let progress = progress(state, entry).await?;
            return Ok(envelope(
                json!({"guid":entry.guid,"grand_guid":entry.show_guid,"parent_guid":if entry.kind=="Episode" {season_guid(entry)}else{entry.libraries[0].clone()},"type":entry.kind,
                "media_guid":entry.media_guid,"video_guid":videos.first().and_then(|v|v["guid"].as_str()).unwrap_or(""),
                "audio_guid":if progress.audio_guid.is_empty(){audios.first().and_then(|v|v["guid"].as_str()).unwrap_or("").to_owned()}else{progress.audio_guid},
                "subtitle_guid":progress.subtitle_guid,"direct_link_audio_index":progress.direct_link_audio_index,"ts":progress.ts as u64,"play_config":{"skip_opening":null,"skip_ending":null},"item":item(state,context,entry,Some(&info)).await?}),
            ));
        }
        "/v/api/v1/stream" => {
            let video = string(value, "video_guid");
            let audio = string(value, "audio_guid");
            let subtitle = string(value, "subtitle_guid");
            if !video.is_empty() && !videos.iter().any(|v| v["guid"] == video)
                || !audio.is_empty() && !audios.iter().any(|v| v["guid"] == audio)
                || !subtitle.is_empty()
                    && subtitle != "_no_display_"
                    && !subtitles.iter().any(|v| v["guid"] == subtitle)
            {
                return Err(AppError::BadRequest("所选媒体轨道无效".into()));
            }
            let _guard = state.vfs.db.fntv.play_lock.lock().await;
            state
                .vfs
                .db
                .fntv
                .select_play(token, &entry.media_guid, string(value, "ip"))?;
            return Ok(envelope(
                json!({"file_stream":file(entry),"video_stream":videos.first(),"audio_streams":audios,"subtitle_streams":subtitles,
                "qualities":[{"bitrate":0,"resolution":"原画","progressive":false,"is_m3u8":false}],"direct_link_qualities":[],"cloud_storage_info":null}),
            ));
        }
        "/v/api/v1/play/play" => {
            // 可接受显式使用该接口的客户端；访问授权仍只来自影视登录会话。
            let _guard = state.vfs.db.fntv.play_lock.lock().await;
            state
                .vfs
                .db
                .fntv
                .select_play(token, &entry.media_guid, "")?;
            return Ok(envelope(json!({"play_link":crate::db::random_token(36)})));
        }
        "/v/api/v1/play/record" => {
            let ts = value["ts"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= info.duration + 2.0)
                .ok_or_else(|| AppError::BadRequest("观看位置超出真实视频时长".into()))?;
            let link = string(value, "play_link");
            let audio = string(value, "audio_guid");
            let subtitle = string(value, "subtitle_guid");
            if string(value, "item_guid") != entry.guid
                || string(value, "media_guid") != entry.media_guid
                || link.is_empty()
                || link.len() > 128
                || !audio.is_empty() && !audios.iter().any(|v| v["guid"] == audio)
                || !subtitle.is_empty()
                    && subtitle != "_no_display_"
                    && !subtitles.iter().any(|v| v["guid"] == subtitle)
            {
                return Err(AppError::BadRequest("观看记录与媒体或轨道不匹配".into()));
            }
            let audio_index = value
                .get("direct_link_audio_index")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if audio_index < -1 || audio_index > audios.len() as i64 {
                return Err(AppError::BadRequest("音轨序号无效".into()));
            }
            let _guard = state.vfs.db.fntv.play_lock.lock().await;
            if let Some(serial) = state
                .vfs
                .db
                .fntv
                .bind_link(token, &entry.media_guid, link)?
            {
                let progress = Progress {
                    ts,
                    audio_guid: audio.into(),
                    subtitle_guid: if subtitle == "_no_display_" {
                        ""
                    } else {
                        subtitle
                    }
                    .into(),
                    direct_link_audio_index: audio_index,
                    updated_at: crate::db::now_ts(),
                };
                let saved =
                    data::progress(&state.vfs.db, context, &entry.media_guid, serial, &progress)
                        .await?;
                state.vfs.db.fntv.record_result(saved);
            }
            return Ok(envelope(json!(true)));
        }
        _ => {}
    }
    Err(AppError::NotFound)
}

async fn group_items(
    state: &AppState,
    context: &Context,
    entries: &[Entry],
    ancestor: &str,
) -> AppResult<Vec<Value>> {
    let mut rows = std::collections::BTreeMap::<String, Value>::new();
    let mut episodes: Vec<_> = entries.iter().filter(|e| e.kind == "Episode").collect();
    episodes.sort_by(|a, b| {
        a.season
            .cmp(&b.season)
            .then_with(|| a.episode.cmp(&b.episode))
            .then_with(|| a.guid.cmp(&b.guid))
    });
    let mut representatives = std::collections::HashMap::new();
    for entry in &episodes {
        if entry.cover.is_some() {
            representatives
                .entry(entry.show_guid.clone())
                .or_insert(*entry);
            representatives.entry(season_guid(entry)).or_insert(*entry);
        }
    }
    for entry in episodes {
        let show = entry.show_guid.clone();
        let season = season_guid(entry);
        if rows.contains_key(&show) && rows.contains_key(&season) {
            continue;
        }
        for (id, kind, title, parent) in [
            (
                show.clone(),
                "TV",
                entry.show_title.clone(),
                entry.libraries[0].clone(),
            ),
            (
                season,
                "Season",
                format!("第 {} 季", entry.season),
                show.clone(),
            ),
        ] {
            let include = ancestor == id
                || (kind == "TV"
                    && (ancestor.is_empty() || entry.libraries.iter().any(|l| l == ancestor)))
                || (kind == "Season" && ancestor == show);
            if !include || rows.contains_key(&id) {
                continue;
            }
            let mut base = item(state, context, entry, None).await?;
            base["guid"] = json!(id);
            base["type"] = json!(kind);
            base["title"] = json!(title);
            base["parent_guid"] = json!(parent);
            if let Some(representative) = representatives.get(&id) {
                let image = poster(representative);
                base["poster"] = json!(image);
                base["posters"] = json!(image);
                base["poster_list"] = json!([image]);
            } else {
                base["poster"] = json!("");
                base["posters"] = json!("");
                base["poster_list"] = json!([]);
            }
            base["number_of_episodes"] = json!(
                entries
                    .iter()
                    .filter(|e| e.show_guid == entry.show_guid
                        && (kind == "TV" || e.season == entry.season))
                    .count()
            );
            base["number_of_seasons"] = json!(
                entries
                    .iter()
                    .filter(|e| e.show_guid == entry.show_guid)
                    .map(|e| e.season)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            );
            base["local_number_of_episodes"] = base["number_of_episodes"].clone();
            base["local_number_of_seasons"] = base["number_of_seasons"].clone();
            base["is_favorite"] = json!(u8::from(
                data::get::<bool>(&state.vfs.db, &format!("fntv:group-favorite:{id}"))
                    .await?
                    .unwrap_or(false)
            ));
            base["is_watched"] = json!(u8::from(
                data::get::<bool>(&state.vfs.db, &format!("fntv:group-watched:{id}"))
                    .await?
                    .unwrap_or(false)
            ));
            base["watched"] = base["is_watched"].clone();
            base["can_play"] = json!(0);
            rows.insert(id, base);
        }
    }
    Ok(rows.into_values().collect())
}

async fn item_list(
    state: &AppState,
    context: &Context,
    value: &Value,
    entries: &[Entry],
) -> AppResult<Response> {
    let ancestor = string(value, "ancestor_guid");
    let mut list = group_items(state, context, entries, ancestor).await?;
    for entry in entries {
        let include = ancestor.is_empty() && entry.kind != "Episode"
            || entry.kind != "Episode" && entry.libraries.iter().any(|l| l == ancestor)
            || entry.kind == "Episode" && season_guid(entry) == ancestor;
        if !include {
            continue;
        }
        let info = probe::cached(state, entry).await?;
        list.push(item(state, context, entry, info.as_ref()).await?);
    }
    page_rows(context, value, list)
}

fn page_rows(context: &Context, value: &Value, mut list: Vec<Value>) -> AppResult<Response> {
    let page = value.get("page").and_then(Value::as_u64).unwrap_or(1);
    let page_size = value.get("page_size").and_then(Value::as_u64).unwrap_or(50);
    if page == 0 || page > 10000 || !(1..=128).contains(&page_size) {
        return Err(AppError::BadRequest("列表分页超出上限".into()));
    }
    let ancestor = string(value, "ancestor_guid");
    let q = string(value, "q").to_lowercase();
    if !q.is_empty() {
        list.retain(|i| {
            i["title"]
                .as_str()
                .is_some_and(|t| t.to_lowercase().contains(&q))
        });
    }
    if let Some(types) = value["tags"]["type"].as_array().filter(|t| !t.is_empty()) {
        list.retain(|i| {
            types.iter().any(|t| t == &i["type"])
                || i["type"] == "Video" && types.iter().any(|t| t == "TV")
        });
    }
    list.sort_by(|a, b| {
        a["title"]
            .as_str()
            .cmp(&b["title"].as_str())
            .then_with(|| a["guid"].as_str().cmp(&b["guid"].as_str()))
    });
    let total = list.len();
    let library = context.config.libraries.iter().find(|l| l.id == ancestor);
    Ok(envelope(
        json!({"mdb_name":library.map(|l|l.name.as_str()).unwrap_or(""),"mdb_category":library.map(|l|if l.kind=="movie"{"Movie"}else{"TV"}).unwrap_or(""),
        "top_dir":"","dir":"","total":total,"jump_list":[],"list":list.into_iter().skip(((page-1)*page_size) as usize).take(page_size as usize).collect::<Vec<_>>() }),
    ))
}

pub(super) async fn byte_stream(
    state: &AppState,
    context: &Context,
    node: &Node,
    start: u64,
    length: u64,
) -> AppResult<Body> {
    guarded_stream(state, context, node, start, length, None).await
}

pub(super) async fn probe_stream(
    state: &AppState,
    context: &Context,
    node: &Node,
    start: u64,
    length: u64,
    budget: std::sync::Arc<crate::telegram::ReadBudget>,
) -> AppResult<Body> {
    guarded_stream(state, context, node, start, length, Some(budget)).await
}

async fn guarded_stream(
    state: &AppState,
    context: &Context,
    node: &Node,
    start: u64,
    length: u64,
    budget: Option<std::sync::Arc<crate::telegram::ReadBudget>>,
) -> AppResult<Body> {
    let mut reader = state
        .vfs
        .open_reader(node, start)
        .await?
        .with_read_limit(length)
        .with_strict_ranges();
    if let Some(budget) = budget {
        reader = reader.with_probe_budget(budget);
    }
    let mut first = [0u8; 1];
    let count = if length == 0 {
        0
    } else {
        tokio::select! {
            biased;
            _=context.lease.cancelled.cancelled()=>return Err(AppError::AppChanged),
            result=reader.read(&mut first)=>result.map_err(|error| {
                if error.get_ref().and_then(|e| e.downcast_ref::<AppError>()).is_some_and(|e| matches!(e,AppError::Telegram(_) | AppError::Io(_))) {
                    AppError::Telegram("媒体分段读取暂时失败".into())
                } else {
                    AppError::BadRequest("当前文件无法严格分段读取；请检查宿主 MTProto 或现有本地缓存，已拒绝整块下载回退".into())
                }
            })?,
        }
    };
    let prefix = if count > 0 {
        Some(bytes::Bytes::copy_from_slice(&first[..count]))
    } else {
        None
    };
    let state = state.clone();
    let context = context.clone();
    let id = node.id;
    let version = node.content_version();
    let stream = futures_util::stream::try_unfold(
        (reader, state, context, version, prefix),
        move |(mut reader, state, context, version, mut prefix)| async move {
            context.lease.check().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "影视服务连接已撤销")
            })?;
            authorized(&state, &context, id, &version)
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "媒体读取授权已失效")
                })?;
            if let Some(bytes) = prefix.take() {
                return Ok(Some((bytes, (reader, state, context, version, None))));
            }
            let mut bytes = vec![0; 64 * 1024];
            let length = tokio::select! {
                biased;
                _=context.lease.cancelled.cancelled()=>return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,"影视传输已中止")),
                _=state.restore.shutdown.cancelled()=>return Err(std::io::Error::new(std::io::ErrorKind::Interrupted,"宿主正在停止")),
                result=reader.read(&mut bytes)=>result?,
            };
            if length == 0 {
                return Ok(None);
            }
            authorized(&state, &context, id, &version)
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "媒体读取授权已失效")
                })?;
            bytes.truncate(length);
            Ok::<_, std::io::Error>(Some((
                bytes::Bytes::from(bytes),
                (reader, state, context, version, None),
            )))
        },
    );
    Ok(Body::from_stream(stream))
}

async fn stream_response(
    state: &AppState,
    context: &Context,
    node: &Node,
    headers: &HeaderMap,
    method: Method,
    mime: Option<&str>,
) -> AppResult<Response> {
    let etag = format!("\"{}\"", node.content_version());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(str::trim)
                .any(|t| t == "*" || t.trim_start_matches("W/") == etag)
        })
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap());
    }
    let size = node.size as u64;
    let range = if headers
        .get(header::IF_RANGE)
        .is_some_and(|v| v.to_str().ok() != Some(etag.as_str()))
    {
        Ok(None)
    } else {
        crate::api::parse_range(headers, size)
    };
    let range = match range {
        Ok(range) => range,
        Err(()) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::ETAG, etag)
                .body(Body::empty())
                .unwrap());
        }
    };
    let (start, length) = range.map(|(s, e)| (s, e - s + 1)).unwrap_or((0, size));
    let guessed = mime_guess::from_path(&node.name)
        .first_or_octet_stream()
        .to_string();
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, mime.unwrap_or(&guessed))
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some((start, end)) = range {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        byte_stream(state, context, node, start, length).await?
    };
    Ok(response.body(body).unwrap())
}
