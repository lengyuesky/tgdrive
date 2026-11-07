//! 只通过单节点回环入口取帧，探测、尝试及编码共用同一读取和时间预算。

use super::{Context, covers::IMAGE_LIMIT, index::Entry, probe};
use crate::{
    api::AppState,
    error::{AppError, AppResult},
};
use axum::http::{HeaderMap, Method};
use image::ImageEncoder;
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub(super) async fn available() -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(2),probe::command_for("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status()).await,Ok(Ok(s)) if s.success())
}

struct Input {
    url: String,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<()>,
    remaining: Arc<AtomicU64>,
    upstream: Arc<crate::telegram::ReadBudget>,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server.abort();
    }
}

impl Input {
    async fn open(state: &AppState, context: &Context, entry: &Entry) -> AppResult<Self> {
        let mut context = context.clone();
        context.lease.cancelled = context.lease.cancelled.child_token();
        let cancel = context.lease.cancelled.clone();
        let remaining = Arc::new(AtomicU64::new(probe::BYTE_LIMIT));
        let upstream = Arc::new(crate::telegram::ReadBudget::new(probe::BYTE_LIMIT));
        let requests = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let path = format!("/{}", crate::db::random_token(48));
        let url = format!("http://{address}{path}");
        let state = state.clone();
        let entry = entry.clone();
        let budget = remaining.clone();
        let upstream_budget = upstream.clone();
        let router = axum::Router::new().route(
            &path,
            axum::routing::get(move |headers: HeaderMap, method: Method| {
                let state = state.clone();
                let context = context.clone();
                let entry = entry.clone();
                let budget = budget.clone();
                let upstream = upstream_budget.clone();
                let requests = requests.clone();
                async move {
                    probe::bounded_source(
                        &state, &context, &entry, &headers, method, budget, requests, upstream,
                    )
                    .await
                }
            }),
        );
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
        });
        Ok(Self {
            url,
            cancel,
            server,
            remaining,
            upstream,
        })
    }
}

pub(super) async fn extract(
    state: &AppState,
    context: &Context,
    entry: &Entry,
) -> AppResult<Vec<u8>> {
    tokio::select! {
        biased;
        _=context.lease.cancelled.cancelled()=>Err(AppError::AppChanged),
        _=state.restore.shutdown.cancelled()=>Err(AppError::AppChanged),
        result=tokio::time::timeout(Duration::from_secs(10),extract_inner(state,context,entry))=>result.unwrap_or_else(|_| Err(AppError::BadRequest("封面截帧超过 10 秒预算".into())))
    }
}

async fn extract_inner(state: &AppState, context: &Context, entry: &Entry) -> AppResult<Vec<u8>> {
    let input = Input::open(state, context, entry).await?;
    let info = match probe::cached(state, entry).await? {
        Some(info) => info,
        None => match probe::run(&input.url, entry).await {
            Ok(info) => info,
            Err(_)
                if input.remaining.load(Ordering::Acquire) == 0
                    || input.upstream.used() >= probe::BYTE_LIMIT =>
            {
                return Err(AppError::BadRequest(
                    "封面截帧超过 8 MiB 累计读取预算".into(),
                ));
            }
            Err(error) => return Err(error),
        },
    };
    let video = info
        .video()
        .ok_or_else(|| AppError::BadRequest("没有可用于封面的真实视频轨道".into()))?;
    let pixels = video["width"]
        .as_u64()
        .unwrap_or(0)
        .saturating_mul(video["height"].as_u64().unwrap_or(0));
    if pixels == 0 || pixels > 16 * 1024 * 1024 {
        return Err(AppError::BadRequest("视频尺寸超出封面解码限额".into()));
    }
    let aspect = video["display_aspect_ratio"]
        .as_str()
        .and_then(|s| s.split_once(':'))
        .and_then(|(a, b)| Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?))
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(
            video["width"].as_u64().unwrap_or(1) as f64
                / video["height"].as_u64().unwrap_or(1) as f64,
        );
    let (width, height) = if aspect >= 1.0 {
        (640, (640.0 / aspect).round().clamp(1.0, 640.0) as u32)
    } else {
        ((640.0 * aspect).round().clamp(1.0, 640.0) as u32, 640)
    };
    let filter = format!("scale={width}:{height},setsar=1");
    let index = video["index"]
        .as_u64()
        .ok_or_else(|| AppError::BadRequest("视频轨道身份无效".into()))?;
    let interval = video["avg_frame_rate"]
        .as_str()
        .and_then(|s| s.split_once('/'))
        .and_then(|(a, b)| Some(b.parse::<f64>().ok()? / a.parse::<f64>().ok()?))
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(1.0 / 25.0);
    let last = (info.duration - interval).max(0.0);
    let mut positions = Vec::new();
    for (target, proportion) in [(1.0_f64, 0.1), (3.0, 0.4), (5.0, 0.8)] {
        let position = target.min(info.duration * proportion).min(last);
        if !positions
            .iter()
            .any(|previous: &f64| (*previous - position).abs() < 0.001)
        {
            positions.push(position);
        }
    }
    let mut failure = None;
    for position in positions {
        context.lease.check()?;
        if input.remaining.load(Ordering::Acquire) == 0
            || input.upstream.used() >= probe::BYTE_LIMIT
        {
            return Err(AppError::BadRequest(
                "封面截帧超过 8 MiB 累计读取预算".into(),
            ));
        }
        match run(&input.url, index, position, &filter).await {
            Ok(frame) => {
                if let Some(image) = encode(frame).await? {
                    return Ok(image);
                }
            }
            Err(_)
                if input.remaining.load(Ordering::Acquire) == 0
                    || input.upstream.used() >= probe::BYTE_LIMIT =>
            {
                return Err(AppError::BadRequest(
                    "封面截帧超过 8 MiB 累计读取预算".into(),
                ));
            }
            Err(error @ (AppError::Telegram(_) | AppError::Io(_))) => return Err(error),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| AppError::BadRequest("预算内没有取得可用的非黑视频画面".into())))
}

