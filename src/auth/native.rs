//! 原生凭据使用独立签名域；刷新凭据只保存哈希，Access Token 每次请求核验设备撤销状态。
use super::*;
use crate::db::devices::DeviceSession;

pub(crate) const DEVICE_ACCESS_TTL: i64 = 15 * 60;
pub(crate) const DEVICE_REFRESH_TTL: i64 = 30 * 24 * 3600;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeClaims {
    pub purpose: String,
    pub session_id: String,
    pub auth_epoch: String,
    pub expires_at: i64,
    pub node_id: Option<i64>,
    pub content_version: Option<String>,
}

fn sign_with(creds: &Creds, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(&creds.secret).expect("HMAC 任意密钥长度均可");
    mac.update(payload.as_bytes());
    b64(&mac.finalize().into_bytes())
}

impl Auth {
    pub(crate) fn device_epoch(&self) -> String {
        sign_with(&self.creds(), "native-auth-epoch-v1")
    }

    pub(crate) fn sign_native_claims(&self, claims: &NativeClaims) -> AppResult<String> {
        let creds = self.creds();
        if !Self::ct_eq(
            &claims.auth_epoch,
            &sign_with(&creds, "native-auth-epoch-v1"),
        ) {
            return Err(AppError::Unauthorized);
        }
        let payload = b64(&serde_json::to_vec(claims).map_err(anyhow::Error::from)?);
        let signature = sign_with(&creds, &format!("native-v1:{payload}"));
        Ok(format!("{payload}.{signature}"))
    }

    pub(crate) fn verify_native_claims(&self, token: &str) -> Option<NativeClaims> {
        if token.len() > 4096 {
            return None;
        }
        let (payload, signature) = token.split_once('.')?;
        let creds = self.creds();
        if !Self::ct_eq(
            &sign_with(&creds, &format!("native-v1:{payload}")),
            signature,
        ) {
            return None;
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let claims: NativeClaims = serde_json::from_slice(&bytes).ok()?;
        if claims.expires_at <= crate::db::now_ts()
            || !Self::ct_eq(
                &claims.auth_epoch,
                &sign_with(&creds, "native-auth-epoch-v1"),
            )
        {
            return None;
        }
        Some(claims)
    }

    pub(crate) async fn device_from_headers(
        &self,
        headers: &http::HeaderMap,
        db: &Db,
    ) -> AppResult<Option<DeviceSession>> {
        let Some(token) = device_bearer(headers) else {
            return Ok(None);
        };
        let Some(claims) = self.verify_native_claims(token) else {
            return Ok(None);
        };
        if claims.purpose != "access"
            || claims.node_id.is_some()
            || claims.content_version.is_some()
        {
            return Ok(None);
        }
        db.active_device_session(&claims.session_id, &claims.auth_epoch)
            .await
    }

    /// 只接受已验证主密码及第二因素的设备会话或网页会话，绝不把一般 Bearer 当强认证。
    pub(crate) async fn strong_session_valid(
        &self,
        headers: &http::HeaderMap,
        db: &Db,
    ) -> AppResult<bool> {
        Ok(self.session_cookie_valid(headers)
            || self.device_from_headers(headers, db).await?.is_some())
    }
}

pub(crate) fn device_bearer(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token.bytes().any(|b| b.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

pub(crate) fn refresh_hash(value: &str) -> String {
    sha256_hex(value)
}
