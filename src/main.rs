//! tgdrive — 以 Telegram 为存储后端的个人网盘
//! 子命令:serve(默认)| check(实测 Bot API 链路)| healthcheck(容器健康检查)

mod api;
mod apps;
mod auth;
mod backup;
mod config;
mod dav;
mod db;
mod engine;
mod error;
mod extras;
mod housekeeping;
mod inbox;
mod migration;
mod mtproto;
mod proxy;
mod security;
mod settings;
mod tasks;
mod telegram;
mod vfs;
mod web;

use crate::api::AppState;
use crate::auth::Auth;
use crate::config::Config;
use crate::db::Db;
use crate::engine::MEMORY_CHUNK_LIMIT;
use crate::engine::upload::{UploadKnobs, Uploader};
use crate::mtproto::MtprotoConfig;
use crate::settings::{Runtime, RuntimeCfg};
use crate::telegram::{TgApi, TgClient, TgRegistry};
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
    db.recover_storage_migration().await?;
    // 环境变量只是底层默认值,后台改过的配置(settings 表)优先
    let mut cfg = RuntimeCfg::load(&db, &env_cfg).await?;

    // 包一层可切换代理:后台改 token/频道/端点后无需重建 Vfs 与 DavHandler。
    // 尚未配置 Telegram 时装占位实现,服务照常起来,登录后在网页里配。
    if !cfg.is_configured() {
        tracing::warn!("尚未配置 Telegram 存储,请登录后打开「系统设置」填写 Bot Token 与存储频道");
    }
    if cfg.is_configured() {
        let mut upload_target_ids = Vec::new();
        for token in cfg.all_upload_bot_tokens() {
            let target = db
                .ensure_storage_target(&cfg.api_base, &token, &cfg.chat_id)
                .await?;
            upload_target_ids.push(target.id);
        }
        cfg.storage_target_id = upload_target_ids.first().copied();
        cfg.upload_target_ids = upload_target_ids;
        let primary_target_id = cfg
            .storage_target_id
            .ok_or_else(|| anyhow::anyhow!("Telegram 主存储目标未初始化"))?;
        let migrated = db.backfill_unassigned_chunks(primary_target_id).await?;
        if migrated > 0 {
            tracing::warn!(
                "已将 {migrated} 个旧版分块关联到当前 Telegram 存储目标；如数据库曾切换过 Bot/频道，请人工核对历史文件"
            );
        }
    }
    let targets = db.storage_targets().await?;
    let mtproto =
        MtprotoConfig::from_values(db.clone(), &cfg.telegram_api_id, &cfg.telegram_api_hash)?;
    if mtproto.is_none() {
        tracing::warn!(
            "未配置 api_id/api_hash：文档与视频下载将使用 Bot API，超大分块无法真正按 Range 流式读取"
        );
    }
    let tg = Arc::new(TgRegistry::new(
        &targets,
        cfg.storage_target_id,
        cfg.upload_target_ids.clone(),
        mtproto,
        cfg.proxy.clone(),
    )?);
    let knobs = Arc::new(UploadKnobs::new(
        cfg.chunk_size,
        cfg.tmp_dir.clone(),
        cfg.upload_concurrency
            .saturating_mul(cfg.upload_target_ids.len().max(1)),
        cfg.send_video,
    ));
    let uploader = Uploader::new(tg.clone(), db.clone(), knobs.clone(), MEMORY_CHUNK_LIMIT);

    // Telegram 消息清理队列的自动/手动开关存在 extras 里，worker 起来之前先同步给 knobs。
    match crate::extras::load(&db).await {
        Ok(ex) => {
            knobs.set_auto_delete_message(ex.auto_delete_message);
        }
        Err(error) => tracing::warn!("读取后台队列开关失败，按手动模式启动: {error}"),
    }

    // 进程退出会让在途分块声明失效；已完成分块仍由可恢复会话保留。
    db.reset_upload_part_claims().await?;
    // 旧上传管线的崩溃残留仍需回滚；带 upload_sessions 的节点不能删除。
    if db.unassigned_chunk_count().await? == 0 {
        let stale = db.cleanup_stale_uploads().await?;
        if !stale.is_empty() {
            tracing::info!("清理上次遗留的 {} 条未完成上传消息", stale.len());
            uploader.delete_messages_best_effort(stale);
        }
    } else {
        tracing::warn!("存在尚未关联存储目标的旧分块，已暂缓清理未完成上传");
    }
    uploader.process_pending_deletes();

    let vfs = Vfs {
        db: db.clone(),
        tg: tg.clone(),
        uploader,
    };
    let migration = crate::migration::StorageMigrationManager::new(db.clone(), tg.clone());
    let auth = Arc::new(
        Auth::init(
            &cfg.username,
            &cfg.password,
            env_cfg.secret.as_deref(),
            env_cfg.cookie_secure,
            &db,
        )
        .await?,
    );
    let listen = env_cfg.listen;
    let cookie_secure = env_cfg.cookie_secure;
    let (api_base, chunk_size, official) =
        (cfg.api_base.clone(), cfg.chunk_size, cfg.is_official_api());
    let rt = Arc::new(Runtime::new(
        db.clone(),
        env_cfg.clone(),
        cfg,
        tg,
        knobs,
        auth.clone(),
    ));
    let dav_handler = dav::build_handler(vfs.clone());
    let tasks = crate::tasks::TaskManager::new();
    let catalog_cache_dir = std::path::Path::new(&env_cfg.db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("app-catalog-cache");
    let apps_catalog = Arc::new(crate::apps::CatalogManager::from_env(
        catalog_cache_dir,
        rt.clone(),
    ));
    let state = AppState {
        vfs,
        auth,
        rt,
        dav: dav_handler,
        attempts: crate::security::AttemptLimiter::default(),
        tasks: tasks.clone(),
        migration: migration.clone(),
        apps_catalog,
    };
    if let Err(error) = db
        .append_backend_log("info", "系统", "tgdrive 服务已启动")
        .await
    {
        tracing::warn!("记录启动日志失败: {error}");
    }

    // 后台任务:回收站超期清理 + 自动备份;Bot 收件轮询
    let housekeeping_vfs = state.vfs.clone();
    let housekeeping_rt = state.rt.clone();
    tasks.supervise("housekeeping", move |reporter| {
        housekeeping::run(housekeeping_vfs.clone(), housekeeping_rt.clone(), reporter)
    });
    let inbox_vfs = state.vfs.clone();
    let inbox_rt = state.rt.clone();
    tasks.supervise("inbox", move |reporter| {
        inbox::run(inbox_vfs.clone(), inbox_rt.clone(), reporter)
    });

    let mut app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/readyz", axum::routing::get(api::readyz))
        .route(
            "/dav",
            axum::routing::any(dav::dav_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/dav/",
            axum::routing::any(dav::dav_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/dav/{*rest}",
            axum::routing::any(dav::dav_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        )
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
            app = app.fallback_service(web::router(&dir));
        }
        None => {
            tracing::warn!("未找到静态前端目录,仅提供 API(开发模式请用 vite dev server)");
        }
    }

    let app = app
        // JSON/表单默认最多 1MiB；只有文件上传与 WebDAV 写入在各自路由放开。
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            cookie_secure,
            crate::security::response_headers,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    tracing::info!(
        listen = %listen,
        api = %api_base,
        official = official,
        chunk = chunk_size,
        "tgdrive 启动"
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(tasks, migration))
    .await?;
    Ok(())
}

async fn shutdown_signal(
    tasks: crate::tasks::TaskManager,
    migration: crate::migration::StorageMigrationManager,
) {
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate => {},
    }
    tracing::info!("收到退出信号，正在停止后台任务并等待连接结束");
    migration.shutdown().await;
    tasks.cancel();
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
    let tg = TgClient::new(
        &cfg.api_base,
        &cfg.bot_token,
        &cfg.chat_id,
        cfg.proxy.clone(),
    )?;

    println!("[1/4] 验证 Bot 与存储频道权限 ...");
    let me = tg.probe_storage().await?;
    println!("      bot = @{me}, api = {}", cfg.api_base);

    println!("[2/4] 上传 1MB 测试文件 ...");
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let want = sha2::Sha256::digest(&data);
    let body = UploadBody::Memory(bytes::Bytes::from(data));
    tg.wait_upload_turn().await;
    let msg = tg
        .send_document(
            "tgdrive-selftest.bin",
            Some("tgdrive 自检文件,可删除".into()),
            None,
            &body,
        )
        .await?;
    println!("      message_id = {}", msg.message_id);

    println!("[3/4] 下载比对 ...");
    let mut reader = tg
        .open_chunk_stream(&msg.file_id, msg.message_id, 1024 * 1024, 0)
        .await?;
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
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "healthz 返回 {}", resp.status());
    println!("ok");
    Ok(())
}