async fn run(input: &str, index: u64, position: f64, filter: &str) -> AppResult<Vec<u8>> {
    let mut child = probe::command_for("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "http,tcp",
            "-format_whitelist",
            "mov,matroska,avi,mpegts,flv,asf,ogg",
            "-probesize",
            "8388608",
            "-analyzeduration",
            "1000000",
            "-rw_timeout",
            "5000000",
            "-ss",
        ])
        .arg(format!("{position:.6}"))
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg(format!("0:{index}"))
        .args(["-an", "-sn", "-dn", "-frames:v", "1", "-vf"])
        .arg(filter)
        .args([
            "-threads",
            "1",
            "-c:v",
            "mjpeg",
            "-q:v",
            "3",
            "-f",
            "image2pipe",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| AppError::BadRequest("宿主封面截帧组件不可用".into()))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let read_output = async {
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() > 1024 * 1024 {
            return Err(AppError::BadRequest("截帧输出超过上限".into()));
        }
        Ok::<_, AppError>(())
    };
    let read_errors = async {
        stderr.take(8193).read_to_end(&mut errors).await?;
        if errors.len() > 8192 {
            return Err(AppError::BadRequest("截帧错误输出超过上限".into()));
        }
        Ok::<_, AppError>(())
    };
    let wait = async { child.wait().await.map_err(AppError::from) };
    let (_, _, status) = tokio::try_join!(read_output, read_errors, wait)?;
    if !status.success() || output.is_empty() {
        return Err(probe::process_error(
            &errors,
            "视频无法解码或封面读取预算不足",
        ));
    }
    Ok(output)
}

async fn encode(bytes: Vec<u8>) -> AppResult<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || {
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Jpeg);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(640);
        limits.max_image_height = Some(640);
        limits.max_alloc = Some(4 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| AppError::BadRequest("截帧结果不是有效的 JPEG".into()))?;
        if image.width() > 640 || image.height() > 640 {
            return Err(AppError::BadRequest("截帧尺寸超过 640 像素".into()));
        }
        let rgb = image.to_rgb8();
        let bright = rgb
            .pixels()
            .filter(|pixel| {
                (u32::from(pixel[0]) * 54 + u32::from(pixel[1]) * 183 + u32::from(pixel[2]) * 19)
                    / 256
                    > 16
            })
            .count();
        if bright * 100 < rgb.width() as usize * rgb.height() as usize * 2 {
            return Ok(None);
        }
        for quality in [85, 70, 50] {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|_| AppError::BadRequest("封面 JPEG 编码失败".into()))?;
            if jpeg.len() <= IMAGE_LIMIT {
                return Ok(Some(jpeg));
            }
        }
        Err(AppError::BadRequest("封面 JPEG 超过 256 KiB".into()))
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?
}
