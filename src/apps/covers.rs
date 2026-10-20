//! 应用封面缓存能力：插件把生成好的封面小图存进独立封面库，下次打开直接取用。
//! 封面是可再生缓存：不需要额外权限，不发事件，超出上限自动淘汰最久未看的封面。
use super::{InstalledApp, files::parse, installed, storage};
use crate::api::AppState;
use crate::db::apps::{COVER_LIMIT_BYTES, COVER_LIMIT_ENTRIES, CoverLimits, CoverStats};
use crate::error::{AppError, AppResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

/// 单张封面解码后的上限；列表缩略图通常只有几十 KiB。
pub(super) const MAX_COVER_BYTES: usize = 256 * 1024;
const MAX_META_BYTES: usize = 4 * 1024;
const MAX_KEYS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    key: String,
    data: String,
    #[serde(default)]
    meta: Option<Value>,
}

fn keys(raw: Vec<String>) -> AppResult<Vec<String>> {
    if raw.is_empty() || raw.len() > MAX_KEYS {
        return Err(AppError::BadRequest(format!(
            "每次需指定 1 到 {MAX_KEYS} 个封面键"
        )));
    }
    let mut unique = Vec::with_capacity(raw.len());
    for key in raw {
        storage::valid(&key, false)?;
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    Ok(unique)
}

/// 解析 `data:image/…;base64,…`：只接受 WebP、JPEG、PNG，且文件头必须与声明一致。
pub(super) fn decode(raw: &str) -> AppResult<(&'static str, Vec<u8>)> {
    let invalid = || AppError::BadRequest("封面需为 WebP、JPEG 或 PNG 的 base64 data URL".into());
    let (declared, payload) = raw
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .ok_or_else(invalid)?;
    let too_large = || AppError::BadRequest("单张封面最多 256 KiB".into());
    if payload.len() > MAX_COVER_BYTES.div_ceil(3) * 4 {
        return Err(too_large());
    }
    let data = STANDARD.decode(payload).map_err(|_| invalid())?;
    if data.is_empty() {
        return Err(invalid());
    }
    if data.len() > MAX_COVER_BYTES {
        return Err(too_large());
    }
    let mime = crate::api::downloaded_thumbnail_mime(&data).ok_or_else(invalid)?;
    if mime != declared {
        return Err(AppError::BadRequest("封面图片格式与声明不符".into()));
    }
    Ok((mime, data))
}

pub(super) fn stats_json(stats: CoverStats) -> Value {
    json!({"entries": stats.entries, "bytes": stats.bytes,
        "limit_bytes": COVER_LIMIT_BYTES, "limit_entries": COVER_LIMIT_ENTRIES})
}

pub(super) async fn invoke(
    state: &AppState,
    app: &InstalledApp,
    method: &str,
    params: Value,
) -> AppResult<Value> {
    let db = &state.vfs.db;
    let id = &app.manifest.id;
    // 验证凭据到实际写入之间可能发生替换；与安装和缓存回收串行，再核对修订。
    // 旧页面的迟到删除也不能误删后来重装应用的同名封面。
    let _guard = if matches!(method, "covers.put" | "covers.delete") {
        let guard = super::INSTALL_LOCK.lock().await;
        let current = installed(state, id).await?;
        if !current.enabled || current.revision != app.revision {
            return Err(AppError::AppChanged);
        }
        Some(guard)
    } else {
        None
    };
    match method {
        "covers.get" => {
            let q: Keys = parse(params)?;
            let rows = db.cover_get_many(id, &keys(q.keys)?).await?;
            let covers = rows
                .into_iter()
                .map(|row| {
                    let meta = row
                        .meta
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .unwrap_or(Value::Null);
                    json!({"key": row.key, "meta": meta, "updated_at": row.updated_at,
                        "data": format!("data:{};base64,{}", row.mime, STANDARD.encode(&row.data))})
                })
                .collect::<Vec<_>>();
            Ok(json!({ "covers": covers }))
        }
        "covers.put" => {
            let q: Put = parse(params)?;
            storage::valid(&q.key, false)?;
            let (mime, data) = decode(&q.data)?;
            let meta = q
                .meta
                .filter(|value| !value.is_null())
                .map(|value| serde_json::to_string(&value).map_err(anyhow::Error::from))
                .transpose()?;
            if meta
                .as_ref()
                .is_some_and(|text| text.len() > MAX_META_BYTES)
            {
                return Err(AppError::BadRequest("封面附加信息最多 4 KiB".into()));
            }
            let evicted = db
                .cover_put(
                    id,
                    &q.key,
                    mime,
                    &data,
                    meta.as_deref(),
                    CoverLimits::default(),
                )
                .await?;
            // 封面库与安装记录不在同一事务：写入期间应用被卸载或更新时撤回，避免留下孤儿封面。
            if !installed(state, id)
                .await
                .is_ok_and(|current| current.enabled && current.revision == app.revision)
            {
                db.cover_delete(id, &[q.key]).await?;
                return Err(AppError::AppChanged);
            }
            Ok(json!({"ok": true, "bytes": data.len(), "evicted": evicted}))
        }
        "covers.delete" => {
            let q: Keys = parse(params)?;
            let deleted = db.cover_delete(id, &keys(q.keys)?).await?;
            Ok(json!({"ok": true, "deleted": deleted}))
        }
        "covers.stats" => Ok(stats_json(db.cover_stats(id).await?)),
        _ => Err(AppError::BadRequest("不支持的封面缓存能力".into())),
    }
}
