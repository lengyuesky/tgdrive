//! 应用私有数据接口，不接受跨应用命名空间或无条件覆盖。
use super::{InstalledApp, files::parse};
use crate::{
    api::AppState,
    error::{AppError, AppResult},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Write {
    key: String,
    value: Option<Value>,
    expected_revision: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    prefix: String,
    cursor: Option<String>,
    limit: Option<u16>,
}
fn valid(key: &str, prefix: bool) -> AppResult<()> {
    if (!prefix && key.is_empty())
        || key.len() > 128
        || !key
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b":_-".contains(&v))
    {
        return Err(AppError::BadRequest(
            "应用数据键需为最多 128 字节的英文、数字、冒号、短横线或下划线".into(),
        ));
    }
    Ok(())
}
pub(super) async fn invoke(
    state: &AppState,
    app: &InstalledApp,
    method: &str,
    params: Value,
) -> AppResult<Value> {
    let db = &state.vfs.db;
    match method {
        "storage.get" => {
            let key: Key = parse(params)?;
            valid(&key.key, false)?;
            db.app_data_get(&app.manifest.id, &key.key)
                .await?
                .map(|r| r.json())
                .transpose()
                .map(|v| v.unwrap_or(Value::Null))
        }
        "storage.list" => {
            let q: List = parse(params)?;
            valid(&q.prefix, true)?;
            let limit = q.limit.unwrap_or(50);
            if !(1..=200).contains(&limit) {
                return Err(AppError::BadRequest("应用数据分页上限为 1 至 200".into()));
            }
            db.app_data_list(&app.manifest.id, &q.prefix, q.cursor.as_deref(), limit)
                .await
        }
        "storage.set" | "storage.delete" => {
            let q: Write = parse(params)?;
            valid(&q.key, false)?;
            if q.expected_revision.as_ref().is_some_and(|v| v.len() > 64) {
                return Err(AppError::BadRequest("数据修订标识无效".into()));
            }
            let value = if method == "storage.set" {
                let value = q
                    .value
                    .ok_or_else(|| AppError::BadRequest("缺少应用数据".into()))?;
                let text = serde_json::to_string(&value).map_err(anyhow::Error::from)?;
                if text.len() > 32 * 1024 {
                    return Err(AppError::BadRequest("单条应用数据最多 32 KiB".into()));
                }
                Some(text)
            } else {
                None
            };
            let record = db
                .app_data_write(
                    &app.manifest.id,
                    &app.revision,
                    &q.key,
                    value.as_deref(),
                    q.expected_revision.as_deref(),
                )
                .await?;
            record
                .map(|r| r.json())
                .transpose()
                .map(|v| v.unwrap_or(json!({"ok":true})))
        }
        _ => Err(AppError::BadRequest("不支持的应用数据能力".into())),
    }
}
