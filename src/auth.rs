//! 认证:单用户。Web 端 HMAC 签名 cookie,WebDAV 端 HTTP Basic,中间件双通道放行。
//!
//! 账号与签名密钥可在运行时替换(后台改配置),已被克隆持有的 `Arc<Auth>` 立即生效。
//! 启用两步验证后,网页登录需要额外的 TOTP 验证码;Basic 通道不再接受主密码,
//! 只接受单独生成的 WebDAV 应用密码,避免借 WebDAV/API 绕过两步验证。

use crate::db::Db;
use crate::error::{AppError, AppResult};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::sync::{Arc, Mutex, RwLock};
use subtle::ConstantTimeEq;

#[path = "apps/auth.rs"]
mod app_tokens;

const COOKIE_NAME: &str = "tgdrive_token";
const TOKEN_TTL_SECS: i64 = 7 * 24 * 3600;
const SHARE_COOKIE_NAME: &str = "tgdrive_share";
const SHARE_TICKET_TTL_SECS: i64 = 30 * 60;
/// settings 表里存签名密钥的键
pub const SECRET_KEY: &str = "secret";
/// 两步验证的 base32 密钥;存在即视为已启用
pub const TOTP_SECRET_KEY: &str = "auth.totp_secret";
/// 剩余恢复码的 SHA-256(JSON 数组)
pub const TOTP_RECOVERY_KEY: &str = "auth.totp_recovery";
/// WebDAV 应用密码的 SHA-256
pub const WEBDAV_PASSWORD_KEY: &str = "auth.webdav_password";
/// 扫码后需在这段时间内输入验证码完成绑定
const TOTP_SETUP_TTL_SECS: i64 = 10 * 60;
/// Basic 认证成功结果的缓存条数(每个客户端一条,够用即可)
const BASIC_CACHE_CAP: usize = 8;

type HmacSha256 = Hmac<Sha256>;

struct Creds {
    username: String,
    /// argon2 哈希,或来自 `.env` 的明文
    password: String,
    secret: Vec<u8>,
    /// 已验证过的 Basic 凭据指纹。WebDAV 客户端每个请求都带密码,
    /// 不缓存的话每次都要跑一遍 argon2。账号或密钥变化时随 `Creds` 一起作废。
    basic_verified: Mutex<Vec<[u8; 32]>>,
}

impl Creds {
    fn new(username: &str, password: &str, secret: Vec<u8>) -> Self {
        Creds {
            username: username.to_string(),
            password: password.to_string(),
            secret,
            basic_verified: Mutex::new(Vec::new()),
        }
    }
}

/// 两步验证与 WebDAV 应用密码的内存副本,修改时先写库再更新这里。
#[derive(Default, Clone)]
struct SecondFactor {
    totp_secret: Option<String>,
    recovery: Vec<String>,
    webdav_password: Option<String>,
    /// 最近一次成功使用的时间步,同一验证码不能用两次
    last_step: Option<u64>,
}

/// 后台「安全」页展示的状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityStatus {
    pub totp_enabled: bool,
    pub recovery_remaining: usize,
    pub webdav_password_set: bool,
}

/// 开始绑定时返回给前端的内容
#[derive(Debug, Clone, serde::Serialize)]
pub struct TotpSetup {
    pub secret: String,
    pub uri: String,
    pub qr_svg: String,
}

