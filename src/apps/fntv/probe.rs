//! 真实媒体探测：单节点本机入口、累计字节预算、固定命令和可取消子进程。

use super::{Context, authorized, data, index::Entry};
use crate::{
    api::AppState,
    error::{AppError, AppResult},
};
use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;

pub(super) const BYTE_LIMIT: u64 = 8 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 128 * 1024;
const TIME_LIMIT: Duration = Duration::from_secs(10);

pub(super) fn limits() -> Value {
    json!({"probe_bytes":BYTE_LIMIT,"probe_seconds":10,"probe_concurrency":2,"probe_queue":0,
        "probe_output_bytes":OUTPUT_LIMIT,"nfo_bytes":64*1024,"poster_bytes":2*1024*1024,"subtitle_bytes":512*1024,
        "index_videos":3000,"private_data_bytes":8*1024*1024,"private_data_entries":10000})
}

pub(super) fn command_for(program: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(program);
    // 不将宿主的代理、密钥或其它环境配置交给媒体解析进程。
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    c
}

fn command() -> tokio::process::Command {
    command_for("ffprobe")
}

pub(super) async fn available() -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(2),command().arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status()).await,Ok(Ok(s)) if s.success())
}

/// 只解析受限子进程的网络错误类别，不向客户端返回输入地址或原始错误输出。
pub(super) fn process_error(errors: &[u8], message: &str) -> AppError {
    let errors = String::from_utf8_lossy(errors).to_ascii_lowercase();
    if [
        "timed out",
        "input/output error",
        "connection reset",
        "connection refused",
        "server returned 5",
        "http error 5",
    ]
    .iter()
    .any(|pattern| errors.contains(pattern))
    {
        AppError::Telegram("媒体分段读取暂时失败".into())
    } else {
        AppError::BadRequest(message.into())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct MediaInfo {
    pub version: String,
    pub wrapper: String,
    pub duration: f64,
    pub streams: Vec<Value>,
    pub reserved_bytes: u64,
    #[serde(default)]
    pub upstream_reserved_bytes: u64,
    #[serde(default)]
    pub policy_version: u32,
}

impl MediaInfo {
    pub fn video(&self) -> Option<&Value> {
        self.streams
            .iter()
            .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
    }
    pub fn valid(&self, entry: &Entry) -> bool {
        self.version == entry.version
            && self.duration.is_finite()
            && self.duration > 0.0
            && self.streams.len() <= 64
            && self.video().is_some()
            && self.reserved_bytes <= BYTE_LIMIT
            && self.upstream_reserved_bytes <= BYTE_LIMIT
            && self.policy_version == 1
    }
}

pub(super) async fn cached(state: &AppState, entry: &Entry) -> AppResult<Option<MediaInfo>> {
    Ok(
        data::get::<MediaInfo>(&state.vfs.db, &format!("fntv:probe:{}", entry.media_guid))
            .await?
            .filter(|m| m.valid(entry)),
    )
}

pub(super) async fn media(
    state: &AppState,
    context: &Context,
    entry: &Entry,
) -> AppResult<MediaInfo> {
    authorized(state, context, entry.node_id, &entry.version).await?;
    if let Some(info) = cached(state, entry).await? {
        return Ok(info);
    }
    // 不排入无界队列；客户端可以稍后重试。
    let _foreground = super::covers::foreground(state);
    let _slot = state
        .vfs
        .db
        .fntv
        .probe_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Conflict("媒体探测正忙，请稍后重试".into()))?;
    if let Some(info) = cached(state, entry).await? {
        return Ok(info);
    }
    let mut local_context = context.clone();
    local_context.lease.cancelled = context.lease.cancelled.child_token();
    let budget = Arc::new(AtomicU64::new(BYTE_LIMIT));
    let upstream_budget = Arc::new(crate::telegram::ReadBudget::new(BYTE_LIMIT));
    let source_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let requests = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let path = format!("/{}", crate::db::random_token(48));
    let input = format!("http://{address}{path}");
    let endpoint_state = state.clone();
    let endpoint_context = local_context.clone();
    let endpoint_entry = entry.clone();
    let endpoint_budget = budget.clone();
    let endpoint_upstream = upstream_budget.clone();
    let endpoint_error = source_error.clone();
    let endpoint_requests = requests.clone();
    let router = axum::Router::new().route(
        &path,
        axum::routing::get(move |headers: HeaderMap, method: Method| {
            let state = endpoint_state.clone();
            let context = endpoint_context.clone();
            let entry = endpoint_entry.clone();
            let budget = endpoint_budget.clone();
            let requests = endpoint_requests.clone();
            let upstream = endpoint_upstream.clone();
            let error = endpoint_error.clone();
            async move {
                let result = bounded_source(
                    &state, &context, &entry, &headers, method, budget, requests, upstream,
                )
                .await;
                if let Err(failure) = &result {
                    let (message, _) = failure.public_parts();
                    error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(message);
                }
                result
            }
        }),
    );
    let shutdown = local_context.lease.cancelled.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let result = tokio::select! {
        biased;
        _=context.lease.cancelled.cancelled()=>Err(AppError::AppChanged),
        result=tokio::time::timeout(TIME_LIMIT,run(&input,entry))=>match result {
            Ok(result)=>result,
            Err(_)=>Err(AppError::BadRequest("媒体探测超过 10 秒上限，未缓存或转换视频".into())),
        }
    };
    local_context.lease.cancelled.cancel();
    let abort = server.abort_handle();
    if tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .is_err()
    {
        abort.abort();
    }
    let mut info = match result {
        Ok(info) => info,
        Err(error) => {
            if let Some(message) = source_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Err(AppError::BadRequest(message));
            }
            return Err(error);
        }
    };
    info.reserved_bytes = BYTE_LIMIT - budget.load(Ordering::Acquire);
    info.upstream_reserved_bytes = upstream_budget.used();
    context.lease.check()?;
    if !info.valid(entry) {
        return Err(AppError::BadRequest(
            "未能在预算内读取真实视频时长和轨道".into(),
        ));
    }
    data::save(
        &state.vfs.db,
        context,
        &format!("fntv:probe:{}", entry.media_guid),
        &info,
    )
    .await?;
    Ok(info)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn bounded_source(
    state: &AppState,
    context: &Context,
    entry: &Entry,
    headers: &HeaderMap,
    method: Method,
    budget: Arc<AtomicU64>,
    requests: Arc<AtomicUsize>,
    upstream: Arc<crate::telegram::ReadBudget>,
) -> AppResult<Response> {
    if requests.fetch_add(1, Ordering::AcqRel) >= 32 {
        return Err(AppError::BadRequest("媒体探测请求次数超过上限".into()));
    }
    let (node, _) = authorized(state, context, entry.node_id, &entry.version).await?;
    if method == Method::HEAD {
        return Ok(Response::builder()
            .header(header::CONTENT_LENGTH, node.size)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::empty())
            .unwrap());
    }
    let size = node.size as u64;
    if size == 0 {
        return Err(AppError::BadRequest("空文件无法进行媒体探测".into()));
    }
    let range = crate::api::parse_range(headers, size)
        .map_err(|_| AppError::BadRequest("探测区间无效".into()))?
        .unwrap_or((0, size.saturating_sub(1)));
    let wanted = range.1 - range.0 + 1;
    let old = budget.load(Ordering::Acquire);
    if old == 0 {
        return Err(AppError::BadRequest(
            "媒体探测超过 8 MiB 累计读取上限".into(),
        ));
    }
    let length = wanted.min(old);
    let end = range.0 + length - 1;
    let stream =
        super::protocol::probe_stream(state, context, &node, range.0, length, upstream).await?;
    use futures_util::StreamExt;
    let stream = Body::from_stream(futures_util::stream::try_unfold(
        (stream.into_data_stream(), budget),
        |(mut stream, budget)| async move {
            let Some(bytes) = stream.next().await else {
                return Ok::<_, std::io::Error>(None);
            };
            let bytes = bytes.map_err(std::io::Error::other)?;
            #[allow(deprecated)]
            budget
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(bytes.len() as u64)
                })
                .map_err(|_| std::io::Error::other("媒体探测超过 8 MiB 累计读取上限"))?;
            Ok(Some((bytes, (stream, budget))))
        },
    ));
    Ok(Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{end}/{size}", range.0),
        )
        .body(stream)
        .unwrap())
}

