//! 配置中心:哪些配置能在运行时改、怎么持久化、怎么热生效。
//!
//! 优先级 **settings 表 > 环境变量 > 内置默认**。后台保存即写表并推给各个热更句柄
//! (`SwitchableTg` / `UploadKnobs` / `Auth`),无需重启容器。
//!
//! 监听地址、数据库路径等启动期配置不在此列(见 `Runtime::readonly_items`)。

use crate::auth::Auth;
use crate::config::{self, Config};
use crate::db::Db;
use crate::engine::upload::UploadKnobs;
use crate::error::{AppError, AppResult};
use crate::telegram::{SwitchableTg, TgApi, TgClient};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// settings 表里配置项的键前缀(与 auth 的 `secret` 键区分开)
const P: &str = "cfg.";
/// getMe 结果缓存时长
const BOT_NAME_TTL: Duration = Duration::from_secs(60);

/// 可运行时修改的项:(键名, 对应环境变量名)。键名同时用作 REST 字段名。
pub const KEYS: &[(&str, &str)] = &[
    ("api_base", "TGDRIVE_API_BASE"),
    ("bot_token", "TGDRIVE_BOT_TOKEN"),
    ("chat_id", "TGDRIVE_CHAT_ID"),
    ("chunk_size", "TGDRIVE_CHUNK_SIZE"),
    ("upload_concurrency", "TGDRIVE_UPLOAD_CONCURRENCY"),
    ("send_video", "TGDRIVE_SEND_VIDEO"),
    ("tmp_dir", "TGDRIVE_TMP_DIR"),
    ("username", "TGDRIVE_USERNAME"),
    ("password", "TGDRIVE_PASSWORD"),
    ("telegram_api_id", "TELEGRAM_API_ID"),
    ("telegram_api_hash", "TELEGRAM_API_HASH"),
];

/// 这两项由 compose 里的 telegram-bot-api 容器读取,tgdrive 只负责保管与导出;
/// 改动要重建那个容器才生效(compose 的 `${VAR}` 插值取自宿主机 .env 文件)。
pub const LOCAL_API_KEYS: &[&str] = &["telegram_api_id", "telegram_api_hash"];

/// 当前生效的可变配置
#[derive(Clone, Debug)]
pub struct RuntimeCfg {
    pub api_base: String,
    pub bot_token: String,
    pub chat_id: String,
    pub chunk_size: u64,
    pub upload_concurrency: usize,
    /// 单块以内的视频改用 sendVideo 发送,在 Telegram 里可直接播放
    pub send_video: bool,
    pub tmp_dir: PathBuf,
    pub username: String,
    pub password: String,
    pub telegram_api_id: String,
    pub telegram_api_hash: String,
}

