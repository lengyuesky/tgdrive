//! 游标只包含有签名的宿主内部键，不暴露可修改的查询参数或无限期快照。

use super::{PlatformError, PlatformResult, now};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tgdrive_contract::{error_code, http::MAX_CURSOR_BYTES};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CursorData {
    pub kind: String,
    pub app: String,
    pub generation: String,
    pub scope: Option<String>,
    pub expires_at: Option<i64>,
    pub binding: String,
    pub position: serde_json::Value,
    pub scan: Option<String>,
    pub epoch: Option<String>,
    pub high_water: Option<i64>,
}

pub(crate) struct CursorCodec {
    key: Vec<u8>,
    key_id: String,
    keys: std::collections::BTreeMap<String, Vec<u8>>,
}
impl CursorCodec {
    pub fn new(key: String, keys: std::sync::Arc<Vec<String>>) -> Self {
        let key_id = crate::engine::upload::to_hex(&Sha256::digest(key.as_bytes()));
        let keys = keys
            .iter()
            .map(|key| {
                (
                    crate::engine::upload::to_hex(&Sha256::digest(key.as_bytes())),
                    key.as_bytes().to_vec(),
                )
            })
            .collect();
        Self {
            key: key.into_bytes(),
            key_id,
            keys,
        }
    }
    pub fn encode(&self, data: &CursorData) -> PlatformResult<String> {
        let body = serde_json::to_vec(data)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC 接受任意长度密钥");
        mac.update(&body);
        let token = format!(
            "{}.{}.{}",
            self.key_id,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        );
        if token.len() > MAX_CURSOR_BYTES as usize {
            return Err(PlatformError::new(
                error_code::INVALID_CURSOR,
                "查询游标超过上限",
            ));
        }
        Ok(token)
    }
    pub fn decode(&self, token: &str) -> PlatformResult<CursorData> {
        let invalid =
            || PlatformError::new(error_code::INVALID_CURSOR, "游标无效，请从首页重新查询");
        if token.len() > MAX_CURSOR_BYTES as usize {
            return Err(invalid());
        }
        let (key_id, remaining) = token.split_once('.').ok_or_else(invalid)?;
        let key = self.keys.get(key_id).ok_or_else(invalid)?;
        let (body, signature) = remaining.split_once('.').ok_or_else(invalid)?;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| invalid())?;
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| invalid())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
        mac.update(&body);
        mac.verify_slice(&signature).map_err(|_| invalid())?;
        let data: CursorData = serde_json::from_slice(&body).map_err(|_| invalid())?;
        if data.expires_at.is_some_and(|expiry| expiry <= now().0) {
            return Err(PlatformError::new(
                error_code::CURSOR_EXPIRED,
                "游标已过期，请重新查询",
            ));
        }
        Ok(data)
    }
    pub fn check(
        data: &CursorData,
        kind: &str,
        app: &super::store::Installation,
        binding: &str,
        scope: bool,
    ) -> PlatformResult<()> {
        if data.kind != kind || data.app != app.id || data.binding != binding {
            return Err(PlatformError::new(
                error_code::INVALID_CURSOR,
                "游标与当前查询不匹配",
            ));
        }
        if data.generation != app.generation {
            return Err(PlatformError::new(
                error_code::APP_CHANGED,
                "游标所属安装已改变",
            ));
        }
        if scope && data.scope.as_deref() != Some(&app.scope_revision) {
            return Err(PlatformError::new(
                error_code::SCOPE_CHANGED,
                "游标所属授权已改变",
            ));
        }
        Ok(())
    }
}

pub(crate) fn binding<T: Serialize>(request: &T) -> PlatformResult<String> {
    Ok(crate::engine::upload::to_hex(&Sha256::digest(
        serde_json::to_vec(request)?,
    )))
}