pub(super) async fn run(input: &str, entry: &Entry) -> AppResult<MediaInfo> {
    let mut child = command()
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            "http,tcp",
            "-format_whitelist",
            "mov,matroska,avi,mpegts,flv,asf,ogg",
            "-probesize",
            "8388608",
            "-analyzeduration",
            "8000000",
            "-rw_timeout",
            "5000000",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| AppError::BadRequest("无法启动宿主 ffprobe".into()))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let read_output = async {
        stdout
            .take(OUTPUT_LIMIT as u64 + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() > OUTPUT_LIMIT {
            return Err(AppError::BadRequest("媒体探测输出超过 128 KiB 上限".into()));
        }
        Ok::<_, AppError>(())
    };
    let read_errors = async {
        stderr.take(8193).read_to_end(&mut errors).await?;
        if errors.len() > 8192 {
            return Err(AppError::BadRequest("媒体探测错误输出超过上限".into()));
        }
        Ok::<_, AppError>(())
    };
    let wait = async { child.wait().await.map_err(AppError::from) };
    let (_, _, status) = tokio::try_join!(read_output, read_errors, wait)?;
    if !status.success() {
        return Err(process_error(
            &errors,
            "媒体探测失败或读取预算不足，未缓存或转换视频",
        ));
    }
    let value: Value = serde_json::from_slice(&output)
        .map_err(|_| AppError::BadRequest("媒体探测结果无效".into()))?;
    let duration = value["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| AppError::BadRequest("未能在预算内读取真实时长".into()))?;
    let streams = value["streams"]
        .as_array()
        .filter(|s| s.len() <= 64)
        .ok_or_else(|| AppError::BadRequest("媒体轨道数量超限或无效".into()))?;
    let fields = [
        "index",
        "codec_name",
        "codec_type",
        "profile",
        "width",
        "height",
        "coded_width",
        "coded_height",
        "display_aspect_ratio",
        "pix_fmt",
        "level",
        "color_range",
        "color_space",
        "color_transfer",
        "color_primaries",
        "refs",
        "r_frame_rate",
        "avg_frame_rate",
        "bits_per_raw_sample",
        "bit_rate",
        "field_order",
        "sample_rate",
        "channels",
        "channel_layout",
        "tags",
        "disposition",
        "side_data_list",
    ];
    let streams = streams
        .iter()
        .map(|s| {
            let mut object = serde_json::Map::new();
            for field in fields {
                if let Some(v) = s.get(field) {
                    object.insert(field.into(), v.clone());
                }
            }
            Value::Object(object)
        })
        .collect();
    let format = value["format"]["format_name"].as_str().unwrap_or_default();
    let wrapper = if format.contains("matroska") {
        "mkv"
    } else if format.contains("mov") {
        "mp4"
    } else {
        format.split(',').next().unwrap_or_default()
    };
    Ok(MediaInfo {
        version: entry.version.clone(),
        wrapper: wrapper.into(),
        duration,
        streams,
        reserved_bytes: 0,
        upstream_reserved_bytes: 0,
        policy_version: 1,
    })
}
