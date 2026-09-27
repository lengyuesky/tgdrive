//! 两步验证：RFC 6238 TOTP（SHA-1、6 位、30 秒）与一次性恢复码。
//!
//! 与 Google Authenticator、Microsoft Authenticator、1Password 等常见验证器兼容。

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

const STEP_SECS: i64 = 30;
const DIGITS: u32 = 6;
const SECRET_BYTES: usize = 20;
/// 允许前后各一个时间窗，容忍手机与服务器几十秒的时钟偏差。
const SKEW_STEPS: i64 = 1;
pub const RECOVERY_CODE_COUNT: usize = 10;
/// 恢复码字母表去掉了 0/o/1/l/i 等易混字符。
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// 生成新的 base32 密钥（无填充，160 位）。
pub fn generate_secret() -> String {
    let mut raw = [0u8; SECRET_BYTES];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut raw);
    data_encoding::BASE32_NOPAD.encode(&raw)
}

fn decode_secret(secret: &str) -> Option<Vec<u8>> {
    data_encoding::BASE32_NOPAD
        .decode(secret.trim().to_ascii_uppercase().as_bytes())
        .ok()
}

fn code_at(key: &[u8], step: u64) -> u32 {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(key).expect("HMAC 任意密钥长度均可");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    binary % 10u32.pow(DIGITS)
}

/// 校验 6 位验证码，成功时返回命中的时间步，供调用方拒绝同一验证码重放。
pub fn verify(secret: &str, code: &str, now: i64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let expected: u32 = code.parse().ok()?;
    let key = decode_secret(secret)?;
    let current = now.div_euclid(STEP_SECS);
    (-SKEW_STEPS..=SKEW_STEPS)
        .map(|delta| current + delta)
        .filter(|step| *step >= 0)
        .map(|step| step as u64)
        // 逐个比较完所有窗口，避免按命中位置泄露时序
        .fold(None, |hit, step| {
            if code_at(&key, step) == expected {
                Some(step)
            } else {
                hit
            }
        })
}

/// 验证器扫码用的 otpauth URI。
pub fn otpauth_uri(account: &str, secret: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let label = utf8_percent_encode(&format!("tgdrive:{account}"), NON_ALPHANUMERIC).to_string();
    format!(
        "otpauth://totp/{label}?secret={secret}&issuer=tgdrive&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}"
    )
}

/// 渲染二维码 SVG，前端直接内联显示，密钥不经过任何第三方服务。
pub fn qr_svg(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build(),
    )
}

/// 生成一组恢复码（形如 `abcd-efgh`），返回明文供展示一次。
pub fn generate_recovery_codes() -> Vec<String> {
    let mut rng = rand::rng();
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let chars: String = (0..8)
                .map(|_| {
                    let index = rand::RngExt::random_range(&mut rng, 0..RECOVERY_ALPHABET.len());
                    RECOVERY_ALPHABET[index] as char
                })
                .collect();
            format!("{}-{}", &chars[..4], &chars[4..])
        })
        .collect()
}

/// 恢复码的存储形式。码本身有 40 位随机熵且登录受限流保护，SHA-256 足够。
pub fn hash_recovery_code(code: &str) -> String {
    let normalized: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 看起来像恢复码（而不是 6 位数字验证码）。
pub fn looks_like_recovery_code(code: &str) -> bool {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).count() == 8
        && code.chars().any(|c| c.is_ascii_alphabetic())
}

/// 测试用:计算某时刻的验证码
#[cfg(test)]
pub fn code_now(secret: &str, now: i64) -> String {
    let key = decode_secret(secret).expect("测试密钥有效");
    format!("{:06}", code_at(&key, now.div_euclid(STEP_SECS) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 附录 B 的 SHA-1 测试向量（密钥 "12345678901234567890"），取低 6 位。
    #[test]
    fn rfc6238_vectors() {
        let key = b"12345678901234567890";
        for (time, expected) in [
            (59_i64, 287_082_u32),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
        ] {
            assert_eq!(
                code_at(key, (time / STEP_SECS) as u64),
                expected,
                "t={time}"
            );
        }
    }

    #[test]
    fn verify_accepts_adjacent_window_only() {
        let secret = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
        assert_eq!(verify(&secret, "287082", 59), Some(1));
        assert_eq!(verify(&secret, "287 082", 59 + 30), Some(1));
        assert_eq!(verify(&secret, "287082", 59 + 90), None);
        assert_eq!(verify(&secret, "28708", 59), None);
        assert_eq!(verify(&secret, "abcdef", 59), None);
    }

    #[test]
    fn generated_secret_and_recovery_codes() {
        let secret = generate_secret();
        assert_eq!(decode_secret(&secret).unwrap().len(), SECRET_BYTES);
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        assert!(codes.iter().all(|c| looks_like_recovery_code(c)));
        assert_eq!(
            hash_recovery_code(&codes[0]),
            hash_recovery_code(&codes[0].to_uppercase().replace('-', " "))
        );
        assert!(!looks_like_recovery_code("123456"));
        assert!(otpauth_uri("admin", &secret).starts_with("otpauth://totp/tgdrive%3Aadmin?"));
        assert!(qr_svg("otpauth://totp/x").unwrap().contains("<svg"));
    }
}
