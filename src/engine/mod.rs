//! 分块引擎:把任意字节流切块上传到 Telegram,以及跨分块的流式读取

pub mod probe;
pub mod reader;
pub mod upload;

/// 分块不超过该值时驻内存,超过则经临时文件中转(内存占用与分块大小解耦)
pub const MEMORY_CHUNK_LIMIT: u64 = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::reader::ChunkedReader;
    use super::upload::{ResumablePartStart, Uploader};
    use crate::db::{Db, ROOT_ID};
    use crate::error::{AppError, AppResult};
    use crate::telegram::{ByteReader, TgApi, TgMessage, TgRegistry, UploadBody, VideoMeta};
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
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
        /// 每次成功发送携带的 caption；普通文件上传必须始终为 None。
        sent_captions: Mutex<Vec<Option<String>>>,
        /// 每次发送是否携带自定义缩略图。
        sent_thumbnail: Mutex<Vec<bool>>,
        /// 每次视频发送时带的元数据(宽高/时长)
        video_meta: Mutex<Vec<Option<VideoMeta>>>,
        /// 注入 sendVideo 失败,验证回退文档路径
        fail_video: AtomicBool,
        /// 注入结果不确定的视频发送错误，验证不会再次按文档发送
        uncertain_video: AtomicBool,
        /// 注入 Telegram 对历史消息的永久删除拒绝。
        fail_delete: AtomicBool,
        /// 挂起 Telegram 发送，用于验证取消会真正终止在途任务。
        block_send: AtomicBool,
        send_started: AtomicBool,
        send_started_count: AtomicU64,
        send_release: tokio::sync::Notify,
        /// 挂起共享发送时隙，用于验证等待限流时取消不会误重启 Bot API。
        block_upload_turn: AtomicBool,
        upload_turn_started: AtomicBool,
        upload_turn_release: tokio::sync::Notify,
        /// 验证取消确实通知了 Bot API 监督器。
        cancel_pending_calls: AtomicU64,
        /// 记录底层实际收到的块内范围，验证 HTTP 终点能跨分块传递。
        read_ranges: Mutex<Vec<(u64, u64)>>,
    }

    impl MockTg {
        fn new(fail_on_send: Option<i64>) -> Arc<Self> {
            Arc::new(MockTg {
                store: Mutex::new(HashMap::new()),
                seq: AtomicI64::new(0),
                deleted: Mutex::new(Vec::new()),
                fail_on_send,
                sent: Mutex::new(Vec::new()),
                sent_captions: Mutex::new(Vec::new()),
                sent_thumbnail: Mutex::new(Vec::new()),
                video_meta: Mutex::new(Vec::new()),
                fail_video: AtomicBool::new(false),
                uncertain_video: AtomicBool::new(false),
                fail_delete: AtomicBool::new(false),
                block_send: AtomicBool::new(false),
                send_started: AtomicBool::new(false),
                send_started_count: AtomicU64::new(0),
                send_release: tokio::sync::Notify::new(),
                block_upload_turn: AtomicBool::new(false),
                upload_turn_started: AtomicBool::new(false),
                upload_turn_release: tokio::sync::Notify::new(),
                cancel_pending_calls: AtomicU64::new(0),
                read_ranges: Mutex::new(Vec::new()),
            })
        }

        async fn wait_if_blocked(&self) {
            if self.block_send.load(Ordering::SeqCst) {
                self.send_started.store(true, Ordering::SeqCst);
                self.send_started_count.fetch_add(1, Ordering::SeqCst);
                self.send_release.notified().await;
            }
        }

        fn store_body(
            &self,
            filename: &str,
            body: &UploadBody,
            video: bool,
            thumbnail: Option<&Bytes>,
            caption: Option<String>,
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
            let thumbnail_file_id = thumbnail.map(|image| {
                let file_id = format!("thumb-{n}");
                self.store
                    .lock()
                    .unwrap()
                    .insert(file_id.clone(), image.clone());
                file_id
            });
            self.store.lock().unwrap().insert(fid.clone(), data);
            self.sent
                .lock()
                .unwrap()
                .push((filename.to_string(), video));
            self.sent_captions.lock().unwrap().push(caption);
            self.sent_thumbnail
                .lock()
                .unwrap()
                .push(thumbnail.is_some());
            Ok(TgMessage {
                message_id: 1000 + n,
                file_id: fid,
                thumbnail_file_id,
            })
        }
    }

    #[async_trait::async_trait]
    impl TgApi for MockTg {
        async fn wait_upload_turn(&self) {
            if self.block_upload_turn.load(Ordering::SeqCst) {
                self.upload_turn_started.store(true, Ordering::SeqCst);
                self.upload_turn_release.notified().await;
            }
        }

        async fn send_document(
            &self,
            filename: &str,
            caption: Option<String>,
            thumbnail: Option<&Bytes>,
            body: &UploadBody,
        ) -> AppResult<TgMessage> {
            self.wait_if_blocked().await;
            self.store_body(filename, body, false, thumbnail, caption)
        }

        async fn send_video(
            &self,
            filename: &str,
            _mime: &str,
            meta: Option<VideoMeta>,
            caption: Option<String>,
            thumbnail: Option<&Bytes>,
            body: &UploadBody,
        ) -> AppResult<TgMessage> {
            if self.fail_video.load(Ordering::SeqCst) {
                return Err(AppError::TelegramRejected("mock: sendVideo 不可用".into()));
            }
            if self.uncertain_video.load(Ordering::SeqCst) {
                return Err(AppError::TelegramUncertain("mock: 响应丢失".into()));
            }
            self.wait_if_blocked().await;
            self.video_meta.lock().unwrap().push(meta);
            self.store_body(filename, body, true, thumbnail, caption)
        }

        async fn delete_message(&self, message_id: i64) -> AppResult<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(AppError::Telegram(
                    "[400] Bad Request: message can't be deleted".into(),
                ));
            }
            self.deleted.lock().unwrap().push(message_id);
            Ok(())
        }

        async fn cancel_pending_uploads(&self) -> AppResult<()> {
            self.cancel_pending_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn get_me(&self) -> AppResult<String> {
            Ok("mock".into())
        }

        async fn open_chunk_stream(
            &self,
            file_id: &str,
            _message_id: i64,
            _chunk_size: u64,
            offset: u64,
        ) -> AppResult<ByteReader> {
            let data = self
                .store
                .lock()
                .unwrap()
                .get(file_id)
                .cloned()
                .ok_or_else(|| AppError::Telegram(format!("mock: {file_id} 不存在")))?;
            Ok(Box::pin(std::io::Cursor::new(
                data.slice(offset as usize..),
            )))
        }

        async fn open_chunk_range(
            &self,
            file_id: &str,
            message_id: i64,
            chunk_size: u64,
            offset: u64,
            length: u64,
        ) -> AppResult<ByteReader> {
            self.read_ranges.lock().unwrap().push((offset, length));
            let reader = self
                .open_chunk_stream(file_id, message_id, chunk_size, offset)
                .await?;
            Ok(Box::pin(reader.take(length)))
        }
    }

    async fn fixture(
        chunk_size: u64,
        mem_limit: u64,
        fail_on_send: Option<i64>,
    ) -> (Uploader, Arc<MockTg>, Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let tg = MockTg::new(fail_on_send);
        let target = db
            .ensure_storage_target("http://mock", "mock-token", "-1001")
            .await
            .unwrap();
        let registry = Arc::new(TgRegistry::with_client(target.id, tg.clone()));
        let knobs = Arc::new(super::upload::UploadKnobs::new(
            chunk_size,
            dir.path().to_path_buf(),
            1,    // 测试用串行,保证 seq 顺序确定
            true, // 视频消息模式默认开,便于测视频路径;普通扩展名不受影响
        ));
        // 生产默认手动消费队列,但这里要验的是上传失败后的补偿删除本身,
        // 所以显式打开自动模式,让 worker 真的跑起来。
        knobs.set_auto_delete_message(true);
        let up = Uploader::new(registry, db.clone(), knobs, mem_limit);
        (up, tg, db, dir)
    }

    async fn feed_all(
        up: &Uploader,
        name: &str,
        data: &[u8],
        piece: usize,
    ) -> AppResult<crate::db::Node> {
        let h = up.start(ROOT_ID, name, &format!("/{name}"), None).await?;
        for chunk in data.chunks(piece.max(1)) {
            h.feed(Bytes::copy_from_slice(chunk)).await?;
        }
        h.finish().await
    }

    async fn feed_all_with_thumbnail(
        up: &Uploader,
        name: &str,
        data: &[u8],
        piece: usize,
        thumbnail: Bytes,
    ) -> AppResult<crate::db::Node> {
        let h = up
            .start(ROOT_ID, name, &format!("/{name}"), Some(thumbnail))
            .await?;
        for chunk in data.chunks(piece.max(1)) {
            h.feed(Bytes::copy_from_slice(chunk)).await?;
        }
        h.finish().await
    }

    #[tokio::test]
    async fn roundtrip_multi_chunk() {
        let (up, _tg, db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        let node = feed_all(&up, "a.bin", &data, 3).await.unwrap();
        assert_eq!(node.size, 10);
        assert_eq!(*_tg.sent_captions.lock().unwrap(), vec![None, None, None]);

        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(
            chunks.iter().map(|c| c.size).collect::<Vec<_>>(),
            vec![4, 4, 2]
        );

        // 顺序读全量
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
        assert_eq!(r.read(&mut []).await.unwrap(), 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);

        // seek 到跨块位置读尾部
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
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
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 9);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, &data[9..]);
    }

    #[tokio::test]
    async fn limited_reader_passes_range_boundaries_to_each_chunk() {
        let (up, tg, db, _guard) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10).collect();
        let node = feed_all(&up, "range.bin", &data, 3).await.unwrap();
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut reader = ChunkedReader::new(up.tg.clone(), &chunks, 2).with_read_limit(5);
        assert_eq!(reader.total_size(), 10);
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, data[2..7]);
        assert_eq!(*tg.read_ranges.lock().unwrap(), vec![(2, 2), (0, 3)]);
    }

    #[tokio::test]
    async fn resumable_parts_survive_an_interrupted_request() {
        let (up, _tg, db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        let target = db.storage_targets().await.unwrap()[0].id;
        let session = db
            .create_upload_session(
                ROOT_ID,
                "resume.bin",
                "/resume.bin",
                data.len() as i64,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();

        let ResumablePartStart::Upload(first) = up
            .start_resumable_part(&session.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("第一个分块不应提前完成");
        };
        first
            .feed(Bytes::copy_from_slice(&data[..4]))
            .await
            .unwrap();
        first.finish().await.unwrap();

        let ResumablePartStart::Upload(interrupted) = up
            .start_resumable_part(&session.session.id, 1)
            .await
            .unwrap()
        else {
            panic!("第二个分块不应提前完成");
        };
        interrupted
            .feed(Bytes::copy_from_slice(&data[4..5]))
            .await
            .unwrap();
        drop(interrupted);
        tokio::task::yield_now().await;
        db.reset_upload_part_claims().await.unwrap();

        for (idx, slice) in [(1, &data[4..8]), (2, &data[8..])] {
            let ResumablePartStart::Upload(handle) = up
                .start_resumable_part(&session.session.id, idx)
                .await
                .unwrap()
            else {
                panic!("缺失分块不应提前完成");
            };
            handle.feed(Bytes::copy_from_slice(slice)).await.unwrap();
            handle.finish().await.unwrap();
        }

        let node = db
            .complete_upload_session(&session.session.id)
            .await
            .unwrap();
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut reader = ChunkedReader::new(up.tg.clone(), &chunks, 0);
        let mut restored = Vec::new();
        reader.read_to_end(&mut restored).await.unwrap();
        assert_eq!(restored, data);
    }

    #[tokio::test]
    async fn spill_to_tempfile_roundtrip() {
        // mem_limit=8 强制走临时文件路径:chunk 20 > 8
        let (up, _tg, db, _g) = fixture(20, 8, None).await;
        let data: Vec<u8> = (0..50u8).collect();
        let node = feed_all(&up, "big.bin", &data, 7).await.unwrap();
        assert_eq!(node.size, 50);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(
            chunks.iter().map(|c| c.size).collect::<Vec<_>>(),
            vec![20, 20, 10]
        );

        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn empty_file() {
        let (up, _tg, db, _g) = fixture(4, 1024, None).await;
        let node = feed_all(&up, "empty", &[], 1).await.unwrap();
        assert_eq!(node.size, 0);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert!(chunks.is_empty());
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
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
        assert!(
            deleted.contains(&1000),
            "分块 0 的消息应被清理: {deleted:?}"
        );
        assert!(
            !deleted.contains(&1001),
            "失败分块不应有删除记录: {deleted:?}"
        );
    }

    #[tokio::test]
    async fn producer_drop_aborts() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let h = up
            .start(ROOT_ID, "drop.bin", "/drop.bin", None)
            .await
            .unwrap();
        h.feed(Bytes::from_static(&[1, 2, 3, 4, 5])).await.unwrap(); // 分块 0 已满并上传
        for _ in 0..50 {
            if !tg.sent.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(h); // 模拟客户端断开:未发 Eof
        for _ in 0..50 {
            if db.resolve(&["drop.bin"]).await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(db.resolve(&["drop.bin"]).await.unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*tg.deleted.lock().unwrap(), vec![1000]);
        assert_eq!(
            tg.cancel_pending_calls.load(Ordering::SeqCst),
            0,
            "已经明确完成的 Telegram 请求不应触发 Bot API 重启"
        );
    }

    #[tokio::test]
    async fn dropping_upload_stops_inflight_bot_api_send() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        tg.block_send.store(true, Ordering::SeqCst);
        let handle = up
            .start(ROOT_ID, "cancel-stream.bin", "/cancel-stream.bin", None)
            .await
            .unwrap();
        handle
            .feed(Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap();
        for _ in 0..100 {
            if tg.send_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(tg.send_started.load(Ordering::SeqCst));

        drop(handle);
        for _ in 0..100 {
            if tg.cancel_pending_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 1);
        assert!(db.resolve(&["cancel-stream.bin"]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_before_bot_api_request_does_not_restart_it() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let target = db.storage_targets().await.unwrap()[0].id;
        let session = db
            .create_upload_session(
                ROOT_ID,
                "cancel-early.bin",
                "/cancel-early.bin",
                4,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        let ResumablePartStart::Upload(handle) = up
            .start_resumable_part(&session.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("分块不应提前完成");
        };

        drop(handle);
        up.begin_cancel_resumable_session(&session.session.id).await;
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 0);
        db.cancel_upload_session(&session.session.id).await.unwrap();
        up.finish_cancel_resumable_session(&session.session.id);
    }

    #[tokio::test]
    async fn cancelling_during_shared_rate_limit_wait_does_not_restart_bot_api() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let target = db.storage_targets().await.unwrap()[0].id;
        let session = db
            .create_upload_session(
                ROOT_ID,
                "cancel-rate-wait.bin",
                "/cancel-rate-wait.bin",
                4,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        tg.block_upload_turn.store(true, Ordering::SeqCst);

        let ResumablePartStart::Upload(handle) = up
            .start_resumable_part(&session.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("分块不应提前完成");
        };
        handle
            .feed(Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap();
        let request = tokio::spawn(handle.finish());
        for _ in 0..100 {
            if tg.upload_turn_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(tg.upload_turn_started.load(Ordering::SeqCst));

        request.abort();
        let _ = request.await;
        up.begin_cancel_resumable_session(&session.session.id).await;
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 0);
        assert!(tg.sent.lock().unwrap().is_empty());
        db.cancel_upload_session(&session.session.id).await.unwrap();
        up.finish_cancel_resumable_session(&session.session.id);
    }

    #[tokio::test]
    async fn dropping_part_request_stops_inflight_telegram_send() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let target = db.storage_targets().await.unwrap()[0].id;
        let session = db
            .create_upload_session(
                ROOT_ID,
                "cancel.bin",
                "/cancel.bin",
                4,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        tg.block_send.store(true, Ordering::SeqCst);

        let ResumablePartStart::Upload(handle) = up
            .start_resumable_part(&session.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("分块不应提前完成");
        };
        handle
            .feed(Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap();
        let request = tokio::spawn(handle.finish());
        for _ in 0..100 {
            if tg.send_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(tg.send_started.load(Ordering::SeqCst));

        // 模拟浏览器 AbortController 中止 PUT：处理请求的 future 被丢弃。
        request.abort();
        let _ = request.await;
        let mut claim_released = false;
        for _ in 0..100 {
            let retry = up.start_resumable_part(&session.session.id, 0).await;
            if let Ok(ResumablePartStart::Upload(retry_handle)) = retry {
                drop(retry_handle);
                claim_released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(claim_released, "取消后应及时释放分块占用");
        assert!(tg.sent.lock().unwrap().is_empty());
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 1);
        up.begin_cancel_resumable_session(&session.session.id).await;
        db.cancel_upload_session(&session.session.id).await.unwrap();
        up.finish_cancel_resumable_session(&session.session.id);
    }

    #[tokio::test]
    async fn cancelling_session_stops_all_inflight_parts() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let target = db.storage_targets().await.unwrap()[0].id;
        let session = db
            .create_upload_session(
                ROOT_ID,
                "cancel-all.bin",
                "/cancel-all.bin",
                4,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        tg.block_send.store(true, Ordering::SeqCst);

        let ResumablePartStart::Upload(handle) = up
            .start_resumable_part(&session.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("分块不应提前完成");
        };
        handle
            .feed(Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap();
        let request = tokio::spawn(handle.finish());
        for _ in 0..100 {
            if tg.send_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(tg.send_started.load(Ordering::SeqCst));

        up.begin_cancel_resumable_session(&session.session.id).await;
        assert!(request.await.unwrap().is_err());
        assert!(tg.sent.lock().unwrap().is_empty());
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 1);
        db.cancel_upload_session(&session.session.id).await.unwrap();
        up.finish_cancel_resumable_session(&session.session.id);
    }

    #[tokio::test]
    async fn cancelling_one_concurrent_session_restarts_once_and_interrupts_the_other() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        up.knobs.set_concurrency(2);
        let target = db.storage_targets().await.unwrap()[0].id;
        let session_a = db
            .create_upload_session(
                ROOT_ID,
                "cancel-a.bin",
                "/cancel-a.bin",
                4,
                &"00".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        let session_b = db
            .create_upload_session(
                ROOT_ID,
                "continue-b.bin",
                "/continue-b.bin",
                4,
                &"11".repeat(32),
                4,
                target,
            )
            .await
            .unwrap();
        tg.block_send.store(true, Ordering::SeqCst);

        let ResumablePartStart::Upload(handle_a) = up
            .start_resumable_part(&session_a.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("A 分块不应提前完成");
        };
        let ResumablePartStart::Upload(handle_b) = up
            .start_resumable_part(&session_b.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("B 分块不应提前完成");
        };
        handle_a
            .feed(Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap();
        handle_b
            .feed(Bytes::from_static(&[5, 6, 7, 8]))
            .await
            .unwrap();
        let request_a = tokio::spawn(handle_a.finish());
        let request_b = tokio::spawn(handle_b.finish());
        for _ in 0..100 {
            if tg.send_started_count.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(tg.send_started_count.load(Ordering::SeqCst), 2);

        up.begin_cancel_resumable_session(&session_a.session.id)
            .await;
        let error_a = request_a.await.unwrap().unwrap_err();
        let error_b = request_b.await.unwrap().unwrap_err();
        assert!(matches!(error_a, AppError::BadRequest(_)));
        assert!(matches!(error_b, AppError::UploadInterrupted));
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 1);
        assert!(tg.sent.lock().unwrap().is_empty());

        // B 收到专用可重试错误前，其数据库声明必须已经释放。
        let ResumablePartStart::Upload(retry_b) = up
            .start_resumable_part(&session_b.session.id, 0)
            .await
            .unwrap()
        else {
            panic!("B 分块应当可以安全续传");
        };
        drop(retry_b);

        // 等待重试句柄的异步清理完成，再删除两个会话，避免测试自身制造写事务竞争。
        up.begin_cancel_resumable_session(&session_b.session.id)
            .await;
        db.cancel_upload_session(&session_a.session.id)
            .await
            .unwrap();
        up.finish_cancel_resumable_session(&session_a.session.id);
        db.cancel_upload_session(&session_b.session.id)
            .await
            .unwrap();
        up.finish_cancel_resumable_session(&session_b.session.id);
    }

    #[tokio::test]
    async fn manual_mode_holds_the_delete_queue_until_asked() {
        use super::upload::QueueTrigger;
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        up.knobs.set_auto_delete_message(false);

        let node = feed_all(&up, "m.bin", &[1, 2, 3, 4, 5], 5).await.unwrap();
        db.delete_node(node.id).await.unwrap();

        // 手动模式下 worker 不许动队列:任务照常积压,消息还留在频道里
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            tg.deleted.lock().unwrap().is_empty(),
            "手动模式不应自动删除消息"
        );
        assert!(db.pending_delete_count().await.unwrap() > 0);

        // 后台点「立即执行」才真正消费
        let summary = up
            .run_delete_batch(200, QueueTrigger::Manual)
            .await
            .unwrap();
        assert!(summary.completed > 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.pending, 0);
        assert!(!tg.deleted.lock().unwrap().is_empty());
        assert_eq!(db.pending_delete_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn manual_run_ignores_retry_backoff() {
        use super::upload::QueueTrigger;
        let (up, _tg, db, _g) = fixture(4, 1024, None).await;
        up.knobs.set_auto_delete_message(false);

        let node = feed_all(&up, "b.bin", &[1, 2, 3, 4, 5], 5).await.unwrap();
        db.delete_node(node.id).await.unwrap();

        // 模拟整队都已失败重试过多次:退避被推到一小时以后
        let jobs = db.delete_jobs_due(10, false).await.unwrap();
        assert!(!jobs.is_empty());
        let queued = jobs.len();
        for job in jobs {
            db.delete_job_failed(job.id, 10, "message can't be deleted")
                .await
                .unwrap();
        }
        assert!(
            db.delete_jobs_due(10, false).await.unwrap().is_empty(),
            "退避未到期时不该取出任务"
        );

        // 手动执行必须绕开退避,否则后台点了像没反应
        let summary = up
            .run_delete_batch(200, QueueTrigger::Manual)
            .await
            .unwrap();
        assert_eq!(summary.completed, queued);
        assert_eq!(summary.pending, 0);
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
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mp4".to_string(), true)]
        );
        assert_eq!(*tg.sent_captions.lock().unwrap(), vec![None]);

        // 读回的字节与上传一致
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn thumbnail_is_sent_with_single_video_message() {
        let (up, tg, db, _g) = fixture(100, 1024, None).await;
        let thumbnail = Bytes::from_static(b"mock-jpeg-thumbnail");
        let node = feed_all_with_thumbnail(&up, "movie.mp4", &[1, 2, 3], 2, thumbnail.clone())
            .await
            .unwrap();

        assert_eq!(*tg.sent_thumbnail.lock().unwrap(), vec![true]);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(chunks[0].tg_thumbnail_file_id.as_deref(), Some("thumb-0"));

        let mut reader = tg.open_thumbnail_stream("thumb-0", 1000).await.unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, thumbnail);
    }

    #[tokio::test]
    async fn multi_chunk_thumbnail_is_only_attached_to_first_message() {
        let (up, tg, db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        let node = feed_all_with_thumbnail(
            &up,
            "movie.mp4",
            &data,
            3,
            Bytes::from_static(b"mock-jpeg-thumbnail"),
        )
        .await
        .unwrap();

        assert_eq!(*tg.sent_thumbnail.lock().unwrap(), vec![true, false, false]);
        let chunks = db.chunks_of(node.id).await.unwrap();
        assert_eq!(chunks[0].tg_thumbnail_file_id.as_deref(), Some("thumb-0"));
        assert!(
            chunks[1..]
                .iter()
                .all(|chunk| chunk.tg_thumbnail_file_id.is_none())
        );
    }

    #[tokio::test]
    async fn mkv_stays_document() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        feed_all(&up, "movie.mkv", &[1, 2, 3], 3).await.unwrap();
        // mkv 即使开着视频消息模式也按文档发送
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mkv".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn multi_chunk_video_stays_document() {
        let (up, tg, _db, _g) = fixture(4, 1024, None).await;
        let data: Vec<u8> = (0..10u8).collect();
        feed_all(&up, "movie.mp4", &data, 3).await.unwrap();
        // 3 个分块全部按文档发送(分块不是完整视频,发视频消息没有意义)
        let sent = tg.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        assert!(
            sent.iter().all(|(_, video)| !video),
            "分块不应按视频发送: {sent:?}"
        );
    }

    #[tokio::test]
    async fn video_mode_off_uses_document() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        up.knobs.set_send_video(false);
        feed_all(&up, "movie.mp4", &[1, 2, 3], 3).await.unwrap();
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mp4".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn video_send_failure_falls_back_to_document() {
        let (up, tg, db, _g) = fixture(100, 1024, None).await;
        tg.fail_video.store(true, Ordering::SeqCst);
        let data = [9u8, 8, 7];
        let node = feed_all(&up, "movie.mp4", &data, 3).await.unwrap();
        // sendVideo 失败后回退文档,上传仍成功
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mp4".to_string(), false)]
        );
        let chunks = db.chunks_of(node.id).await.unwrap();
        let mut r = ChunkedReader::new(up.tg.clone(), &chunks, 0);
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
        assert_eq!(tg.cancel_pending_calls.load(Ordering::SeqCst), 1);
        assert!(
            tg.sent.lock().unwrap().is_empty(),
            "结果不确定时不得再按文档发送"
        );
        assert!(db.resolve(&["movie.mp4"]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn video_upload_carries_probed_dimensions() {
        let (up, tg, _db, _g) = fixture(64 * 1024, 1024, None).await;
        // 合成 mp4(1920x1080、60 秒)走完整上传管线,宽高应传到 sendVideo
        let data = crate::engine::probe::testutil::mp4_file(1920, 1080, false);
        feed_all(&up, "movie.mp4", &data, 128).await.unwrap();
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mp4".to_string(), true)]
        );
        assert_eq!(
            *tg.video_meta.lock().unwrap(),
            vec![Some(VideoMeta {
                width: 1920,
                height: 1080,
                duration_secs: Some(60)
            })]
        );
    }

    #[tokio::test]
    async fn unparsable_video_sends_without_meta() {
        let (up, tg, _db, _g) = fixture(100, 1024, None).await;
        // 扩展名是视频但内容解析不了 → 仍按视频发送,只是不带宽高
        feed_all(&up, "movie.mp4", &[1, 2, 3], 3).await.unwrap();
        assert_eq!(
            *tg.sent.lock().unwrap(),
            vec![("movie.mp4".to_string(), true)]
        );
        assert_eq!(*tg.video_meta.lock().unwrap(), vec![None]);
    }
}