pub struct Auth {
    creds: RwLock<Arc<Creds>>,
    factor: RwLock<SecondFactor>,
    /// 串行化两步验证状态的修改,避免并发请求重复消费同一个恢复码
    factor_update: tokio::sync::Mutex<()>,
    pending_totp: Mutex<Option<(String, i64)>>,
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
        let values = db
            .settings_get(&[TOTP_SECRET_KEY, TOTP_RECOVERY_KEY, WEBDAV_PASSWORD_KEY])
            .await?;
        let recovery = values
            .get(TOTP_RECOVERY_KEY)
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default();
        Ok(Auth {
            creds: RwLock::new(Arc::new(Creds::new(
                username,
                password,
                secret.into_bytes(),
            ))),
            factor: RwLock::new(SecondFactor {
                totp_secret: values.get(TOTP_SECRET_KEY).cloned(),
                recovery,
                webdav_password: values.get(WEBDAV_PASSWORD_KEY).cloned(),
                last_step: None,
            }),
            factor_update: tokio::sync::Mutex::new(()),
            pending_totp: Mutex::new(None),
            cookie_secure,
        })
    }

    #[cfg(test)]
    fn for_test(username: &str, password: &str) -> Self {
        Auth {
            creds: RwLock::new(Arc::new(Creds::new(
                username,
                password,
                b"test-secret".to_vec(),
            ))),
            factor: RwLock::new(SecondFactor::default()),
            factor_update: tokio::sync::Mutex::new(()),
            pending_totp: Mutex::new(None),
            cookie_secure: false,
        }
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
        *self.creds.write().unwrap() = Arc::new(Creds::new(username, password, old.secret.clone()));
    }

    /// 安装已经与账号配置在同一数据库事务中持久化的新密钥。
    /// `password` 是存储形式(哈希或 `.env` 明文)。
    pub fn set_account_and_secret(&self, username: &str, password: &str, secret: &str) {
        *self.creds.write().unwrap() =
            Arc::new(Creds::new(username, password, secret.as_bytes().to_vec()));
    }

    /// 换一把新签名密钥并持久化:所有已签发的 cookie 立刻失效
    pub async fn rotate_secret(&self, db: &Db) -> AppResult<()> {
        let s = random_secret();
        db.setting_set(SECRET_KEY, &s).await?;
        let old = self.creds();
        *self.creds.write().unwrap() =
            Arc::new(Creds::new(&old.username, &old.password, s.into_bytes()));
        Ok(())
    }

    fn ct_eq(a: &str, b: &str) -> bool {
        a.as_bytes().ct_eq(b.as_bytes()).into()
    }

    /// 对提取码等短秘密做常量时间比较。
    pub fn secrets_equal(a: &str, b: &str) -> bool {
        Self::ct_eq(a, b)
    }

    /// 校验网页登录的用户名与主密码。可能要跑 argon2,异步上下文应放进阻塞线程。
    pub fn verify_login_password(&self, user: &str, pass: &str) -> bool {
        let c = self.creds();
        // 两项都比较,避免用户名短路泄露时序信息
        let u = Self::ct_eq(user, &c.username);
        let p = crate::password::verify(&c.password, pass);
        u && p
    }

    /// 校验 Basic 凭据(WebDAV 与命令行 API)。应用密码始终可用;
    /// 主密码只在未启用两步验证时接受。
    pub fn verify_basic(&self, user: &str, pass: &str) -> bool {
        let c = self.creds();
        if !Self::ct_eq(user, &c.username) {
            return false;
        }
        let factor = self.factor.read().unwrap().clone();
        if let Some(stored) = &factor.webdav_password
            && Self::ct_eq(&sha256_hex(pass), stored)
        {
            return true;
        }
        if factor.totp_secret.is_some() {
            return false;
        }
        let fingerprint = {
            let mut mac = HmacSha256::new_from_slice(&c.secret).expect("HMAC 任意密钥长度均可");
            mac.update(b"basic-cache\0");
            mac.update(user.as_bytes());
            mac.update(b"\0");
            mac.update(pass.as_bytes());
            let out: [u8; 32] = mac.finalize().into_bytes().into();
            out
        };
        if c.basic_verified
            .lock()
            .unwrap()
            .iter()
            .any(|known| bool::from(known.ct_eq(&fingerprint)))
        {
            return true;
        }
        if !crate::password::verify(&c.password, pass) {
            return false;
        }
        let mut cache = c.basic_verified.lock().unwrap();
        if cache.len() >= BASIC_CACHE_CAP {
            cache.remove(0);
        }
        cache.push(fingerprint);
        true
    }

    pub fn totp_enabled(&self) -> bool {
        self.factor.read().unwrap().totp_secret.is_some()
    }

    pub fn security_status(&self) -> SecurityStatus {
        let factor = self.factor.read().unwrap();
        SecurityStatus {
            totp_enabled: factor.totp_secret.is_some(),
            recovery_remaining: factor.recovery.len(),
            webdav_password_set: factor.webdav_password.is_some(),
        }
    }

    /// 校验两步验证码或恢复码。恢复码用后即焚;TOTP 同一时间步只能用一次。
    /// 未启用两步验证时返回 `false`。
    pub async fn check_second_factor(&self, code: &str, db: &Db) -> AppResult<bool> {
        let _guard = self.factor_update.lock().await;
        let factor = self.factor.read().unwrap().clone();
        let Some(secret) = factor.totp_secret else {
            return Ok(false);
        };
        if crate::totp::looks_like_recovery_code(code) {
            let hashed = crate::totp::hash_recovery_code(code);
            let Some(index) = factor
                .recovery
                .iter()
                .position(|known| Self::ct_eq(known, &hashed))
            else {
                return Ok(false);
            };
            let mut remaining = factor.recovery.clone();
            remaining.remove(index);
            db.setting_set(TOTP_RECOVERY_KEY, &json_list(&remaining))
                .await?;
            self.factor.write().unwrap().recovery = remaining;
            return Ok(true);
        }
        let Some(step) = crate::totp::verify(&secret, code, crate::db::now_ts()) else {
            return Ok(false);
        };
        if factor.last_step.is_some_and(|last| step <= last) {
            return Ok(false);
        }
        self.factor.write().unwrap().last_step = Some(step);
        Ok(true)
    }

    /// 生成待绑定的新密钥。确认验证码之前不影响现有登录方式。
    pub fn begin_totp_setup(&self) -> TotpSetup {
        let secret = crate::totp::generate_secret();
        let uri = crate::totp::otpauth_uri(&self.username(), &secret);
        let qr_svg = crate::totp::qr_svg(&uri).unwrap_or_default();
        *self.pending_totp.lock().unwrap() = Some((secret.clone(), crate::db::now_ts()));
        TotpSetup {
            secret,
            uri,
            qr_svg,
        }
    }

    /// 用验证器上的验证码确认绑定,返回一组新恢复码(只展示这一次)。
    pub async fn enable_totp(&self, code: &str, db: &Db) -> AppResult<Vec<String>> {
        let _guard = self.factor_update.lock().await;
        let pending = self.pending_totp.lock().unwrap().clone();
        let Some((secret, _)) =
            pending.filter(|(_, created)| crate::db::now_ts() - created <= TOTP_SETUP_TTL_SECS)
        else {
            return Err(AppError::BadRequest("绑定已过期，请重新生成二维码".into()));
        };
        let Some(step) = crate::totp::verify(&secret, code, crate::db::now_ts()) else {
            return Err(AppError::BadRequest(
                "验证码不正确，请确认手机时间准确后重试".into(),
            ));
        };
        let codes = crate::totp::generate_recovery_codes();
        let hashed: Vec<String> = codes
            .iter()
            .map(|code| crate::totp::hash_recovery_code(code))
            .collect();
        db.settings_apply(
            &[
                (TOTP_SECRET_KEY.to_string(), secret.clone()),
                (TOTP_RECOVERY_KEY.to_string(), json_list(&hashed)),
            ],
            &[],
        )
        .await?;
        {
            let mut factor = self.factor.write().unwrap();
            factor.totp_secret = Some(secret);
            factor.recovery = hashed;
            factor.last_step = Some(step);
        }
        *self.pending_totp.lock().unwrap() = None;
        Ok(codes)
    }

    /// 关闭两步验证(调用方已校验过验证码)。
    pub async fn disable_totp(&self, db: &Db) -> AppResult<()> {
        let _guard = self.factor_update.lock().await;
        db.settings_apply(
            &[],
            &[TOTP_SECRET_KEY.to_string(), TOTP_RECOVERY_KEY.to_string()],
        )
        .await?;
        let mut factor = self.factor.write().unwrap();
        factor.totp_secret = None;
        factor.recovery.clear();
        factor.last_step = None;
        Ok(())
    }

    /// 重新生成恢复码,旧的全部作废(调用方已校验过验证码)。
    pub async fn regenerate_recovery_codes(&self, db: &Db) -> AppResult<Vec<String>> {
        let _guard = self.factor_update.lock().await;
        if !self.totp_enabled() {
            return Err(AppError::BadRequest("尚未启用两步验证".into()));
        }
        let codes = crate::totp::generate_recovery_codes();
        let hashed: Vec<String> = codes
            .iter()
            .map(|code| crate::totp::hash_recovery_code(code))
            .collect();
        db.setting_set(TOTP_RECOVERY_KEY, &json_list(&hashed))
            .await?;
        self.factor.write().unwrap().recovery = hashed;
        Ok(codes)
    }

    /// 生成新的 WebDAV 应用密码并替换旧的,明文只返回这一次。
    pub async fn generate_webdav_password(&self, db: &Db) -> AppResult<String> {
        let _guard = self.factor_update.lock().await;
        let mut raw = [0u8; 18];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut raw);
        let password = b64(&raw);
        let hashed = sha256_hex(&password);
        db.setting_set(WEBDAV_PASSWORD_KEY, &hashed).await?;
        self.factor.write().unwrap().webdav_password = Some(hashed);
        Ok(password)
    }

    pub async fn revoke_webdav_password(&self, db: &Db) -> AppResult<()> {
        let _guard = self.factor_update.lock().await;
        db.setting_del(WEBDAV_PASSWORD_KEY).await?;
        self.factor.write().unwrap().webdav_password = None;
        Ok(())
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

    /// 请求是否携带有效的登录 Cookie
    pub fn session_cookie_valid(&self, headers: &http::HeaderMap) -> bool {
        let Some(cookie) = headers
            .get(http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        cookie.split(';').any(|kv| {
            kv.trim()
                .split_once('=')
                .is_some_and(|(k, v)| k == COOKIE_NAME && self.verify_token(v.trim()))
        })
    }
}

/// 解析 `Authorization: Basic` 凭据;格式不合法视同未提供。
pub fn basic_credentials(headers: &http::HeaderMap) -> Option<(String, String)> {
    let auth = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(auth.strip_prefix("Basic ")?.trim())
        .ok()?;
    let (user, pass) = String::from_utf8(raw)
        .ok()?
        .split_once(':')
        .map(|(u, p)| (u.to_string(), p.to_string()))?;
    Some((user, pass))
}

fn sha256_hex(value: &str) -> String {
    use sha2::Digest;
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn json_list(items: &[String]) -> String {
    serde_json::to_string(items).expect("字符串数组总能序列化")
}

/// 登录请求体
#[derive(serde::Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
    /// 两步验证码或恢复码;未启用两步验证时忽略
    #[serde(default)]
    pub totp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::for_test("u", "p")
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
        let (u, p) = basic_credentials(&h).unwrap();
        assert!(a.verify_basic(&u, &p));

        let bad = base64::engine::general_purpose::STANDARD.encode("u:wrong");
        h.insert(
            http::header::AUTHORIZATION,
            format!("Basic {bad}").parse().unwrap(),
        );
        let (u, p) = basic_credentials(&h).unwrap();
        assert!(!a.verify_basic(&u, &p));

        h.insert(http::header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert!(basic_credentials(&h).is_none());
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
        assert!(a.session_cookie_valid(&h));
        h.insert(
            http::header::COOKIE,
            format!("{COOKIE_NAME}=123.bogus").parse().unwrap(),
        );
        assert!(!a.session_cookie_valid(&h));
    }

    #[test]
    fn account_swap_takes_effect() {
        let a = auth();
        assert!(a.verify_login_password("u", "p"));
        a.set_account("u2", "p2");
        assert_eq!(a.username(), "u2");
        assert!(!a.verify_login_password("u", "p")); // 旧账号立即失效
        assert!(a.verify_login_password("u2", "p2"));
        // 换密钥后旧 token 失效
        let t = a.issue_token();
        assert!(a.verify_token(&t));
        *a.creds.write().unwrap() = Arc::new(Creds::new("u2", "p2", b"another-secret".to_vec()));
        assert!(!a.verify_token(&t));
    }

    #[test]
    fn hashed_password_and_basic_cache() {
        let hashed = crate::password::hash("secret-pass").unwrap();
        let a = Auth::for_test("u", &hashed);
        assert!(a.verify_login_password("u", "secret-pass"));
        assert!(!a.verify_login_password("u", "wrong"));
        assert!(a.verify_basic("u", "secret-pass"));
        assert_eq!(a.creds().basic_verified.lock().unwrap().len(), 1);
        // 命中缓存后仍然区分密码
        assert!(a.verify_basic("u", "secret-pass"));
        assert!(!a.verify_basic("u", "wrong"));
        assert!(!a.verify_basic("x", "secret-pass"));
        // 换账号后缓存随旧凭据作废
        a.set_account("u", &crate::password::hash("new-pass").unwrap());
        assert!(!a.verify_basic("u", "secret-pass"));
    }

    #[tokio::test]
    async fn totp_blocks_main_password_on_basic_and_rejects_replay() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(dir.path().join("auth.db").to_str().unwrap())
            .await
            .unwrap();
        let a = Auth::for_test("u", "p");
        let setup = a.begin_totp_setup();
        assert!(setup.uri.contains(&setup.secret));
        assert!(!a.totp_enabled());
        assert!(a.enable_totp("000000x", &db).await.is_err());

        let code = current_code(&setup.secret);
        let recovery = a.enable_totp(&code, &db).await.unwrap();
        assert_eq!(recovery.len(), crate::totp::RECOVERY_CODE_COUNT);
        assert!(a.totp_enabled());
        // 绑定时用过的验证码不能再用于登录
        assert!(!a.check_second_factor(&code, &db).await.unwrap());
        // 主密码不再能走 Basic,应用密码可以
        assert!(!a.verify_basic("u", "p"));
        let app_password = a.generate_webdav_password(&db).await.unwrap();
        assert!(a.verify_basic("u", &app_password));
        assert!(!a.verify_basic("other", &app_password));
        // 恢复码一次性
        assert!(a.check_second_factor(&recovery[0], &db).await.unwrap());
        assert!(!a.check_second_factor(&recovery[0], &db).await.unwrap());
        assert_eq!(a.security_status().recovery_remaining, 9);

        // 重启后从数据库恢复同样的状态
        let reloaded = Auth::init("u", "p", None, false, &db).await.unwrap();
        assert!(reloaded.totp_enabled());
        assert_eq!(reloaded.security_status().recovery_remaining, 9);
        assert!(reloaded.verify_basic("u", &app_password));

        a.disable_totp(&db).await.unwrap();
        a.revoke_webdav_password(&db).await.unwrap();
        assert!(a.verify_basic("u", "p"));
        assert!(!a.verify_basic("u", &app_password));
    }

    fn current_code(secret: &str) -> String {
        crate::totp::code_now(secret, crate::db::now_ts())
    }
}