impl RuntimeCfg {
    /// 用 settings 表里的值覆盖环境变量层
    pub async fn load(db: &Db, env: &Config) -> AppResult<Self> {
        let db_vals = db.settings_with_prefix(P).await?;
        let get = |k: &str| db_vals.get(k).map(String::as_str);

        let api_base = get("api_base")
            .map(|v| v.trim_end_matches('/').to_string())
            .unwrap_or_else(|| env.api_base.clone());

        // 分块:表里有就用表里的;否则 env 显式设过就用 env 的,再否则按端点取默认
        let env_set_chunk = env_present("TGDRIVE_CHUNK_SIZE");
        let mut chunk_size = match get("chunk_size").and_then(|v| v.parse::<u64>().ok()) {
            Some(v) => v,
            None if env_set_chunk => env.chunk_size,
            None => config::default_chunk_size(&api_base),
        };
        // 启动时宽容处理:端点被改小了而分块没跟着改,回落到该端点的默认值而不是拒绝启动
        if let Err(e) = config::validate_chunk_size(&api_base, chunk_size) {
            let fallback = config::default_chunk_size(&api_base);
            tracing::warn!("分块大小与当前端点不匹配({e}),暂用默认值 {fallback}");
            chunk_size = fallback;
        }

        // 视频消息模式:同分块一样,默认值跟随实际生效的端点(自建默认开)
        let send_video = match get("send_video").and_then(|v| config::parse_bool(v).ok()) {
            Some(v) => v,
            None if env_present("TGDRIVE_SEND_VIDEO") => env.send_video,
            None => config::default_send_video(&api_base),
        };

        Ok(RuntimeCfg {
            api_base,
            bot_token: get("bot_token").map(str::to_string).unwrap_or_else(|| env.bot_token.clone()),
            chat_id: get("chat_id").map(str::to_string).unwrap_or_else(|| env.chat_id.clone()),
            chunk_size,
            upload_concurrency: get("upload_concurrency")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(env.upload_concurrency)
                .clamp(1, 16),
            send_video,
            tmp_dir: get("tmp_dir").map(PathBuf::from).unwrap_or_else(|| env.tmp_dir.clone()),
            username: get("username").map(str::to_string).unwrap_or_else(|| env.username.clone()),
            password: get("password").map(str::to_string).unwrap_or_else(|| env.password.clone()),
            telegram_api_id: get("telegram_api_id")
                .map(str::to_string)
                .unwrap_or_else(|| env.telegram_api_id.clone()),
            telegram_api_hash: get("telegram_api_hash")
                .map(str::to_string)
                .unwrap_or_else(|| env.telegram_api_hash.clone()),
        })
    }

    /// Telegram 存储是否已配置好(没配时上传下载会给出提示而不是报 401)
    pub fn is_configured(&self) -> bool {
        !self.bot_token.trim().is_empty() && !self.chat_id.trim().is_empty()
    }

    /// 按当前配置造一个 TG 客户端;没配好就返回占位实现
    pub fn make_tg(&self) -> AppResult<Arc<dyn TgApi>> {
        if !self.is_configured() {
            return Ok(Arc::new(crate::telegram::UnconfiguredTg));
        }
        let client = TgClient::new(&self.api_base, &self.bot_token, &self.chat_id)
            .map_err(|e| AppError::BadRequest(format!("客户端构造失败: {e}")))?;
        Ok(Arc::new(client))
    }

    pub fn is_official_api(&self) -> bool {
        config::is_official(&self.api_base)
    }

    /// 取某项的字符串形式(写入 settings 表与导出 .env 用)
    fn value_of(&self, key: &str) -> String {
        match key {
            "api_base" => self.api_base.clone(),
            "bot_token" => self.bot_token.clone(),
            "chat_id" => self.chat_id.clone(),
            "chunk_size" => self.chunk_size.to_string(),
            "upload_concurrency" => self.upload_concurrency.to_string(),
            "send_video" => self.send_video.to_string(),
            "tmp_dir" => self.tmp_dir.display().to_string(),
            "username" => self.username.clone(),
            "password" => self.password.clone(),
            "telegram_api_id" => self.telegram_api_id.clone(),
            "telegram_api_hash" => self.telegram_api_hash.clone(),
            _ => String::new(),
        }
    }
}

