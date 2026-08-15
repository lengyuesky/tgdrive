//! 认证:单用户。Web 端 HMAC 签名 cookie,WebDAV 端 HTTP Basic,中间件双通道放行。
//!
//! 账号与签名密钥可在运行时替换(后台改配置),已被克隆持有的 `Arc<Auth>` 立即生效。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::sync::{Arc, RwLock};
use subtle::ConstantTimeEq;

const COOKIE_NAME: &str = "tgdrive_token";
const TOKEN_TTL_SECS: i64 = 7 * 24 * 3600;
const SHARE_COOKIE_NAME: &str = "tgdrive_share";
const SHARE_TICKET_TTL_SECS: i64 = 30 * 60;
/// settings 表里存签名密钥的键
pub const SECRET_KEY: &str = "secret";

type HmacSha256 = Hmac<Sha256>;

struct Creds {
    username: String,
    password: String,
    secret: Vec<u8>,
}

pub struct Auth {
    creds: RwLock<Arc<Creds>>,
    cookie_secure: bool,
}

/// 已验证提取码后签发的短期分享票据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareTicket {
    pub share_id: i64,
    pub ticket_id: String,
    pub expires_at: i64,
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// 生成一个随机签名密钥(base64 文本)
pub fn random_secret() -> String {
    let mut raw = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut raw);
    b64(&raw)
}

impl Auth {
    /// 账号取当前生效配置;secret 优先用 settings 表里持久化的值,
    /// 没有则用环境变量给的初始值(或随机生成)并写入表中。
    ///
    /// 之所以让 DB 优先于环境变量:后台「登出所有会话」轮换密钥后若被 env 覆盖,
    /// 一重启旧 cookie 就会复活。
    pub async fn init(
        username: &str,
        password: &str,
        env_secret: Option<&str>,
        cookie_secure: bool,
        db: &Db,
    ) -> AppResult<Self> {
        let secret = match db.setting_get(SECRET_KEY).await? {
            Some(s) => s,
            None => {
                let s = env_secret.map(str::to_string).unwrap_or_else(random_secret);
                db.setting_set(SECRET_KEY, &s).await?;
                s
            }
        };
        Ok(Auth {
            creds: RwLock::new(Arc::new(Creds {
                username: username.to_string(),
                password: password.to_string(),
                secret: secret.into_bytes(),
            })),
            cookie_secure,
        })
    }

    /// guard 立即释放,不跨 await
    fn creds(&self) -> Arc<Creds> {
        self.creds.read().unwrap().clone()
    }

    pub fn username(&self) -> String {
        self.creds().username.clone()
    }

    /// 替换账号(密钥不动)
    #[cfg(test)]
    pub fn set_account(&self, username: &str, password: &str) {
        let old = self.creds();
        *self.creds.write().unwrap() = Arc::new(Creds {
            username: username.to_string(),
            password: password.to_string(),
            secret: old.secret.clone(),
        });
    }

    /// 安装已经与账号配置在同一数据库事务中持久化的新密钥。
    pub fn set_account_and_secret(&self, username: &str, password: &str, secret: &str) {
        *self.creds.write().unwrap() = Arc::new(Creds {
            username: username.to_string(),
            password: password.to_string(),
            secret: secret.as_bytes().to_vec(),
        });
    }

    /// 换一把新签名密钥并持久化:所有已签发的 cookie 立刻失效
    pub async fn rotate_secret(&self, db: &Db) -> AppResult<()> {
        let s = random_secret();
        db.setting_set(SECRET_KEY, &s).await?;
        let old = self.creds();
        *self.creds.write().unwrap() = Arc::new(Creds {
            username: old.username.clone(),
            password: old.password.clone(),
            secret: s.into_bytes(),
        });
        Ok(())
    }

    fn ct_eq(a: &str, b: &str) -> bool {
        a.as_bytes().ct_eq(b.as_bytes()).into()
    }

