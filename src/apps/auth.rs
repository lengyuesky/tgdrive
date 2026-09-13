//! 应用运行凭据，从主程序的认证模块接入，沿用当前账号签名密钥。

use super::{Auth, b64};
use base64::Engine as _;

impl Auth {
    /// 插件凭据与登录、分享凭据使用独立的签名域；轮换账号密钥时一并失效。
    pub(crate) fn sign_app_claims(&self, claims: &str) -> String {
        let payload = b64(claims.as_bytes());
        format!("{payload}.{}", self.sign(&format!("app-v1:{payload}")))
    }

    pub(crate) fn verify_app_claims(&self, token: &str) -> Option<String> {
        if token.len() > 4096 {
            return None;
        }
        let (payload, signature) = token.split_once('.')?;
        if !Self::ct_eq(&self.sign(&format!("app-v1:{payload}")), signature) {
            return None;
        }
        String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .ok()?,
        )
        .ok()
    }
}
