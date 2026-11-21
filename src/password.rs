//! 登录密码的存储形式：argon2id PHC 字符串（`$argon2id$v=19$...`）。
//!
//! 数据库里只存哈希；`.env` 的 `TGDRIVE_PASSWORD` 为兼容起见仍可写明文，
//! 也可以直接写 `tgdrive hash-password` 生成的哈希。

use crate::error::{AppError, AppResult};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use subtle::ConstantTimeEq;

/// 是否已是 argon2 PHC 哈希；其余一律按明文对待。
pub fn is_hash(stored: &str) -> bool {
    stored.starts_with("$argon2")
}

/// 生成 argon2id 哈希（默认参数：19 MiB 内存、2 轮），调用方应放在阻塞线程里执行。
pub fn hash(plain: &str) -> AppResult<String> {
    Argon2::default()
        .hash_password(plain.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::Internal(anyhow::anyhow!("密码哈希失败：{error}")))
}

/// 在阻塞线程里生成哈希，避免 argon2 的几十毫秒 CPU 时间卡住异步 worker。
pub async fn hash_blocking(plain: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || hash(&plain))
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("密码哈希任务失败：{error}")))?
}

/// 校验候选密码。存储值是哈希时走 argon2，明文（仅来自 `.env`）时做常量时间比较。
pub fn verify(stored: &str, candidate: &str) -> bool {
    if is_hash(stored) {
        match PasswordHash::new(stored) {
            Ok(parsed) => Argon2::default()
                .verify_password(candidate.as_bytes(), &parsed)
                .is_ok(),
            Err(error) => {
                tracing::error!("已保存的密码哈希格式无效：{error}");
                false
            }
        }
    } else {
        stored.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip_and_plaintext_fallback() {
        let stored = hash("hunter2").unwrap();
        assert!(is_hash(&stored));
        assert!(verify(&stored, "hunter2"));
        assert!(!verify(&stored, "hunter3"));
        // 同一密码每次加盐不同
        assert_ne!(stored, hash("hunter2").unwrap());

        assert!(!is_hash("plain"));
        assert!(verify("plain", "plain"));
        assert!(!verify("plain", "plain2"));
        assert!(!verify("$argon2id$broken", "x"));
    }
}