fn env_present(key: &str) -> bool {
    std::env::var(key).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// 前端提交的改动;`None` 表示该项不动
#[derive(Deserialize, Default, Debug)]
pub struct ConfigPatch {
    pub api_base: Option<String>,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    /// 字符串以支持 "18MB" 这类写法
    pub chunk_size: Option<String>,
    pub upload_concurrency: Option<usize>,
    pub send_video: Option<bool>,
    pub tmp_dir: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub telegram_api_id: Option<String>,
    pub telegram_api_hash: Option<String>,
    /// 跳过保存前的 getMe 验证(例如自建端点还没起来)
    #[serde(default)]
    pub force: bool,
}

/// 保存结果,供前端决定提示什么
#[derive(serde::Serialize)]
pub struct ApplyOutcome {
    /// 实际发生变化的键
    pub changed: Vec<String>,
    /// 账号变了 → 所有会话已失效,需要重新登录
    pub account_changed: bool,
    /// 改的项里有 telegram-bot-api 容器的凭据,需要重建那个容器才生效
    pub needs_local_api_restart: bool,
    /// 验证到的 bot 名(跳过验证时为 null)
    pub bot: Option<String>,
}

/// 热更句柄集合,放进 AppState 共享
pub struct Runtime {
    pub db: Db,
    /// 环境变量层,用于来源标注与「恢复」
    pub env: Arc<Config>,
    current: RwLock<Arc<RuntimeCfg>>,
    tg: Arc<SwitchableTg>,
    knobs: Arc<UploadKnobs>,
    pub auth: Arc<Auth>,
    pub started_at: i64,
    bot_name: RwLock<Option<(String, Instant)>>,
}

impl Runtime {
    pub fn new(
        db: Db,
        env: Arc<Config>,
        cfg: RuntimeCfg,
        tg: Arc<SwitchableTg>,
        knobs: Arc<UploadKnobs>,
        auth: Arc<Auth>,
    ) -> Self {
        Runtime {
            db,
            env,
            current: RwLock::new(Arc::new(cfg)),
            tg,
            knobs,
            auth,
            started_at: crate::db::now_ts(),
            bot_name: RwLock::new(None),
        }
    }

    /// guard 立即释放,不跨 await
    pub fn current(&self) -> Arc<RuntimeCfg> {
        self.current.read().unwrap().clone()
    }

    /// 每项的来源:db(后台改过)/ env(来自环境变量)/ default(内置默认)
    pub async fn sources(&self) -> AppResult<HashMap<String, &'static str>> {
        let db_vals = self.db.settings_with_prefix(P).await?;
        Ok(KEYS
            .iter()
            .map(|(k, envk)| {
                let src = if db_vals.contains_key(*k) {
                    "db"
                } else if env_present(envk) {
                    "env"
                } else {
                    "default"
                };
                (k.to_string(), src)
            })
            .collect())
    }

    /// bot 名称(60 秒缓存,进后台页时用;失败返回 None 而不是报错)
    pub async fn bot_name(&self) -> Option<String> {
        if !self.current().is_configured() {
            return None;
        }
        if let Some((n, at)) = self.bot_name.read().unwrap().clone()
            && at.elapsed() < BOT_NAME_TTL
        {
            return Some(n);
        }
        match self.tg.get_me().await {
            Ok(n) => {
                *self.bot_name.write().unwrap() = Some((n.clone(), Instant::now()));
                Some(n)
            }
            Err(e) => {
                tracing::warn!("getMe 失败: {e}");
                None
            }
        }
    }

    /// 用指定参数试连(不落库),返回 bot 名
    pub async fn probe(api_base: &str, token: &str, chat_id: &str) -> AppResult<String> {
        let client = TgClient::new(api_base, token, chat_id)
            .map_err(|e| AppError::BadRequest(format!("客户端构造失败: {e}")))?;
        client.get_me().await
    }

    /// 保存并热生效
    pub async fn apply(&self, patch: ConfigPatch) -> AppResult<ApplyOutcome> {
        let prev = self.current();
        let mut next = (*prev).clone();

        if let Some(v) = &patch.api_base {
            next.api_base = v.trim().trim_end_matches('/').to_string();
        }
        if let Some(v) = &patch.bot_token {
            next.bot_token = v.trim().to_string();
        }
        if let Some(v) = &patch.chat_id {
            next.chat_id = v.trim().to_string();
        }
        if let Some(v) = &patch.chunk_size {
            next.chunk_size = config::parse_size(v)
                .map_err(|e| AppError::BadRequest(format!("分块大小无效: {e}")))?;
        }
        if let Some(v) = patch.upload_concurrency {
            next.upload_concurrency = v;
        }
        if let Some(v) = patch.send_video {
            next.send_video = v;
        }
        if let Some(v) = &patch.tmp_dir {
            next.tmp_dir = PathBuf::from(v.trim());
        }
        if let Some(v) = &patch.username {
            next.username = v.trim().to_string();
        }
        if let Some(v) = &patch.password {
            next.password = v.clone();
        }
        if let Some(v) = &patch.telegram_api_id {
            next.telegram_api_id = v.trim().to_string();
        }
        if let Some(v) = &patch.telegram_api_hash {
            next.telegram_api_hash = v.trim().to_string();
        }

        self.validate(&next)?;

        // 动到 Telegram 三项时先验证,别把网盘改到连不上
        let tg_changed = next.api_base != prev.api_base
            || next.bot_token != prev.bot_token
            || next.chat_id != prev.chat_id;
        let mut bot = None;
        if tg_changed && next.is_configured() && !patch.force {
            match Self::probe(&next.api_base, &next.bot_token, &next.chat_id).await {
                Ok(name) => bot = Some(name),
                Err(e) => {
                    return Err(AppError::BadRequest(format!(
                        "新配置连不上 Telegram({e});确认无误可选择强制保存"
                    )))
                }
            }
        }

        let changed = self.persist(&prev, &next).await?;
        let needs_local_api_restart =
            changed.iter().any(|k| LOCAL_API_KEYS.contains(&k.as_str()));
        let account_changed = self.install(&prev, next, tg_changed).await?;
        Ok(ApplyOutcome { changed, account_changed, needs_local_api_restart, bot })
    }

    /// 恢复指定项为环境变量/默认值
    pub async fn reset(&self, keys: &[String]) -> AppResult<ApplyOutcome> {
        let prev = self.current();
        for k in keys {
            if !KEYS.iter().any(|(name, _)| name == k) {
                return Err(AppError::BadRequest(format!("未知配置项: {k}")));
            }
            self.db.setting_del(&format!("{P}{k}")).await?;
        }
        let next = RuntimeCfg::load(&self.db, &self.env).await?;
        // 恢复出的组合仍要合法(例如端点回落到官方而分块过大)
        if let Err(e) = self.validate(&next) {
            // 回滚:把刚删掉的键写回去
            for k in keys {
                self.db.setting_set(&format!("{P}{k}"), &prev.value_of(k)).await?;
            }
            return Err(e);
        }
        let tg_changed = next.api_base != prev.api_base
            || next.bot_token != prev.bot_token
            || next.chat_id != prev.chat_id;
        let account_changed = self.install(&prev, next, tg_changed).await?;
        Ok(ApplyOutcome {
            changed: keys.to_vec(),
            account_changed,
            needs_local_api_restart: keys.iter().any(|k| LOCAL_API_KEYS.contains(&k.as_str())),
            bot: None,
        })
    }

    fn validate(&self, next: &RuntimeCfg) -> AppResult<()> {
        let bad = AppError::BadRequest;
        if !next.api_base.starts_with("http://") && !next.api_base.starts_with("https://") {
            return Err(bad("API 端点必须以 http:// 或 https:// 开头".into()));
        }
        // Bot Token 与频道允许同时留空(尚未配置状态);填了就得成对
        if next.bot_token.is_empty() != next.chat_id.is_empty() {
            return Err(bad("Bot Token 与存储频道要么都填,要么都留空".into()));
        }
        config::validate_chunk_size(&next.api_base, next.chunk_size)
            .map_err(|e| bad(e.to_string()))?;
        if !(1..=16).contains(&next.upload_concurrency) {
            return Err(bad("上传并发数需在 1~16 之间".into()));
        }
        if next.username.trim().is_empty() || next.password.is_empty() {
            return Err(bad("用户名与密码都不能为空".into()));
        }
        if !next.telegram_api_id.is_empty()
            && next.telegram_api_id.parse::<u64>().is_err()
        {
            return Err(bad("api_id 应为纯数字(在 my.telegram.org 申请)".into()));
        }
        // 临时目录必须存在且可写,否则大分块上传会在半路失败
        std::fs::create_dir_all(&next.tmp_dir)
            .map_err(|e| bad(format!("临时目录不可用: {e}")))?;
        tempfile::NamedTempFile::new_in(&next.tmp_dir)
            .map_err(|e| bad(format!("临时目录不可写: {e}")))?;
        Ok(())
    }

    /// 把变化的项写入 settings 表,返回变化的键名
    async fn persist(&self, prev: &RuntimeCfg, next: &RuntimeCfg) -> AppResult<Vec<String>> {
        let mut changed = Vec::new();
        for (k, _) in KEYS {
            let (a, b) = (prev.value_of(k), next.value_of(k));
            if a != b {
                self.db.setting_set(&format!("{P}{k}"), &b).await?;
                changed.push(k.to_string());
            }
        }
        Ok(changed)
    }

    /// 推送到各热更句柄并更新当前值;返回账号是否变化
    async fn install(
        &self,
        prev: &RuntimeCfg,
        next: RuntimeCfg,
        tg_changed: bool,
    ) -> AppResult<bool> {
        if tg_changed {
            // 没配全时装占位实现,让上传/下载给出「去设置里配」而不是 401
            self.tg.replace(next.make_tg()?);
            *self.bot_name.write().unwrap() = None;
        }
        if next.chunk_size != prev.chunk_size {
            self.knobs.set_chunk_size(next.chunk_size);
        }
        if next.tmp_dir != prev.tmp_dir {
            self.knobs.set_tmp_dir(next.tmp_dir.clone());
        }
        if next.upload_concurrency != prev.upload_concurrency {
            self.knobs.set_concurrency(next.upload_concurrency);
        }
        if next.send_video != prev.send_video {
            self.knobs.set_send_video(next.send_video);
        }
        let account_changed =
            next.username != prev.username || next.password != prev.password;
        if account_changed {
            self.auth.set_account(&next.username, &next.password);
            // 改了账号就把旧会话踢掉,否则拿着旧 cookie 仍能访问
            self.auth.rotate_secret(&self.db).await?;
        }
        *self.current.write().unwrap() = Arc::new(next);
        Ok(account_changed)
    }

    /// 需重启才能改的项(展示用)
    pub fn readonly_items(&self) -> Vec<(&'static str, String)> {
        vec![
            ("listen", self.env.listen.to_string()),
            ("db_path", self.env.db_path.clone()),
            (
                "static_dir",
                std::env::var("TGDRIVE_STATIC_DIR").unwrap_or_else(|_| "(自动探测)".into()),
            ),
            ("log_level", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())),
        ]
    }

    /// 按当前生效值生成 .env 文本,供用户同步回宿主机
    pub fn env_export(&self) -> String {
        let c = self.current();
        let mut s = String::from("# 由 tgdrive 后台导出,反映当前生效配置\n");
        for (k, envk) in KEYS {
            if LOCAL_API_KEYS.contains(k) {
                continue;
            }
            s.push_str(&format!("{envk}={}\n", c.value_of(k)));
        }
        s.push_str(&format!("TGDRIVE_LISTEN={}\n", self.env.listen));
        s.push_str(&format!("TGDRIVE_DB_PATH={}\n", self.env.db_path));
        s.push_str(
            "\n# 自建 Bot API 服务器(compose 的 telegram-bot-api 服务)用,\n\
             # 改这两项必须把本文件覆盖回宿主机再重建容器才生效\n",
        );
        for k in LOCAL_API_KEYS {
            let envk = KEYS.iter().find(|(n, _)| n == k).map(|(_, e)| *e).unwrap_or("");
            s.push_str(&format!("{envk}={}\n", c.value_of(k)));
        }
        s.push_str(
            "\n# 注意:后台改过的项以数据库为准,.env 里的同名变量不会覆盖它\n\
             # 想让 .env 重新生效,请在后台点对应项的「恢复」\n",
        );
        s
    }
}