    /// 对提取码等短秘密做常量时间比较。
    pub fn secrets_equal(a: &str, b: &str) -> bool {
        Self::ct_eq(a, b)
    }

    pub fn verify_creds(&self, user: &str, pass: &str) -> bool {
        let c = self.creds();
        // 两项都比较,避免用户名短路泄露时序信息
        let u = Self::ct_eq(user, &c.username);
        let p = Self::ct_eq(pass, &c.password);
        u && p
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.creds().secret).expect("HMAC 任意密钥长度均可");
        mac.update(payload.as_bytes());
        b64(&mac.finalize().into_bytes())
    }

    /// token 形如 "过期时间戳.签名"
    pub fn issue_token(&self) -> String {
        let exp = crate::db::now_ts() + TOKEN_TTL_SECS;
        let payload = exp.to_string();
        format!("{payload}.{}", self.sign(&payload))
    }

    pub fn verify_token(&self, token: &str) -> bool {
        let Some((payload, sig)) = token.split_once('.') else {
            return false;
        };
        let Ok(exp) = payload.parse::<i64>() else {
            return false;
        };
        if exp < crate::db::now_ts() {
            return false;
        }
        Self::ct_eq(&self.sign(payload), sig)
    }

    /// 签发只对一个分享有效的短期票据。随机 ID 同时用于合并 Range 请求的计数。
    pub fn issue_share_ticket(&self, share_id: i64) -> (String, ShareTicket) {
        let expires_at = crate::db::now_ts() + SHARE_TICKET_TTL_SECS;
        let mut nonce = [0u8; 16];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce);
        let ticket_id = b64(&nonce);
        let payload = format!("s1:{share_id}:{expires_at}:{ticket_id}");
        let token = format!("{payload}.{}", self.sign(&payload));
        (
            token,
            ShareTicket {
                share_id,
                ticket_id,
                expires_at,
            },
        )
    }

    pub fn verify_share_ticket(&self, token: &str) -> Option<ShareTicket> {
        let (payload, sig) = token.rsplit_once('.')?;
        if !Self::ct_eq(&self.sign(payload), sig) {
            return None;
        }
        let mut parts = payload.split(':');
        if parts.next()? != "s1" {
            return None;
        }
        let share_id = parts.next()?.parse::<i64>().ok()?;
        let expires_at = parts.next()?.parse::<i64>().ok()?;
        let ticket_id = parts.next()?;
        if parts.next().is_some()
            || share_id <= 0
            || expires_at < crate::db::now_ts()
            || ticket_id.len() != 22
            || !ticket_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        Some(ShareTicket {
            share_id,
            ticket_id: ticket_id.to_string(),
            expires_at,
        })
    }

    pub fn cookie_header(&self, token: &str) -> String {
        format!(
            "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={TOKEN_TTL_SECS}{}",
            self.secure_cookie_suffix()
        )
    }

    pub fn clear_cookie_header(&self) -> String {
        format!(
            "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
            self.secure_cookie_suffix()
        )
    }

    /// 分享票据使用独立、窄路径 Cookie，不会被发往后台和其他分享。
    pub fn share_cookie_header(&self, token: &str, share_token: &str) -> String {
        format!(
            "{SHARE_COOKIE_NAME}={token}; Path=/api/share/{share_token}; HttpOnly; SameSite=Lax; Max-Age={SHARE_TICKET_TTL_SECS}{}",
            self.secure_cookie_suffix()
        )
    }

    fn secure_cookie_suffix(&self) -> &'static str {
        if self.cookie_secure { "; Secure" } else { "" }
    }

    /// 从 HttpOnly Cookie 或命令行客户端的 Bearer 头读取分享票据。
    pub fn share_ticket_from_headers(&self, headers: &http::HeaderMap) -> Option<ShareTicket> {
        if let Some(cookie) = headers
            .get(http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
        {
            for kv in cookie.split(';') {
                if let Some((k, v)) = kv.trim().split_once('=')
                    && k == SHARE_COOKIE_NAME
                    && let Some(ticket) = self.verify_share_ticket(v.trim())
                {
                    return Some(ticket);
                }
            }
        }
        let bearer = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))?;
        self.verify_share_ticket(bearer.trim())
    }

    /// 从请求头判定是否已认证(cookie 或 Basic 任一通过)
    pub fn check_headers(&self, headers: &http::HeaderMap) -> bool {
        if let Some(cookie) = headers
            .get(http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
        {
            for kv in cookie.split(';') {
                if let Some((k, v)) = kv.trim().split_once('=')
                    && k == COOKIE_NAME
                    && self.verify_token(v.trim())
                {
                    return true;
                }
            }
        }
        if let Some(auth) = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            && let Some(b64cred) = auth.strip_prefix("Basic ")
            && let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64cred.trim())
            && let Ok(s) = String::from_utf8(raw)
            && let Some((u, p)) = s.split_once(':')
        {
            return self.verify_creds(u, p);
        }
        false
    }
}

