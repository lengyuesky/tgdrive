//! 配置:全部来自环境变量(容器友好),启动时一次性解析并校验

use std::net::SocketAddr;
use std::path::PathBuf;

pub const OFFICIAL_API_BASE: &str = "https://api.telegram.org";
/// 官方 getFile 下载上限 20MB,留安全余量后允许的最大分块
pub const OFFICIAL_CHUNK_MAX: u64 = 19_500_000;
/// 官方端点默认分块 18MB
pub const OFFICIAL_CHUNK_DEFAULT: u64 = 18 * 1024 * 1024;
/// 自建端点单文件上限 2000MB,留 100MB 余量
pub const SELF_HOSTED_CHUNK_MAX: u64 = 1_900 * 1024 * 1024;
/// 自建端点默认使用 256MiB。接近 2GiB 的单次 multipart 请求可能持续数分钟，
/// 并发时更容易因连接中断进入“结果不确定”状态，导致整个大分块无法安全重试。
pub const LOCAL_CHUNK_DEFAULT: u64 = 256 * 1024 * 1024;
pub const MIN_CHUNK: u64 = 1024 * 1024;
pub const DEFAULT_LOCAL_DATA_ROOT: &str = "/var/lib/telegram-bot-api";

#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub bot_token: String,
    /// 仅用于上传分流的额外 Bot；主 Bot 仍负责收件箱与数据库备份。
    pub upload_bot_tokens: Vec<String>,
    /// 存储目标聊天:私有频道 -100xxxxxxxx 或 @channelname
    pub chat_id: String,
    /// Bot API 基地址,官方或自部署 telegram-bot-api
    pub api_base: String,
    pub chunk_size: u64,
    pub upload_concurrency: usize,
    pub db_path: String,
    pub tmp_dir: PathBuf,
    /// 自建 Bot API `--local` 返回的绝对路径只能从此目录下读取。
    pub local_data_root: PathBuf,
    pub username: String,
    pub password: String,
    /// cookie 签名密钥;为空则首次启动生成并存入 settings 表
    pub secret: Option<String>,
    /// 由 HTTPS 提供服务时给认证 Cookie 加上 Secure 属性。
    pub cookie_secure: bool,
    /// Telegram 应用凭据：tgdrive 用于 MTProto Range 下载，compose 中的
    /// telegram-bot-api 服务也会读取；改动后需重启/重建两个服务。
    pub telegram_api_id: String,
    pub telegram_api_hash: String,
    /// 单块以内的视频文件改用 sendVideo 发送,在 Telegram 里可直接播放
    pub send_video: bool,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_required(key: &str) -> anyhow::Result<String> {
    env_opt(key).ok_or_else(|| anyhow::anyhow!("缺少必填环境变量 {key}"))
}

/// 多 Bot 配置支持逗号、分号和换行分隔，并自动去重、忽略空项。
pub fn parse_bot_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for token in value.split([',', ';', '\n', '\r']) {
        let token = token.trim();
        if !token.is_empty() && !tokens.iter().any(|existing| existing == token) {
            tokens.push(token.to_string());
        }
    }
    tokens
}

/// 支持 "18MB" / "512m" / "19500000" 等写法
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(p) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
        (p, 1024 * 1024 * 1024)
    } else if let Some(p) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
        (p, 1024 * 1024)
    } else if let Some(p) = s.strip_suffix("kb").or_else(|| s.strip_suffix('k')) {
        (p, 1024)
    } else if let Some(p) = s.strip_suffix('b') {
        (p, 1)
    } else {
        (s.as_str(), 1)
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("无法解析大小: {s}"))?;
    Ok((n * mul as f64) as u64)
}

/// 端点是否为官方 Bot API(官方有 20MB 下载上限,分块规则不同)
pub fn is_official(api_base: &str) -> bool {
    api_base.trim_end_matches('/') == OFFICIAL_API_BASE
}

/// 端点对应的默认分块大小
pub fn default_chunk_size(api_base: &str) -> u64 {
    if is_official(api_base) {
        OFFICIAL_CHUNK_DEFAULT
    } else {
        LOCAL_CHUNK_DEFAULT
    }
}

/// 视频消息模式的默认值:自建端点默认开(分块大、多数视频单块放得下),
/// 官方端点默认关(18MB 以内的视频才有意义)
pub fn default_send_video(api_base: &str) -> bool {
    !is_official(api_base)
}

/// 解析布尔开关,支持 true/false、1/0、yes/no、on/off
pub fn parse_bool(s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("无法解析布尔值: {other}(可用 true/false)"),
    }
}

