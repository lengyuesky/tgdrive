//! tgdrive — 以 Telegram 为存储后端的个人网盘
//! 子命令:serve(默认)| check(实测 Bot API 链路)| healthcheck(容器健康检查)

mod api;
mod auth;
mod config;
mod dav;
mod db;
mod engine;
mod error;
mod extras;
mod housekeeping;
mod inbox;
mod settings;
mod telegram;
mod vfs;

use crate::api::AppState;
use crate::auth::Auth;
use crate::config::Config;
use crate::db::Db;
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::engine::MEMORY_CHUNK_LIMIT;
use crate::settings::{Runtime, RuntimeCfg};
use crate::telegram::{SwitchableTg, TgApi, TgClient};
use crate::vfs::Vfs;
use std::sync::Arc;

fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let rt = tokio::runtime::Runtime::new().expect("无法创建 tokio 运行时");
    let result = match cmd.as_str() {
        "serve" => rt.block_on(serve()),
        "check" => rt.block_on(check()),
        "healthcheck" => rt.block_on(healthcheck()),
        other => {
            eprintln!("未知子命令: {other}\n用法: tgdrive [serve|check|healthcheck]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

async fn serve() -> anyhow::Result<()> {
    let env_cfg = Arc::new(Config::from_env()?);
    let db = Db::open(&env_cfg.db_path).await?;
    // 环境变量只是底层默认值,后台改过的配置(settings 表)优先
    let cfg = RuntimeCfg::load(&db, &env_cfg).await?;

    // 包一层可切换代理:后台改 token/频道/端点后无需重建 Vfs 与 DavHandler。
    // 尚未配置 Telegram 时装占位实现,服务照常起来,登录后在网页里配。
    if !cfg.is_configured() {
        tracing::warn!("尚未配置 Telegram 存储,请登录后打开「系统设置」填写 Bot Token 与存储频道");
    }
    let switch = Arc::new(SwitchableTg::new(cfg.make_tg()?));
    let tg: Arc<dyn TgApi> = switch.clone();
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        cfg.tmp_dir.clone(),
        cfg.upload_concurrency,
        cfg.send_video,
    ));
    let uploader = Uploader {
        tg: tg.clone(),
        db: db.clone(),
        knobs: knobs.clone(),
        mem_limit: MEMORY_CHUNK_LIMIT,
    };

    // 崩溃残留的 uploading 节点:回滚记录并尽力删除已发出的分块消息
    let stale = db.cleanup_stale_uploads().await?;
    if !stale.is_empty() {
        tracing::info!("清理上次遗留的 {} 条未完成上传消息", stale.len());
        uploader.delete_messages_best_effort(stale);
    }

    let vfs = Vfs { db: db.clone(), tg, uploader };
    let auth = Arc::new(
        Auth::init(&cfg.username, &cfg.password, env_cfg.secret.as_deref(), &db).await?,
    );
    let listen = env_cfg.listen;
    let (api_base, chunk_size, official) =
        (cfg.api_base.clone(), cfg.chunk_size, cfg.is_official_api());
    let rt = Arc::new(Runtime::new(
        db.clone(),
        env_cfg,
        cfg,
        switch,
        knobs,
        auth.clone(),
    ));
    let dav_handler = dav::build_handler(vfs.clone());
    let state = AppState { vfs, auth, rt, dav: dav_handler };

    // 后台任务:回收站超期清理 + 自动备份;Bot 收件轮询
    tokio::spawn(housekeeping::run(state.vfs.clone(), state.rt.clone()));
    tokio::spawn(inbox::run(state.vfs.clone(), state.rt.clone()));

    let mut app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/dav", axum::routing::any(dav::dav_handler))
        .route("/dav/", axum::routing::any(dav::dav_handler))
        .route("/dav/{*rest}", axum::routing::any(dav::dav_handler))
        .with_state(state.clone())
        .nest("/api", api::router(state.clone()));

    // 前端静态文件(SPA fallback 到 index.html);候选目录都不存在则只提供 API
    let static_dir = std::env::var("TGDRIVE_STATIC_DIR")
        .ok()
        .into_iter()
        .chain(["static".to_string(), "frontend/dist".to_string()])
        .find(|d| std::path::Path::new(d).is_dir());
    match static_dir {
        Some(dir) => {
            tracing::info!("静态前端目录: {dir}");
            let index = std::path::Path::new(&dir).join("index.html");
            let serve = tower_http::services::ServeDir::new(&dir)
                .not_found_service(tower_http::services::ServeFile::new(index));
            app = app.fallback_service(serve);
        }
        None => {
            tracing::warn!("未找到静态前端目录,仅提供 API(开发模式请用 vite dev server)");
        }
    }

    let app = app
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    tracing::info!(
        listen = %listen,
        api = %api_base,
        official = official,
        chunk = chunk_size,
        "tgdrive 启动"
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// 用真实 bot 验证配置与整条链路(getMe → 上传测试文件 → 下载比对 → 删除)
async fn check() -> anyhow::Result<()> {
    use crate::telegram::UploadBody;
    use sha2::Digest;
    use tokio::io::AsyncReadExt;

    // 与 serve 用同一份生效配置(后台改过的以 settings 表为准)
    let env_cfg = Config::from_env()?;
    let db = Db::open(&env_cfg.db_path).await?;
    let cfg = RuntimeCfg::load(&db, &env_cfg).await?;
    anyhow::ensure!(
        cfg.is_configured(),
        "尚未配置 Telegram 存储:请在 .env 里填 TGDRIVE_BOT_TOKEN / TGDRIVE_CHAT_ID,\
         或启动服务后在网页「系统设置」里配置"
    );
    let tg = TgClient::new(&cfg.api_base, &cfg.bot_token, &cfg.chat_id)?;

    println!("[1/4] getMe ...");
    let me = tg.get_me().await?;
    println!("      bot = @{me}, api = {}", cfg.api_base);

    println!("[2/4] 上传 1MB 测试文件 ...");
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let want = sha2::Sha256::digest(&data);
    let body = UploadBody::Memory(bytes::Bytes::from(data));
    let msg = tg
        .send_document("tgdrive-selftest.bin", Some("tgdrive 自检文件,可删除".into()), &body)
        .await?;
    println!("      message_id = {}", msg.message_id);

    println!("[3/4] 下载比对 ...");
    let mut reader = tg.open_chunk_stream(&msg.file_id, 0).await?;
    let mut got = Vec::new();
    reader.read_to_end(&mut got).await?;
    let got_hash = sha2::Sha256::digest(&got);
    anyhow::ensure!(got.len() == 1024 * 1024, "下载大小不符: {}", got.len());
    anyhow::ensure!(want == got_hash, "内容哈希不一致");

    println!("[4/4] 清理测试消息 ...");
    tg.delete_message(msg.message_id).await?;
    println!("✔ 链路自检通过(上传/下载/删除均正常)");
    Ok(())
}

/// 容器 HEALTHCHECK:只依赖 TGDRIVE_LISTEN,不需要完整配置
async fn healthcheck() -> anyhow::Result<()> {
    let listen = std::env::var("TGDRIVE_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = listen.rsplit(':').next().unwrap_or("8080");
    let url = format!("http://127.0.0.1:{port}/healthz");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "healthz 返回 {}", resp.status());
    println!("ok");
    Ok(())
}
