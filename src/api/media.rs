//! 缩略图、下载与 Range 流式读取。

use super::*;

// ---------- 视频缩略图 ----------

/// 防止上游异常响应占用过多内存；Telegram 自定义缩略图本身限制为 200KB。
pub(super) const MAX_DOWNLOADED_THUMBNAIL_SIZE: usize = 512 * 1024;

#[derive(Deserialize)]
pub(super) struct ThumbnailQuery {
    pub(super) path: String,
}

pub(super) fn detect_thumbnail_mime(declared: &str, data: &[u8]) -> AppResult<&'static str> {
    if declared == "image/jpeg" && data.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else {
        Err(AppError::BadRequest("只接受有效的 JPEG 缩略图".into()))
    }
}

/// 从 JPEG 的 SOF 段读取尺寸，不解码像素。
pub(super) fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut pos = 2usize;
    while pos + 3 < data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        let marker = *data.get(pos)?;
        pos += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let segment_len = u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?]) as usize;
        if segment_len < 2 || pos + segment_len > data.len() {
            return None;
        }
        let is_sof = matches!(
            marker,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
        );
        if is_sof {
            if segment_len < 7 {
                return None;
            }
            let height = u16::from_be_bytes([data[pos + 3], data[pos + 4]]);
            let width = u16::from_be_bytes([data[pos + 5], data[pos + 6]]);
            return (width > 0 && height > 0).then_some((width, height));
        }
        pos += segment_len;
    }
    None
}

pub(super) fn validate_thumbnail_upload(declared: &str, data: &[u8]) -> AppResult<()> {
    if data.is_empty() || data.len() >= crate::telegram::MAX_THUMBNAIL_SIZE {
        return Err(AppError::BadRequest(format!(
            "缩略图大小必须在 1~{}KB 之间（不含上限）",
            crate::telegram::MAX_THUMBNAIL_SIZE / 1024
        )));
    }
    detect_thumbnail_mime(declared, data)?;
    let (width, height) = jpeg_dimensions(data)
        .ok_or_else(|| AppError::BadRequest("JPEG 缩略图缺少有效尺寸".into()))?;
    if width > 320 || height > 320 {
        return Err(AppError::BadRequest(
            "Telegram 缩略图的宽高都不能超过 320 像素".into(),
        ));
    }
    Ok(())
}

pub(crate) fn downloaded_thumbnail_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(super) fn thumbnail_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::empty())
        .expect("构造缩略图未命中响应失败")
}

pub(super) fn thumbnail_etag(file_id: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(file_id.as_bytes());
    format!(
        "\"telegram-thumbnail-{}\"",
        crate::engine::upload::to_hex(&digest)
    )
}

/// 根据第 0 块记录的 Telegram thumbnail file_id 请求小图；不会读取原视频。
pub(super) async fn thumbnail_get(
    State(state): State<AppState>,
    Query(q): Query<ThumbnailQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let node = state.vfs.resolve(&q.path).await?;
    thumbnail_for_node(&state, &node, &headers).await
}

pub(crate) async fn thumbnail_for_node(
    state: &AppState,
    node: &Node,
    headers: &HeaderMap,
) -> AppResult<Response> {
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let Some(first) = state.vfs.db.first_chunk(node.id).await? else {
        return Ok(thumbnail_not_found());
    };
    let Some(file_id) = first.tg_thumbnail_file_id.as_deref() else {
        // 404 不缓存，历史消息重新登记或替换后可立即显示。
        return Ok(thumbnail_not_found());
    };
    let etag = thumbnail_etag(file_id);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(axum::body::Body::empty())
            .expect("构造缩略图缓存响应失败"));
    }
    let storage_target_id = first
        .storage_target_id
        .ok_or_else(|| AppError::BadRequest("该历史文件没有可用的 Telegram 存储目标".into()))?;
    let binding = state.vfs.tg.binding(storage_target_id)?;
    let reader = binding
        .api
        .open_thumbnail_stream(file_id, first.tg_message_id)
        .await?;
    let mut limited = reader.take((MAX_DOWNLOADED_THUMBNAIL_SIZE + 1) as u64);
    let mut data = Vec::new();
    limited.read_to_end(&mut data).await?;
    if data.len() > MAX_DOWNLOADED_THUMBNAIL_SIZE {
        return Err(AppError::Telegram("Telegram 缩略图大小异常".into()));
    }
    let mime = downloaded_thumbnail_mime(&data)
        .ok_or_else(|| AppError::Telegram("Telegram 返回的缩略图格式无效".into()))?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::ETAG, etag)
        .header("x-content-type-options", "nosniff")
        .body(axum::body::Body::from(data))
        .expect("构造缩略图响应失败"))
}

