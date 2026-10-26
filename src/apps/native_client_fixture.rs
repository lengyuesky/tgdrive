//! 显式原生 HTTP 夹具：可接收上传到内存，绝不调用真实 Telegram。
use super::*;

struct NativeMemoryMedia {
    inner: Arc<MemoryMedia>,
    next_message: AtomicUsize,
}
#[async_trait::async_trait]
impl TgApi for NativeMemoryMedia {
    async fn send_document(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<&bytes::Bytes>,
        body: &UploadBody,
    ) -> AppResult<TgMessage> {
        let bytes = match body {
            UploadBody::Memory(bytes) => bytes.to_vec(),
            UploadBody::TempFile { file, .. } => tokio::fs::read(file.path()).await?,
        };
        let id = self.next_message.fetch_add(1, Ordering::SeqCst);
        let file_id = format!("native-{id}");
        self.inner
            .files
            .lock()
            .unwrap()
            .insert(file_id.clone(), bytes);
        Ok(TgMessage {
            message_id: id as i64,
            file_id,
            thumbnail_file_id: None,
        })
    }
    async fn delete_message(&self, _: i64) -> AppResult<()> {
        Ok(())
    }
    async fn get_me(&self) -> AppResult<String> {
        Ok("原生隔离内存媒体".into())
    }
    async fn open_chunk_stream(
        &self,
        id: &str,
        message: i64,
        size: u64,
        offset: u64,
    ) -> AppResult<ByteReader> {
        self.inner
            .open_chunk_stream(id, message, size, offset)
            .await
    }
    async fn open_chunk_range(
        &self,
        id: &str,
        message: i64,
        size: u64,
        offset: u64,
        length: u64,
    ) -> AppResult<ByteReader> {
        self.inner
            .open_chunk_range(id, message, size, offset, length)
            .await
    }
}

#[tokio::test]
#[ignore = "需明确运行，提供 120 秒原生客户端 HTTP 联调窗口"]
async fn native_client_fixture() {
    let directory = tempfile::tempdir().unwrap();
    let media = Arc::new(MemoryMedia::default());
    let mut state = state_at(&directory.path().join("native.db"), media.clone()).await;
    let target = state.vfs.db.storage_targets().await.unwrap()[0].id;
    let api = Arc::new(NativeMemoryMedia {
        inner: media.clone(),
        next_message: AtomicUsize::new(10000),
    });
    let tg = Arc::new(TgRegistry::with_client(target, api));
    let knobs = Arc::new(UploadKnobs::new(
        1024 * 1024,
        directory.path().to_path_buf(),
        1,
        false,
    ));
    state
        .vfs
        .db
        .setting_set("cfg.chunk_size", "1048576")
        .await
        .unwrap();
    let cfg = RuntimeCfg::load(&state.vfs.db, &state.rt.env)
        .await
        .unwrap();
    let auth = state.auth.clone();
    state.rt = Arc::new(Runtime::new(
        state.vfs.db.clone(),
        state.rt.env.clone(),
        cfg,
        tg.clone(),
        knobs.clone(),
        auth,
    ));
    state.vfs.tg = tg.clone();
    state.vfs.uploader = Uploader::new(tg, state.vfs.db.clone(), knobs, 1024 * 1024);
    state.vfs.mkdir("/文档").await.unwrap();
    seed_file(
        &state,
        &media,
        "/文档/联调.txt",
        "原生 HTTP 夹具：不是生产数据。".as_bytes(),
        16,
    )
    .await;
    add_app(&state, &manifest(), None).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(&state).route(
        "/__native_fixture",
        axum::routing::get(|| async { Json(json!({"fixture":"tgdrive-native-tests"})) }),
    );
    println!("NATIVE_FIXTURE_URL=http://{address}");
    println!("仅使用临时 SQLite 和内存上传存储，绝不连接生产或 Telegram。");
    use std::io::Write as _;
    std::io::stdout().flush().unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async { tokio::time::sleep(std::time::Duration::from_secs(120)).await })
    .await
    .unwrap();
}
