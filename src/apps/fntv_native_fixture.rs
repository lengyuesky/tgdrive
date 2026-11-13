//! 显式官方客户端验收窗口：校验合成素材身份，临时 SQLite、内存存储和真实宿主路由。
use super::*;
use sha2::{Digest, Sha256};

pub(super) fn assets() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(
        std::env::var("TGDRIVE_FNTV_SYNTHETIC_DIR").expect("需显式指定飞牛合成素材目录"),
    );
    assert!(
        std::fs::symlink_metadata(&path).unwrap().is_dir(),
        "合成素材目录不能是符号链接"
    );
    let marker: Value =
        serde_json::from_slice(&std::fs::read(path.join("__fntv_synthetic.json")).unwrap())
            .unwrap();
    assert_eq!(marker["fixture"], "tgdrive-fntv-synthetic");
    assert_eq!(marker["synthetic"], true);
    for name in ["sample.mp4", "sample.mkv", "sample.srt", "poster.jpg"] {
        let file = path.join(name);
        let info = std::fs::symlink_metadata(&file).unwrap();
        assert!(info.is_file());
        assert!(info.len() <= 64 * 1024 * 1024);
        let bytes = std::fs::read(file).unwrap();
        assert_eq!(
            crate::engine::upload::to_hex(&Sha256::digest(&bytes)),
            marker["sha256"][name].as_str().unwrap()
        );
    }
    path
}