/// 分块大小校验;启动解析与后台改配置共用同一套规则
pub fn validate_chunk_size(api_base: &str, chunk_size: u64) -> anyhow::Result<()> {
    if is_official(api_base) && chunk_size > OFFICIAL_CHUNK_MAX {
        anyhow::bail!(
            "官方 API 端点的 getFile 下载上限为 20MB,分块大小不得超过 {} 字节(当前 {});\
             请调小分块或改用自部署 telegram-bot-api",
            OFFICIAL_CHUNK_MAX,
            chunk_size
        );
    }
    if chunk_size < MIN_CHUNK {
        anyhow::bail!("分块大小不得小于 1MB(当前 {chunk_size})");
    }
    if chunk_size > SELF_HOSTED_CHUNK_MAX {
        anyhow::bail!("分块大小不得超过 1900MB(Bot API 单文件上限 2000MB)");
    }
    Ok(())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_base = env_opt("TGDRIVE_API_BASE")
            .unwrap_or_else(|| OFFICIAL_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string();

        let chunk_size = match env_opt("TGDRIVE_CHUNK_SIZE") {
            Some(v) => parse_size(&v)?,
            None => default_chunk_size(&api_base),
        };
        validate_chunk_size(&api_base, chunk_size)
            .map_err(|e| anyhow::anyhow!("TGDRIVE_CHUNK_SIZE 无效: {e}"))?;

        let send_video = match env_opt("TGDRIVE_SEND_VIDEO") {
            Some(v) => {
                parse_bool(&v).map_err(|e| anyhow::anyhow!("TGDRIVE_SEND_VIDEO 无效: {e}"))?
            }
            None => default_send_video(&api_base),
        };

        let cfg = Config {
            listen: env_opt("TGDRIVE_LISTEN")
                .unwrap_or_else(|| "0.0.0.0:8080".into())
                .parse()
                .map_err(|e| anyhow::anyhow!("TGDRIVE_LISTEN 无效: {e}"))?,
            // Telegram 相关都允许留空:服务照常启动,登录后在网页「系统设置」里配
            bot_token: env_opt("TGDRIVE_BOT_TOKEN").unwrap_or_default(),
            upload_bot_tokens: env_opt("TGDRIVE_UPLOAD_BOT_TOKENS")
                .map(|value| parse_bot_tokens(&value))
                .unwrap_or_default(),
            chat_id: env_opt("TGDRIVE_CHAT_ID").unwrap_or_default(),
            api_base,
            chunk_size,
            upload_concurrency: env_opt("TGDRIVE_UPLOAD_CONCURRENCY")
                .map(|v| v.parse::<usize>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("TGDRIVE_UPLOAD_CONCURRENCY 无效: {e}"))?
                .unwrap_or(2)
                .clamp(1, 16),
            db_path: env_opt("TGDRIVE_DB_PATH").unwrap_or_else(|| "data/tgdrive.db".into()),
            tmp_dir: env_opt("TGDRIVE_TMP_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
            local_data_root: env_opt("TGDRIVE_LOCAL_DATA_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCAL_DATA_ROOT)),
            // 账号是唯一必填项:没有它就没法登录后台去配其余部分
            username: env_required("TGDRIVE_USERNAME")?,
            password: env_required("TGDRIVE_PASSWORD")?,
            secret: env_opt("TGDRIVE_SECRET"),
            cookie_secure: env_opt("TGDRIVE_COOKIE_SECURE")
                .map(|v| parse_bool(&v))
                .transpose()
                .map_err(|e| anyhow::anyhow!("TGDRIVE_COOKIE_SECURE 无效: {e}"))?
                .unwrap_or(false),
            telegram_api_id: env_opt("TELEGRAM_API_ID").unwrap_or_default(),
            telegram_api_hash: env_opt("TELEGRAM_API_HASH").unwrap_or_default(),
            send_video,
        };
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("18MB").unwrap(), 18 * 1024 * 1024);
        assert_eq!(parse_size("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(
            parse_size("1.5g").unwrap(),
            (1.5 * 1024.0 * 1024.0 * 1024.0) as u64
        );
        assert_eq!(parse_size("19500000").unwrap(), 19_500_000);
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn test_parse_bool() {
        assert!(parse_bool("true").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(parse_bool("ON").unwrap());
        assert!(!parse_bool("false").unwrap());
        assert!(!parse_bool("off").unwrap());
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn test_default_send_video() {
        assert!(!default_send_video(OFFICIAL_API_BASE));
        assert!(default_send_video("http://telegram-bot-api:8081"));
    }

    #[test]
    fn bot_tokens_accept_multiple_separators_and_deduplicate() {
        assert_eq!(
            parse_bot_tokens("a,b\nc; b\r\n d"),
            vec!["a", "b", "c", "d"]
        );
    }
}