/// 解析单个 Range;None=无 Range,Err=不可满足
pub(crate) fn parse_range(headers: &HeaderMap, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let Some(spec) = h.strip_prefix("bytes=") else {
        return Ok(None);
    };
    let first = spec.split(',').next().unwrap_or("").trim();
    let Some((a, b)) = first.split_once('-') else {
        return Err(());
    };
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            // 后缀范围:最后 n 字节
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 || total == 0 {
                return Err(());
            }
            (total.saturating_sub(n), total - 1)
        }
        (s, "") => (s.parse().map_err(|_| ())?, total.saturating_sub(1)),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse().map_err(|_| ())?),
    };
    if total == 0 || start >= total || start > end {
        return Err(());
    }
    Ok(Some((start, end.min(total - 1))))
}

pub(super) async fn download(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let node = state.vfs.resolve(&q.path).await?;
    stream_node(&state, &node, &headers, q.inline == Some(1)).await
}

/// 流式返回一个文件节点(带 Range 支持);REST 下载与分享下载共用
pub(crate) async fn stream_node(
    state: &AppState,
    node: &Node,
    headers: &HeaderMap,
    inline: bool,
) -> AppResult<Response> {
    if node.is_dir {
        return Err(AppError::IsDirectory);
    }
    let guessed_mime = mime_guess::from_path(&node.name).first_or_octet_stream();
    let safe_inline = inline_mime_allowed(guessed_mime.as_ref());
    let effective_inline = inline && safe_inline;
    // 主动内容即使带 inline=1 也只按二进制附件返回，避免 HTML/SVG/脚本在网盘
    // 自身域名下执行并读取认证 Cookie。
    let response_mime = if inline && !safe_inline {
        "application/octet-stream"
    } else {
        guessed_mime.as_ref()
    };
    let disposition = {
        let kind = if effective_inline {
            "inline"
        } else {
            "attachment"
        };
        let enc = utf8_percent_encode(&node.name, NON_ALPHANUMERIC);
        format!("{kind}; filename*=UTF-8''{enc}")
    };

    let etag = format!("\"{}\"", node.content_version());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|v| v == "*" || v.trim_start_matches("W/") == etag)
        })
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "private, no-cache")
            .body(axum::body::Body::empty())
            .expect("响应构造失败"));
    }
    // 只接受精确的强 ETag；日期或弱标签无法证明字节相同，必须返回整份 200。
    let range = if headers.contains_key(header::RANGE)
        && headers
            .get(header::IF_RANGE)
            .is_some_and(|value| value.to_str().ok() != Some(etag.as_str()))
    {
        Ok(None)
    } else {
        parse_range(headers, node.size as u64)
    };
    match range {
        Err(()) => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            [
                (header::CONTENT_RANGE, format!("bytes */{}", node.size)),
                (header::ETAG, etag),
            ],
        )
            .into_response()),
        Ok(None) => {
            let reader = state.vfs.open_reader(node, 0).await?;
            let total = reader.total_size();
            let body = axum::body::Body::from_stream(ReaderStream::new(reader));
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::ETAG, &etag)
                .header(header::CACHE_CONTROL, "private, no-cache")
                .header(header::CONTENT_TYPE, response_mime)
                .header(header::CONTENT_LENGTH, total)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
        Ok(Some((start, end))) => {
            let len = end - start + 1;
            let reader = state.vfs.open_reader(node, start).await?;
            let body =
                axum::body::Body::from_stream(ReaderStream::new(reader.with_read_limit(len)));
            Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::ETAG, &etag)
                .header(header::CACHE_CONTROL, "private, no-cache")
                .header(header::CONTENT_TYPE, response_mime)
                .header(header::CONTENT_LENGTH, len)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", node.size),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(body)
                .expect("响应构造失败"))
        }
    }
}

pub(crate) fn inline_mime_allowed(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg"
            | "image/png"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "audio/mpeg"
            | "audio/mp4"
            | "audio/aac"
            | "audio/ogg"
            | "audio/webm"
            | "audio/wav"
            | "audio/flac"
            | "video/mp4"
            | "video/webm"
            | "video/ogg"
            | "video/quicktime"
            | "video/x-matroska"
            | "text/plain"
            | "application/pdf"
    )
}
