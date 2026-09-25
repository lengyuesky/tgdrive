//! 事件流 (SSE) 与文件树变更广播。

use super::*;

// ---------- 事件流 (SSE) ----------

/// 单用户事件流：应用数据、设置、目录范围与文件树的粗粒度变更。
/// 只携带标识不携带内容；重连由 EventSource 自动完成，断档由客户端全量重拉弥补。
/// 广播已知目录的文件树变更；空列表表示可见树无变化。
pub(super) fn emit_files(state: &AppState, paths: Vec<String>) {
    crate::events::emit(&state.events, crate::events::HostEvent::files_at(paths));
}

/// 广播某目录列表变更；目录不可见（并发删除）时退化为范围未知。
pub(super) async fn emit_files_in_parent(state: &AppState, parent_id: Option<i64>) {
    let path = match parent_id {
        Some(id) => state.vfs.db.full_path_if_visible(id).await.ok().flatten(),
        None => None,
    };
    let event = match path {
        Some(path) => crate::events::HostEvent::files_at(vec![path]),
        None => crate::events::HostEvent::files(),
    };
    crate::events::emit(&state.events, event);
}

/// 文件事件静默此时长后下发；批量上传时每个文件完成都会发一次，合并后每连接只发一条。
pub(super) const FILES_EVENT_QUIET: std::time::Duration = std::time::Duration::from_millis(250);
/// 连续变更不断时最多延迟这么久，保证进度可见。
pub(super) const FILES_EVENT_MAX_DELAY: std::time::Duration =
    std::time::Duration::from_millis(1000);

/// 一条 SSE 连接上待合并的文件事件。
pub(super) struct PendingFiles {
    pub(super) first: tokio::time::Instant,
    pub(super) last: tokio::time::Instant,
    /// `None` 表示合并结果范围未知。
    pub(super) paths: Option<Vec<String>>,
}

impl PendingFiles {
    fn merge(pending: &mut Option<Self>, paths: Option<Vec<String>>) {
        let now = tokio::time::Instant::now();
        match pending {
            None => {
                *pending = Some(Self {
                    first: now,
                    last: now,
                    paths,
                })
            }
            Some(existing) => {
                existing.last = now;
                match (&mut existing.paths, paths) {
                    (Some(merged), Some(incoming)) => {
                        for path in incoming {
                            if !merged.contains(&path) {
                                merged.push(path);
                            }
                        }
                        if merged.len() > crate::events::MAX_EVENT_PATHS {
                            existing.paths = None;
                        }
                    }
                    (merged, _) => *merged = None,
                }
            }
        }
    }

    fn deadline(&self) -> tokio::time::Instant {
        (self.last + FILES_EVENT_QUIET).min(self.first + FILES_EVENT_MAX_DELAY)
    }

    fn frame(self) -> String {
        let mut event = crate::events::HostEvent::files();
        event.paths = self.paths.map(|mut paths| {
            paths.sort();
            paths
        });
        sse_frame(&event)
    }
}

pub(super) fn sse_frame(event: &crate::events::HostEvent) -> String {
    let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    format!("event: {}\ndata: {json}\n\n", event.kind)
}

pub(super) async fn events_sse(State(state): State<AppState>) -> Response {
    let receiver = state.events.subscribe();
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(30));
    keepalive.tick().await; // 跳过立即触发的首次 tick
    let stream = futures_util::stream::once(async {
        Ok::<_, std::io::Error>("retry: 5000\n\n".to_string())
    })
    .chain(futures_util::stream::unfold(
        (receiver, keepalive, None::<PendingFiles>),
        |(mut receiver, mut keepalive, mut pending)| async move {
            loop {
                // 文件事件在本连接内合并去抖；其他事件立即下发，不受待合并事件阻塞。
                let deadline = pending.as_ref().map(PendingFiles::deadline);
                let far = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) if event.kind == "files" => {
                            PendingFiles::merge(&mut pending, event.paths);
                            continue;
                        }
                        Ok(event) => {
                            return Some((Ok(sse_frame(&event)), (receiver, keepalive, pending)));
                        }
                        // 落后丢失的通知里可能有文件变更：合并为一次范围未知的刷新。
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            PendingFiles::merge(&mut pending, None);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return pending.take().map(|files| (Ok(files.frame()), (receiver, keepalive, None)));
                        }
                    },
                    _ = tokio::time::sleep_until(deadline.unwrap_or(far)), if deadline.is_some() => {
                        if let Some(files) = pending.take() {
                            return Some((Ok(files.frame()), (receiver, keepalive, None)));
                        }
                    }
                    _ = keepalive.tick() => {
                        return Some((Ok(": keepalive\n\n".to_string()), (receiver, keepalive, pending)));
                    }
                }
            }
        },
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        // 反向代理（nginx 等）默认缓冲响应体会让 SSE 卡住。
        .header("x-accel-buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .expect("构造事件流失败")
}