async fn seed_sorting_media(state: &AppState, media: &MemoryMedia, assets: &std::path::Path) {
    let video = std::fs::read(assets.join("sample.mp4")).unwrap();
    let poster = std::fs::read(assets.join("poster.jpg")).unwrap();
    for directory in [
        "/飞牛合成验证/排序电影",
        "/飞牛合成验证/排序剧集",
        "/飞牛合成验证/排序剧集/A 对照剧",
        "/飞牛合成验证/排序剧集/A 对照剧/Season01",
        "/飞牛合成验证/排序剧集/B 更新剧",
        "/飞牛合成验证/排序剧集/B 更新剧/Season01",
        "/飞牛合成验证/排序剧集/B 更新剧/Season02",
    ] {
        state.vfs.mkdir(directory).await.unwrap();
        seed_file(
            state,
            media,
            &format!("{directory}/poster.jpg"),
            &poster,
            256 * 1024,
        )
        .await;
    }
    for (relative, day) in [
        ("排序电影/A 最早添加.mp4", 0),
        ("排序电影/B 最新添加.mp4", 3),
        ("排序电影/C 中间添加.mp4", 2),
        ("排序电影/D 较早添加.mp4", 1),
        ("排序剧集/A 对照剧/Season01/A 对照剧 S01E01.mp4", 2),
        ("排序剧集/B 更新剧/Season01/B 更新剧 S01E01.mp4", 0),
        ("排序剧集/B 更新剧/Season01/B 更新剧 S01E02.mp4", 4),
        ("排序剧集/B 更新剧/Season02/B 更新剧 S02E01.mp4", 3),
    ] {
        let node = seed_file(
            state,
            media,
            &format!("/飞牛合成验证/{relative}"),
            &video,
            256 * 1024,
        )
        .await;
        // 添加、修改和标题顺序各不相同，防止客户端排序看起来生效却使用了错误字段。
        sqlx::query("UPDATE nodes SET created_at = ?, modified_at = ? WHERE id = ?")
            .bind(1_700_000_000_i64 + day * 86400)
            .bind(1_710_000_000_i64 - day * 86400)
            .bind(node)
            .execute(&state.vfs.db.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "需显式指定已校验的合成素材和输出目录，提供官方客户端隔离验收窗口"]
async fn fntv_native_fixture() {
    let assets = assets();
    let seconds = std::env::var("TGDRIVE_FNTV_FIXTURE_SECONDS")
        .map(|value| value.parse::<u64>().expect("验收秒数必须为整数"))
        .unwrap_or(1200);
    assert!(
        (60..=3600).contains(&seconds),
        "验收窗口必须为 60 至 3600 秒"
    );
    let output = std::path::PathBuf::from(
        std::env::var("TGDRIVE_FNTV_FIXTURE_OUTPUT").expect("需显式指定验收输出目录"),
    );
    std::fs::create_dir_all(&output).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let media = Arc::new(MemoryMedia::default());
    // 仅合成夹具限速，让拖动可以触发尚未下载区间的真实 Range 请求。
    media.rate.store(768 * 1024, Ordering::SeqCst);
    let state = state_at(&directory.path().join("fntv-native.db"), media.clone()).await;
    // 仅显式夹具可暂缓后台补图，供官方客户端验证从占位到真实图片的转换。
    let cover_gate = tokio_util::sync::CancellationToken::new();
    let paused = std::env::var("TGDRIVE_FNTV_FIXTURE_PAUSE_COVERS").is_ok_and(|value| value == "1");
    if paused {
        let gate_state = state.clone();
        let release = cover_gate.clone();
        let (ready, waiting) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = gate_state.vfs.db.fntv.cover_lock.lock().await;
            let _ = ready.send(());
            release.cancelled().await;
        });
        waiting.await.unwrap();
    }
    for path in [
        "/飞牛合成验证",
        "/飞牛合成验证/电影",
        "/飞牛合成验证/纯视频",
        "/飞牛合成验证/缩略图",
        "/飞牛合成验证/剧集",
        "/飞牛合成验证/剧集/合成剧",
        "/飞牛合成验证/剧集/合成剧/Season01",
        "/不授权合成目录",
    ] {
        state.vfs.mkdir(path).await.unwrap();
    }
    for (name, remote) in [
        ("sample.mp4", "/飞牛合成验证/电影/合成 MP4.mp4"),
        ("sample.mkv", "/飞牛合成验证/电影/合成 MKV.mkv"),
        ("sample.srt", "/飞牛合成验证/电影/合成 MKV.srt"),
        ("poster.jpg", "/飞牛合成验证/电影/poster.jpg"),
        ("sample.mp4", "/飞牛合成验证/纯视频/无海报 MP4.mp4"),
        ("sample.mkv", "/飞牛合成验证/纯视频/无海报 MKV.mkv"),
        (
            "sample.mp4",
            "/飞牛合成验证/剧集/合成剧/Season01/合成剧 S01E01.mp4",
        ),
    ] {
        seed_file(
            &state,
            &media,
            remote,
            &std::fs::read(assets.join(name)).unwrap(),
            256 * 1024,
        )
        .await;
    }
    let thumbnail_video = seed_file(
        &state,
        &media,
        "/飞牛合成验证/缩略图/已有小图 MP4.mp4",
        &std::fs::read(assets.join("sample.mp4")).unwrap(),
        256 * 1024,
    )
    .await;
    let thumbnail = std::fs::read(assets.join("poster.jpg")).unwrap();
    assert!(thumbnail.len() <= 512 * 1024);
    media
        .files
        .lock()
        .unwrap()
        .insert("fntv-synthetic-thumbnail".into(), thumbnail);
    sqlx::query("UPDATE chunks SET tg_thumbnail_file_id='fntv-synthetic-thumbnail' WHERE node_id=? AND idx=0")
        .bind(thumbnail_video).execute(&state.vfs.db.pool).await.unwrap();
    seed_file(&state,&media,"/飞牛合成验证/电影/合成 MP4.nfo","<movie><title>TGDrive 合成 MP4 5分钟</title><plot>独立临时宿主中的合成验证视频。</plot></movie>".as_bytes(),1024).await;
    seed_file(&state,&media,"/飞牛合成验证/电影/合成 MKV.nfo","<movie><title>TGDrive 合成 MKV 双音轨</title><plot>合成的 440 Hz、880 Hz 音轨与外挂文本字幕。</plot></movie>".as_bytes(),1024).await;
    seed_file(
        &state,
        &media,
        "/不授权合成目录/隔离检查.mp4",
        b"this is synthetic and must stay outside libraries",
        1024,
    )
    .await;
    let sorting = std::env::var("TGDRIVE_FNTV_FIXTURE_SORTING").is_ok_and(|value| value == "1");
    if sorting {
        seed_sorting_media(&state, &media, &assets).await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stats_state = state.clone();
    let stats_media = media.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let stop_request = stop.clone();
    let stop_state = state.clone();
    let release_covers = cover_gate.clone();
    let request_log = Arc::new(StdMutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output.join("native-requests.jsonl"))
            .unwrap(),
    ));
    let fixture=axum::Router::new().nest("/api",crate::api::router(state.clone())).merge(crate::apps::fntv::router(state.clone()))
        .route("/__fntv_fixture",axum::routing::get(move||{
            let state=stats_state.clone();let media=stats_media.clone();async move {
                let ranges=media.ranges.lock().unwrap();
                Json(json!({"fixture":"tgdrive-fntv-native","synthetic":true,"telegram":"memory","host_version":env!("CARGO_PKG_VERSION"),
                    "protocol_stats":state.vfs.db.fntv.protocol_stats(),
                    "memory":{"range_requests":ranges.len(),"requested_bytes":ranges.iter().map(|r|r.1).sum::<u64>(),"nonzero_offsets":ranges.iter().filter(|r|r.0>0).count(),"active_readers":media.active.load(Ordering::SeqCst)}}))
            }
        }))
        .route("/__fntv_fixture/stop",axum::routing::post(move||{
            let signal=stop_request.clone();let state=stop_state.clone();async move {
                state.vfs.db.fntv.invalidate();signal.cancel();StatusCode::NO_CONTENT
            }
        }))
        .route("/__fntv_fixture/release-covers",axum::routing::post(move||{
            let release=release_covers.clone();async move {release.cancel();StatusCode::NO_CONTENT}
        }))
        .fallback_service(crate::web::router("frontend/dist"))
        .layer(middleware::from_fn_with_state(false,crate::security::response_headers))
        .layer(middleware::from_fn(move|request:axum::extract::Request,next:axum::middleware::Next|{
            let log=request_log.clone();async move {
                let path=request.uri().path().to_owned();let method=request.method().to_string();
                let agent=request.headers().get("user-agent").and_then(|value|value.to_str().ok()).unwrap_or("").chars().take(256).collect::<String>();
                let is_list=sorting && method=="POST" && matches!(path.as_str(),"/v/api/v1/item/list"|"/v/api/v1/favorite/list");
                let (request,sort)=if is_list {
                    let (parts,body)=request.into_parts();
                    let Ok(bytes)=to_bytes(body,16*1024).await else {return StatusCode::BAD_REQUEST.into_response()};
                    let value=serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                    let mut fields=serde_json::Map::new();
                    for key in ["sort_column","sort_type"] {
                        let safe=value[key].as_str().filter(|s|s.len()<=64 && s.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'_'));
                        fields.insert(key.into(),json!(safe));
                    }
                    for key in ["page","page_size"] {fields.insert(key.into(),json!(value[key].as_u64()));}
                    (Request::from_parts(parts,Body::from(bytes)),Value::Object(fields))
                } else {(request,Value::Null)};
                let response=next.run(request).await;
                let (response,items)=if is_list && response.status().is_success() {
                    let (parts,body)=response.into_parts();
                    let bytes=to_bytes(body,1024*1024).await.unwrap();
                    let value=serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                    let items=value["data"]["list"].as_array().map(|rows|rows.iter().map(|row|json!({
                        "guid":row["guid"],"title":row["title"],"create_time":row["create_time"]
                    })).collect::<Vec<_>>());
                    (axum::response::Response::from_parts(parts,Body::from(bytes)),json!(items))
                } else {(response,Value::Null)};
                // 排序验收只额外记录白名单字段及合成条目；不保存原始请求体、Cookie 或鉴权头。
                if path=="/trimcon"||path.starts_with("/v/")||path.starts_with("/tgdrive-poster/") {
                    let row=json!({"at":crate::db::now_ts(),"method":method,"path":path,"status":response.status().as_u16(),"user_agent":agent,"sort":sort,"items":items});
                    writeln!(log.lock().unwrap(),"{row}").unwrap();
                }
                response
            }
        }));
    let startup = json!({"fixture":"tgdrive-fntv-native","synthetic":true,"telegram":"memory","host_version":env!("CARGO_PKG_VERSION"),"base_url":format!("http://{address}"),"pid":std::process::id(),"database":"temporary","preinstalled_fntv":false,"window_seconds":seconds,"covers_initially_paused":paused,"sorting_samples":sorting});
    std::fs::write(
        output.join("startup.json"),
        serde_json::to_vec_pretty(&startup).unwrap(),
    )
    .unwrap();
    println!("FNTV_NATIVE_FIXTURE_URL=http://{address}");
    println!("只使用已校验合成文件、临时 SQLite 和内存 Telegram；从应用中心安装插件。 ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            fixture.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(server_stop.cancelled_owned())
        .await
    });
    tokio::select! {
        _=stop.cancelled()=>{},
        _=tokio::time::sleep(std::time::Duration::from_secs(seconds))=>{},
    }
    state.vfs.db.fntv.invalidate();
    state.restore.shutdown.cancel();
    cover_gate.cancel();
    stop.cancel();
    // 管理页的长期事件连接不能让显式验收窗口无限等待。
    let abort = server.abort_handle();
    match tokio::time::timeout(std::time::Duration::from_secs(2), server).await {
        Ok(result) => {
            result.unwrap().unwrap();
        }
        Err(_) => abort.abort(),
    }
    std::fs::write(output.join("final-stats.json"), serde_json::to_vec_pretty(&json!({
        "fixture":"tgdrive-fntv-native","synthetic":true,"host_version":env!("CARGO_PKG_VERSION"),
        "protocol_stats":state.vfs.db.fntv.protocol_stats(),"active_readers":media.active.load(Ordering::SeqCst)
    })).unwrap()).unwrap();
}