/// 登录请求体
#[derive(serde::Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

impl LoginReq {
    pub fn verify(&self, auth: &Auth) -> AppResult<()> {
        if auth.verify_creds(&self.username, &self.password) {
            Ok(())
        } else {
            Err(AppError::Unauthorized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth {
            creds: RwLock::new(Arc::new(Creds {
                username: "u".into(),
                password: "p".into(),
                secret: b"test-secret".to_vec(),
            })),
            cookie_secure: false,
        }
    }

    #[test]
    fn token_roundtrip() {
        let a = auth();
        let t = a.issue_token();
        assert!(a.verify_token(&t));
        assert!(!a.verify_token("123.bogus"));
        assert!(!a.verify_token("bogus"));
        // 过期 token
        let payload = (crate::db::now_ts() - 10).to_string();
        let expired = format!("{payload}.{}", a.sign(&payload));
        assert!(!a.verify_token(&expired));
    }

    #[test]
    fn share_ticket_is_scoped_and_tamper_proof() {
        let a = auth();
        let (raw, issued) = a.issue_share_ticket(42);
        assert_eq!(a.verify_share_ticket(&raw), Some(issued));
        assert!(a.verify_share_ticket(&(raw + "x")).is_none());
    }

    #[test]
    fn secure_cookie_can_be_enabled() {
        let mut a = auth();
        a.cookie_secure = true;
        assert!(a.cookie_header("x").ends_with("; Secure"));
        assert!(a.share_cookie_header("x", "abc").contains("; Secure"));
    }

    #[test]
    fn basic_header() {
        let a = auth();
        let mut h = http::HeaderMap::new();
        let cred = base64::engine::general_purpose::STANDARD.encode("u:p");
        h.insert(
            http::header::AUTHORIZATION,
            format!("Basic {cred}").parse().unwrap(),
        );
        assert!(a.check_headers(&h));

        let bad = base64::engine::general_purpose::STANDARD.encode("u:wrong");
        h.insert(
            http::header::AUTHORIZATION,
            format!("Basic {bad}").parse().unwrap(),
        );
        assert!(!a.check_headers(&h));
    }

    #[test]
    fn cookie_header_check() {
        let a = auth();
        let t = a.issue_token();
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::COOKIE,
            format!("x=1; {COOKIE_NAME}={t}").parse().unwrap(),
        );
        assert!(a.check_headers(&h));
    }

    #[test]
    fn account_swap_takes_effect() {
        let a = auth();
        assert!(a.verify_creds("u", "p"));
        a.set_account("u2", "p2");
        assert_eq!(a.username(), "u2");
        assert!(!a.verify_creds("u", "p")); // 旧账号立即失效
        assert!(a.verify_creds("u2", "p2"));
        // 换密钥后旧 token 失效
        let t = a.issue_token();
        assert!(a.verify_token(&t));
        *a.creds.write().unwrap() = Arc::new(Creds {
            username: "u2".into(),
            password: "p2".into(),
            secret: b"another-secret".to_vec(),
        });
        assert!(!a.verify_token(&t));
    }
}