/// 敏感值脱敏展示
pub fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 10 {
        return "*".repeat(n.clamp(1, 8));
    }
    let head: String = s.chars().take(6).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}***{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_cfg() -> Config {
        Config {
            listen: "0.0.0.0:8080".parse().unwrap(),
            bot_token: "envtoken".into(),
            chat_id: "-100111".into(),
            api_base: config::OFFICIAL_API_BASE.into(),
            chunk_size: 18 * 1024 * 1024,
            upload_concurrency: 2,
            db_path: "x.db".into(),
            tmp_dir: std::env::temp_dir(),
            username: "envuser".into(),
            password: "envpass".into(),
            secret: None,
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            send_video: false,
        }
    }

    async fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn db_overrides_env() {
        let (db, _g) = test_db().await;
        let env = env_cfg();
        // 没有覆盖时全部取 env
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert_eq!(c.bot_token, "envtoken");
        assert_eq!(c.chunk_size, 18 * 1024 * 1024);

        db.setting_set("cfg.bot_token", "dbtoken").await.unwrap();
        db.setting_set("cfg.chunk_size", "2097152").await.unwrap();
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert_eq!(c.bot_token, "dbtoken");
        assert_eq!(c.chunk_size, 2 * 1024 * 1024);
        assert_eq!(c.username, "envuser"); // 未覆盖的仍来自 env
    }

    #[tokio::test]
    async fn send_video_default_follows_endpoint() {
        let (db, _g) = test_db().await;
        // 官方端点默认关
        let c = RuntimeCfg::load(&db, &env_cfg()).await.unwrap();
        assert!(!c.send_video);

        // 端点改成自建(db 覆盖)→ 默认开
        db.setting_set("cfg.api_base", "http://tg:8081").await.unwrap();
        let c = RuntimeCfg::load(&db, &env_cfg()).await.unwrap();
        assert!(c.send_video);

        // 后台显式关掉 → 以 db 为准
        db.setting_set("cfg.send_video", "false").await.unwrap();
        let c = RuntimeCfg::load(&db, &env_cfg()).await.unwrap();
        assert!(!c.send_video);
    }

    #[tokio::test]
    async fn oversized_chunk_falls_back_on_load() {
        let (db, _g) = test_db().await;
        // 官方端点 + 512MB 分块(自建时代的遗留)→ 启动应回落默认值而非拒绝
        db.setting_set("cfg.chunk_size", &(512u64 * 1024 * 1024).to_string()).await.unwrap();
        let c = RuntimeCfg::load(&db, &env_cfg()).await.unwrap();
        assert_eq!(c.chunk_size, config::OFFICIAL_CHUNK_DEFAULT);
    }

    #[test]
    fn chunk_rules() {
        let official = config::OFFICIAL_API_BASE;
        assert!(config::validate_chunk_size(official, 25 * 1024 * 1024).is_err()); // 超官方下载上限
        assert!(config::validate_chunk_size(official, 18 * 1024 * 1024).is_ok());
        assert!(config::validate_chunk_size("http://tg:8081", 512 * 1024 * 1024).is_ok());
        assert!(config::validate_chunk_size("http://tg:8081", 1024).is_err()); // 太小
        assert!(config::validate_chunk_size("http://tg:8081", 2_000 * 1024 * 1024).is_err());
    }

    #[tokio::test]
    async fn unconfigured_gives_readable_error() {
        let (db, _g) = test_db().await;
        let mut env = env_cfg();
        env.bot_token = String::new();
        env.chat_id = String::new();
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert!(!c.is_configured());
        // 未配置时装的是占位实现:提示去设置,而不是 Telegram 的 401
        let err = c.make_tg().unwrap().get_me().await.unwrap_err().to_string();
        assert!(err.contains("系统设置"), "实际错误: {err}");

        // 配置齐了就换成真客户端
        db.setting_set("cfg.bot_token", "123:abc").await.unwrap();
        db.setting_set("cfg.chat_id", "-100999").await.unwrap();
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert!(c.is_configured());
    }

    #[test]
    fn mask_hides_secret() {
        assert_eq!(mask("short"), "*****");
        let m = mask("123456789:AABBCCDDEEFF");
        assert!(m.starts_with("123456") && m.ends_with("EEFF") && !m.contains("AABB"));
    }
}
