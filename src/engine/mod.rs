//! 分块引擎:把任意字节流切块上传到 Telegram,以及跨分块的流式读取

pub mod probe;
pub mod reader;
pub mod upload;

/// 分块不超过该值时驻内存,超过则经临时文件中转(内存占用与分块大小解耦)
pub const MEMORY_CHUNK_LIMIT: u64 = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::reader::ChunkedReader;
    use super::upload::Uploader;
    use crate::db::{Db, ROOT_ID};
    use crate::error::{AppError, AppResult};
    use crate::telegram::{ByteReader, TgApi, TgMessage, UploadBody, VideoMeta};
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    /// 内存版 Telegram:file_id → 数据;可注入第 N 次上传失败
    struct MockTg {
        store: Mutex<HashMap<String, Bytes>>,
        seq: AtomicI64,
        deleted: Mutex<Vec<i64>>,
        fail_on_send: Option<i64>,
        /// 每次成功发送的 (文件名, 是否视频消息)
        sent: Mutex<Vec<(String, bool)>>,
        /// 每次视频发送时带的元数据(宽高/时长)
        video_meta: Mutex<Vec<Option<VideoMeta>>>,
        /// 注入 sendVideo 失败,验证回退文档路径
        fail_video: AtomicBool,
        /// 注入结果不确定的视频发送错误，验证不会再次按文档发送
        uncertain_video: AtomicBool,
    }

    impl MockTg {
        fn new(fail_on_send: Option<i64>) -> Arc<Self> {
            Arc::new(MockTg {
                store: Mutex::new(HashMap::new()),
                seq: AtomicI64::new(0),
                deleted: Mutex::new(Vec::new()),
                fail_on_send,
                sent: Mutex::new(Vec::new()),
                video_meta: Mutex::new(Vec::new()),
                fail_video: AtomicBool::new(false),
                uncertain_video: AtomicBool::new(false),
            })
        }

        fn store_body(
            &self,
            filename: &str,
            body: &UploadBody,
            video: bool,
        ) -> AppResult<TgMessage> {
            let n = self.seq.fetch_add(1, Ordering::SeqCst);
            if self.fail_on_send == Some(n) {
                return Err(AppError::Telegram("mock 注入的失败".into()));
            }
            let data = match body {
                UploadBody::Memory(b) => b.clone(),
                UploadBody::TempFile { file, .. } => {
                    let mut v = Vec::new();
                    file.reopen().unwrap().read_to_end(&mut v).unwrap();
                    Bytes::from(v)
                }
            };
            let fid = format!("f{n}");
            self.store.lock().unwrap().insert(fid.clone(), data);
            self.sent.lock().unwrap().push((filename.to_string(), video));
            Ok(TgMessage { message_id: 1000 + n, file_id: fid })
        }
    }

    #[async_trait::async_trait]
    impl TgApi for MockTg {
        async fn send_document(
            &self,
            filename: &str,
            _caption: Option<String>,
            body: &UploadBody,
        ) -> AppResult<TgMessage> {
            self.store_body(filename, body, false)
        }

        async fn send_video(
            &self,
            filename: &str,
            _mime: &str,
            meta: Option<VideoMeta>,
            _caption: Option<String>,
            body: &UploadBody,
        ) -> AppResult<TgMessage> {
            if self.fail_video.load(Ordering::SeqCst) {
                return Err(AppError::TelegramRejected("mock: sendVideo 不可用".into()));
            }
            if self.uncertain_video.load(Ordering::SeqCst) {
                return Err(AppError::TelegramUncertain("mock: 响应丢失".into()));
            }
            self.video_meta.lock().unwrap().push(meta);
            self.store_body(filename, body, true)
        }

        async fn delete_message(&self, message_id: i64) -> AppResult<()> {
            self.deleted.lock().unwrap().push(message_id);
            Ok(())
        }

        async fn get_me(&self) -> AppResult<String> {
            Ok("mock".into())
        }

        async fn open_chunk_stream(&self, file_id: &str, offset: u64) -> AppResult<ByteReader> {
            let data = self
                .store
                .lock()
                .unwrap()
                .get(file_id)
                .cloned()
                .ok_or_else(|| AppError::Telegram(format!("mock: {file_id} 不存在")))?;
            Ok(Box::pin(std::io::Cursor::new(data.slice(offset as usize..))))
        }
    }

    async fn fixture(
        chunk_size: u64,
        mem_limit: u64,
        fail_on_send: Option<i64>,
    ) -> (Uploader, Arc<MockTg>, Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).await.unwrap();
        let tg = MockTg::new(fail_on_send);
        let up = Uploader {
            tg: tg.clone(),
            db: db.clone(),
            knobs: Arc::new(super::upload::UploadKnobs::new(
                chunk_size,
                dir.path().to_path_buf(),
                1, // 测试用串行,保证 seq 顺序确定
                true, // 视频消息模式默认开,便于测视频路径;普通扩展名不受影响
            )),
            mem_limit,
        };
        (up, tg, db, dir)
    }

    async fn feed_all(up: &Uploader, name: &str, data: &[u8], piece: usize) -> AppResult<crate::db::Node> {
        let h = up.start(ROOT_ID, name, &format!("/{name}")).await?;
        for chunk in data.chunks(piece.max(1)) {
            h.feed(Bytes::copy_from_slice(chunk)).await?;
        }
        h.finish().await
    }

    #[tokio::test]
    async fn roundtrip_multi_chunk() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        let node = feed_all(&up, "a.bin", &data, 3).await.unwrap();
        assert_eq!(node.size, 10);

        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(chunks.iter().map(|c| c.size).collect::<Vec<_>>(), vec![4, 4, 2]);

        // 顺序读全量
        let mut r = ChunkedReader::new(tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);

        // seek 到跨块位置读尾部
        let mut r = ChunkedReader::new(tg.clone(), &chunks, 0);
        r.seek(std::io::SeekFrom::Start(5)).await.unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, &data[5..]);

        // 回退 seek
        r.seek(std::io::SeekFrom::Start(2)).await.unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, &data[2..]);

        // 直接从偏移构造(REST Range 场景)
        let mut r = ChunkedReader::new(tg.clone(), &chunks, 9);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, &data[9..]);
    }

    #[tokio::test]
    async fn spill_to_tempfile_roundtrip() {
        // mem_limit=8 强制走临时文件路径:chunk 20 > 8
        let (up, tg, db, _g) = fixture(20, 8, None).await;
        let data: Vec<u8> = (0..50u8).collect();
        let node = feed_all(&up, "big.bin", &data, 7).await.unwrap();
        assert_eq!(node.size, 50);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(chunks.iter().map(|c| c.size).collect::<Vec<_>>(), vec![20, 20, 10]);

        let mut r = ChunkedReader::new(tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn empty_file() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let node = feed_all(&up, "empty", &[], 1).await.unwrap();
        assert_eq!(node.size, 0);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert!(chunks.is_empty());
        let mut r = ChunkedReader::new(tg, &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn send_failure_rolls_back() {
        let (up, tg, db, _g) = fixture(4, 1024, Some(1)).await;
        let data: Vec<u8> = (0..10u8).collect();
        let err = feed_all(&up, "x.bin", &data, 10).await;
        assert!(err.is_err());
        // 节点被回滚删除
        assert!(db.resolve(&["x.bin"]).await.unwrap().is_none());
        // 已成功上传的分块被清理;失败的分块(seq 1)从未产生消息
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let deleted = tg.deleted.lock().unwrap().clone();
        assert!(deleted.contains(&1000), "分块 0 的消息应被清理: {deleted:?}");
        assert!(!deleted.contains(&1001), "失败分块不应有删除记录: {deleted:?}");
    }

    #[tokio::test]
    async fn producer_drop_aborts() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let h = up.start(ROOT_ID, "drop.bin", "/drop.bin").await.unwrap();
        h.feed(Bytes::from_static(&[1, 2, 3, 4, 5])).await.unwrap(); // 分块 0 已满并上传
        let join = h.join;
        drop(h.tx); // 模拟客户端断开:未发 Eof
        let res = join.await.unwrap();
        assert!(res.is_err());
        assert!(db.resolve(&["drop.bin"]).await.unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*tg.deleted.lock().unwrap(), vec![1000]);
    }

    #[test]
    fn video_mime_by_extension() {
        use super::upload::video_mime;
        assert_eq!(video_mime("a.MP4"), Some("video/mp4"));
        assert_eq!(video_mime("b.m4v"), Some("video/x-m4v"));
        assert_eq!(video_mime("c.mov"), Some("video/quicktime"));
        // mkv/webm 客户端播放器不保证有画面,仍走文档
        assert_eq!(video_mime("电影.mkv"), None);
        assert_eq!(video_mime("b.webm"), None);
        assert_eq!(video_mime("d.bin"), None);
        assert_eq!(video_mime("无扩展名"), None);
        assert_eq!(video_mime("x.mp4.part1"), None); // 后续分块不是完整视频
    }

    #[tokio::test]
    async fn single_chunk_video_uses_send_video() {
        let (up, tg, db, _g) = fixture(100, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        let node = feed_all(&up, "movie.mp4", &data, 3).await.unwrap();
        assert_eq!(node.size, 10);
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mp4".to_string(), true)]);

        // 读回的字节与上传一致
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut r = ChunkedReader::new(tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn mkv_stays_document() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        feed_all(&up, "movie.mkv", &[1, 2, 3], 3).await.unwrap();
        // mkv 即使开着视频消息模式也按文档发送
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mkv".to_string(), false)]);
    }

    #[tokio::test]
    async fn multi_chunk_video_stays_document() {
        let (up, tg, _db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        feed_all(&up, "movie.mp4", &data, 3).await.unwrap();
        // 3 个分块全部按文档发送(分块不是完整视频,发视频消息没有意义)
        let sent = tg.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|(_, video)| !video), "分块不应按视频发送: {sent:?}");
    }

    #[tokio::test]
    async fn video_mode_off_uses_document() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        up.knobs.set_send_video(false);
        feed_all(&up, "movie.mp4", &[1, 2, 3], 3).await.unwrap();
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mp4".to_string(), false)]);
    }

    #[tokio::test]
    async fn video_send_failure_falls_back_to_document() {
        let (up, tg, db, _g) = fixture(100, 1024, None).await;
        tg.fail_video.store(true, Ordering::SeqCst);
        let data = [9u8, 8, 7];
        let node = feed_all(&up, "movie.mp4", &data, 3).await.unwrap();
        // sendVideo 失败后回退文档,上传仍成功
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mp4".to_string(), false)]);
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut r = ChunkedReader::new(tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn uncertain_video_send_does_not_fall_back() {
        let (up, tg, db, _g) = fixture(100, 1024, None).await;
        tg.uncertain_video.store(true, Ordering::SeqCst);
        let err = feed_all(&up, "movie.mp4", &[9, 8, 7], 3).await.unwrap_err();
        assert!(matches!(err, AppError::TelegramUncertain(_)));
        assert!(tg.sent.lock().unwrap().is_empty(), "结果不确定时不得再按文档发送");
        assert!(db.resolve(&["movie.mp4"]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn video_upload_carries_probed_dimensions() {
        let (up, tg, _db, _g) = fixture(64 * 1024, 1024, None).await;
        // 合成 mp4(1920x1080、60 秒)走完整上传管线,宽高应传到 sendVideo
        let data = crate::engine::probe::testutil::mp4_file(1920, 1080, false);
        feed_all(&up, "movie.mp4", &data, 128).await.unwrap();
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mp4".to_string(), true)]);
        assert_eq!(
            *tg.video_meta.lock().unwrap(),
            vec![Some(VideoMeta { width: 1920, height: 1080, duration_secs: Some(60) })]
        );
    }

    #[tokio::test]
    async fn unparsable_video_sends_without_meta() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        // 扩展名是视频但内容解析不了 → 仍按视频发送,只是不带宽高
        feed_all(&up, "movie.mp4", &[1, 2, 3], 3).await.unwrap();
        assert_eq!(*tg.sent.lock().unwrap(), vec![("movie.mp4".to_string(), true)]);
        assert_eq!(*tg.video_meta.lock().unwrap(), vec![None]);
    }
}
