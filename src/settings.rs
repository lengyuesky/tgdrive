//! 配置中心:哪些配置能在运行时改、怎么持久化、怎么热生效。
//!
//! 优先级 **settings 表 > 环境变量 > 内置默认**。后台保存即写表并推给各个热更句柄
//! (`TgRegistry` / `UploadKnobs` / `Auth`),无需重启容器。
//!
//! 监听地址、数据库路径等启动期配置不在此列(见 `Runtime::readonly_items`)。

use crate::auth::Auth;
use crate::config::{self, Config};
use crate::db::Db;
use crate::engine::upload::UploadKnobs;
use crate::error::{AppError, AppResult};
use crate::proxy::{ProxyConfig, ProxySnapshot};
#[cfg(test)]
use crate::telegram::TgApi;
use crate::telegram::{TgClient, TgRegistry};
use futures_util::future::join_all;
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
    ("proxy_enabled", "TGDRIVE_PROXY_ENABLED"),
    ("proxy_url", "TGDRIVE_PROXY_URL"),
    ("bot_token", "TGDRIVE_BOT_TOKEN"),
    ("upload_bot_tokens", "TGDRIVE_UPLOAD_BOT_TOKENS"),
    ("chat_id", "TGDRIVE_CHAT_ID"),
    ("chunk_size", "TGDRIVE_CHUNK_SIZE"),
    ("upload_concurrency", "TGDRIVE_UPLOAD_CONCURRENCY"),
    ("download_concurrency", "TGDRIVE_DOWNLOAD_CONCURRENCY"),
    ("send_video", "TGDRIVE_SEND_VIDEO"),
    ("tmp_dir", "TGDRIVE_TMP_DIR"),
    ("username", "TGDRIVE_USERNAME"),
    ("password", "TGDRIVE_PASSWORD"),
    ("telegram_api_id", "TELEGRAM_API_ID"),
    ("telegram_api_hash", "TELEGRAM_API_HASH"),
];

/// 这两项同时供 tgdrive 的 MTProto 流式下载和 telegram-bot-api 容器使用。
/// tgdrive 在启动时建立会话，compose 的 `${VAR}` 也只在创建容器时插值，
/// 因此修改后需要重启/重建两个服务。
pub const LOCAL_API_KEYS: &[&str] = &["telegram_api_id", "telegram_api_hash"];

/// 当前生效的可变配置
#[derive(Clone)]
pub struct RuntimeCfg {
    pub api_base: String,
    pub proxy: ProxyConfig,
    pub bot_token: String,
    /// 额外上传 Bot，不包含主 Bot；主 Bot 仍负责收件箱与备份。
    pub upload_bot_tokens: Vec<String>,
    pub chat_id: String,
    pub chunk_size: u64,
    pub upload_concurrency: usize,
    pub download_concurrency: usize,
    /// 单块以内的视频改用 sendVideo 发送,在 Telegram 里可直接播放
    pub send_video: bool,
    pub tmp_dir: PathBuf,
    pub username: String,
    /// 存储形式：数据库里一律是 argon2 哈希；来自 `.env` 时可能是明文。
    pub password: String,
    pub telegram_api_id: String,
    pub telegram_api_hash: String,
    /// 不属于用户配置字段；指向上述 Telegram 三元组对应的不可变存储目标。
    pub storage_target_id: Option<i64>,
    /// 新上传文件可选择的目标，首项始终是主 Bot 对应目标。
    pub upload_target_ids: Vec<i64>,
}

impl RuntimeCfg {
    /// 用 settings 表里的值覆盖环境变量层
    pub async fn load(db: &Db, env: &Config) -> AppResult<Self> {
        let db_vals = db.settings_with_prefix(P).await?;
        Self::from_overrides(&db_vals, env)
    }

    fn from_overrides(db_vals: &HashMap<String, String>, env: &Config) -> AppResult<Self> {
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

        let bot_token = get("bot_token")
            .map(str::to_string)
            .unwrap_or_else(|| env.bot_token.clone());
        let mut upload_bot_tokens = get("upload_bot_tokens")
            .map(config::parse_bot_tokens)
            .unwrap_or_else(|| env.upload_bot_tokens.clone());
        upload_bot_tokens.retain(|token| token.trim() != bot_token.trim());

        let proxy = ProxyConfig::new(
            get("proxy_enabled")
                .map(config::parse_bool)
                .transpose()
                .map_err(|_| AppError::BadRequest("代理开关配置无效".into()))?
                .unwrap_or(env.proxy.enabled),
            get("proxy_url").unwrap_or(&env.proxy.url),
        )?;

        Ok(RuntimeCfg {
            api_base,
            proxy,
            bot_token,
            upload_bot_tokens,
            chat_id: get("chat_id")
                .map(str::to_string)
                .unwrap_or_else(|| env.chat_id.clone()),
            chunk_size,
            upload_concurrency: get("upload_concurrency")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(env.upload_concurrency)
                .clamp(1, 16),
            download_concurrency: get("download_concurrency")
                .and_then(|v| config::parse_download_concurrency(v).ok())
                .unwrap_or(env.download_concurrency),
            send_video,
            tmp_dir: get("tmp_dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| env.tmp_dir.clone()),
            username: get("username")
                .map(str::to_string)
                .unwrap_or_else(|| env.username.clone()),
            password: get("password")
                .map(str::to_string)
                .unwrap_or_else(|| env.password.clone()),
            telegram_api_id: get("telegram_api_id")
                .map(str::to_string)
                .unwrap_or_else(|| env.telegram_api_id.clone()),
            telegram_api_hash: get("telegram_api_hash")
                .map(str::to_string)
                .unwrap_or_else(|| env.telegram_api_hash.clone()),
            storage_target_id: None,
            upload_target_ids: Vec::new(),
        })
    }

    /// Telegram 存储是否已配置好(没配时上传下载会给出提示而不是报 401)
    pub fn is_configured(&self) -> bool {
        !self.bot_token.trim().is_empty() && !self.chat_id.trim().is_empty()
    }

    /// 主 Bot + 去重后的额外上传 Bot。
    pub fn all_upload_bot_tokens(&self) -> Vec<String> {
        let mut tokens = Vec::new();
        for token in std::iter::once(&self.bot_token).chain(self.upload_bot_tokens.iter()) {
            let token = token.trim();
            if !token.is_empty() && !tokens.iter().any(|existing| existing == token) {
                tokens.push(token.to_string());
            }
        }
        tokens
    }

    /// 按当前配置造一个 TG 客户端;没配好就返回占位实现
    #[cfg(test)]
    pub fn make_tg(&self) -> AppResult<Arc<dyn TgApi>> {
        if !self.is_configured() {
            return Ok(Arc::new(crate::telegram::UnconfiguredTg));
        }
        let client = TgClient::new(
            &self.api_base,
            &self.bot_token,
            &self.chat_id,
            self.proxy.clone(),
        )
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
            "proxy_enabled" => self.proxy.enabled.to_string(),
            "proxy_url" => self.proxy.url.clone(),
            "bot_token" => self.bot_token.clone(),
            // .env 不适合多行值，统一导出为逗号分隔。
            "upload_bot_tokens" => self.upload_bot_tokens.join(","),
            "chat_id" => self.chat_id.clone(),
            "chunk_size" => self.chunk_size.to_string(),
            "upload_concurrency" => self.upload_concurrency.to_string(),
            "download_concurrency" => self.download_concurrency.to_string(),
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

use crate::config::env_present;

/// 前端提交的改动;`None` 表示该项不动
#[derive(Deserialize, Default)]
pub struct ConfigPatch {
    pub api_base: Option<String>,
    pub proxy_enabled: Option<bool>,
    pub proxy_url: Option<String>,
    pub bot_token: Option<String>,
    /// 多行或逗号分隔的额外上传 Bot Token。
    pub upload_bot_tokens: Option<String>,
    pub chat_id: Option<String>,
    /// 字符串以支持 "18MB" 这类写法
    pub chunk_size: Option<String>,
    pub upload_concurrency: Option<usize>,
    pub download_concurrency: Option<usize>,
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
    /// api_id/api_hash 有变化，需要重启 tgdrive 并重建 telegram-bot-api 才生效。
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
    /// 串行提交，防止同时保存或恢复时让代理句柄与持久配置互相覆盖。
    update_lock: tokio::sync::Mutex<()>,
    tg: Arc<TgRegistry>,
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
        tg: Arc<TgRegistry>,
        knobs: Arc<UploadKnobs>,
        auth: Arc<Auth>,
    ) -> Self {
        tg.set_download_concurrency(cfg.download_concurrency);
        Runtime {
            db,
            env,
            current: RwLock::new(Arc::new(cfg)),
            update_lock: tokio::sync::Mutex::new(()),
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

    /// 后台改消息清理的自动/手动开关后热更给 uploader worker,无需重启容器
    pub fn set_delete_queue_mode(&self, auto_delete_message: bool) {
        self.knobs.set_auto_delete_message(auto_delete_message);
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
        let binding = match self.tg.current() {
            Ok(binding) => binding,
            Err(_) => return None,
        };
        match binding.api.get_me().await {
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
    pub async fn probe(
        api_base: &str,
        token: &str,
        chat_id: &str,
        proxy: &ProxyConfig,
    ) -> AppResult<String> {
        let client = TgClient::new(api_base, token, chat_id, proxy.clone())
            .map_err(|e| AppError::BadRequest(format!("客户端构造失败: {e}")))?;
        tokio::time::timeout(Duration::from_secs(15), client.probe_storage())
            .await
            .map_err(|_| AppError::BadRequest("连接验证超时(15 秒)".into()))?
    }

    /// 保存并热生效
    pub async fn apply(&self, patch: ConfigPatch) -> AppResult<ApplyOutcome> {
        let _guard = self.update_lock.lock().await;
        let prev = self.current();
        let mut next = (*prev).clone();
        next.proxy = prev
            .proxy
            .patched(patch.proxy_enabled, patch.proxy_url.as_deref())?;

        if let Some(v) = &patch.api_base {
            next.api_base = v.trim().trim_end_matches('/').to_string();
        }
        if let Some(v) = &patch.bot_token {
            next.bot_token = v.trim().to_string();
        }
        if let Some(v) = &patch.upload_bot_tokens {
            next.upload_bot_tokens = config::parse_bot_tokens(v);
        }
        next.upload_bot_tokens
            .retain(|token| token.trim() != next.bot_token.trim());
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
        if let Some(v) = patch.download_concurrency {
            next.download_concurrency = v;
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
        if let Some(v) = patch.password.clone() {
            if v.is_empty() {
                return Err(AppError::BadRequest("用户名与密码都不能为空".into()));
            }
            // 与原密码相同就保留原哈希，免得无谓地让所有会话失效
            let prev_stored = prev.password.clone();
            next.password = tokio::task::spawn_blocking(move || {
                if crate::password::verify(&prev_stored, &v) {
                    Ok(prev_stored)
                } else {
                    crate::password::hash(&v)
                }
            })
            .await
            .map_err(|error| AppError::Internal(anyhow::anyhow!("密码哈希任务失败：{error}")))??;
        }
        if let Some(v) = &patch.telegram_api_id {
            next.telegram_api_id = v.trim().to_string();
        }
        if let Some(v) = &patch.telegram_api_hash {
            next.telegram_api_hash = v.trim().to_string();
        }

        self.validate(&next)?;

        // 端点、凭据或代理变化时先验证，避免把网盘改到连不上。
        let proxy_update = (next.proxy != prev.proxy)
            .then(|| ProxySnapshot::new(next.proxy.clone()))
            .transpose()?;
        let tg_changed = next.proxy != prev.proxy
            || next.api_base != prev.api_base
            || next.bot_token != prev.bot_token
            || next.upload_bot_tokens != prev.upload_bot_tokens
            || next.chat_id != prev.chat_id;
        let mut bot = None;
        if tg_changed && next.is_configured() && !patch.force {
            let tokens = next.all_upload_bot_tokens();
            let api_base = next.api_base.clone();
            let chat_id = next.chat_id.clone();
            let results = join_all(tokens.iter().enumerate().map(|(index, token)| {
                let api_base = api_base.clone();
                let chat_id = chat_id.clone();
                let proxy = next.proxy.clone();
                async move {
                    let result = tokio::time::timeout(
                        Duration::from_secs(15),
                        Self::probe(&api_base, token, &chat_id, &proxy),
                    )
                    .await
                    .map_err(|_| AppError::BadRequest("连接验证超时(15 秒)".into()))
                    .and_then(|result| result);
                    (index, result)
                }
            }))
            .await;
            for (index, result) in results {
                match result {
                    Ok(name) if index == 0 => bot = Some(name),
                    Ok(_) => {}
                    Err(e) => {
                        let label = if index == 0 {
                            "主 Bot".to_string()
                        } else {
                            format!("额外上传 Bot #{index}")
                        };
                        return Err(AppError::BadRequest(format!(
                            "{label} 连不上 Telegram({e});确认无误可选择强制保存"
                        )));
                    }
                }
            }
        }

        self.attach_storage_target(&mut next).await?;

        let account_changed = next.username != prev.username || next.password != prev.password;
        let new_secret = account_changed.then(crate::auth::random_secret);
        let changed = self.persist(&prev, &next, new_secret.as_deref()).await?;
        let needs_local_api_restart = changed.iter().any(|k| LOCAL_API_KEYS.contains(&k.as_str()));
        self.install(&prev, next, tg_changed, new_secret.as_deref(), proxy_update)?;
        Ok(ApplyOutcome {
            changed,
            account_changed,
            needs_local_api_restart,
            bot,
        })
    }

    /// 恢复指定项为环境变量/默认值
    pub async fn reset(&self, keys: &[String]) -> AppResult<ApplyOutcome> {
        let _guard = self.update_lock.lock().await;
        let prev = self.current();
        for k in keys {
            if !KEYS.iter().any(|(name, _)| name == k) {
                return Err(AppError::BadRequest(format!("未知配置项: {k}")));
            }
        }
        // 先在内存中计算删除覆盖项后的配置，全部验证通过后再一次性提交。
        let mut overrides = self.db.settings_with_prefix(P).await?;
        for key in keys {
            overrides.remove(key);
        }
        let mut next = RuntimeCfg::from_overrides(&overrides, &self.env)?;
        // 恢复出的组合仍要合法(例如端点回落到官方而分块过大)
        self.validate(&next)?;
        let proxy_update = (next.proxy != prev.proxy)
            .then(|| ProxySnapshot::new(next.proxy.clone()))
            .transpose()?;
        self.attach_storage_target(&mut next).await?;
        let tg_changed = next.proxy != prev.proxy
            || next.api_base != prev.api_base
            || next.bot_token != prev.bot_token
            || next.upload_bot_tokens != prev.upload_bot_tokens
            || next.chat_id != prev.chat_id;
        let deletes: Vec<String> = keys.iter().map(|key| format!("{P}{key}")).collect();
        let account_changed = next.username != prev.username || next.password != prev.password;
        let new_secret = account_changed.then(crate::auth::random_secret);
        let secret_sets: Vec<(String, String)> = new_secret
            .as_ref()
            .map(|secret| vec![(crate::auth::SECRET_KEY.to_string(), secret.clone())])
            .unwrap_or_default();
        self.db.settings_apply(&secret_sets, &deletes).await?;
        self.install(&prev, next, tg_changed, new_secret.as_deref(), proxy_update)?;
        Ok(ApplyOutcome {
            changed: keys.to_vec(),
            account_changed,
            needs_local_api_restart: keys.iter().any(|k| LOCAL_API_KEYS.contains(&k.as_str())),
            bot: None,
        })
    }

    async fn attach_storage_target(&self, next: &mut RuntimeCfg) -> AppResult<()> {
        if !next.is_configured() {
            next.storage_target_id = None;
            next.upload_target_ids.clear();
            return Ok(());
        }
        let mut target_ids = Vec::new();
        for token in next.all_upload_bot_tokens() {
            let target = self
                .db
                .ensure_storage_target(&next.api_base, &token, &next.chat_id)
                .await?;
            self.tg.register(&target)?;
            target_ids.push(target.id);
        }
        next.storage_target_id = target_ids.first().copied();
        next.upload_target_ids = target_ids;
        Ok(())
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
        if !next.upload_bot_tokens.is_empty() && !next.is_configured() {
            return Err(bad("配置额外上传 Bot 前必须先配置主 Bot 与存储频道".into()));
        }
        if next.all_upload_bot_tokens().len() > 20 {
            return Err(bad("上传 Bot 总数不能超过 20 个".into()));
        }
        config::validate_chunk_size(&next.api_base, next.chunk_size)
            .map_err(|e| bad(e.to_string()))?;
        if !(1..=16).contains(&next.upload_concurrency) {
            return Err(bad("上传并发数需在 1~16 之间".into()));
        }
        config::parse_download_concurrency(&next.download_concurrency.to_string())
            .map_err(|e| bad(e.to_string()))?;
        if next.username.trim().is_empty() || next.password.is_empty() {
            return Err(bad("用户名与密码都不能为空".into()));
        }
        if !next.telegram_api_id.is_empty()
            && !matches!(next.telegram_api_id.parse::<i32>(), Ok(id) if id > 0)
        {
            return Err(bad(
                "api_id 应为有效的正整数（在 my.telegram.org 申请）".into()
            ));
        }
        // 临时目录必须存在且可写,否则大分块上传会在半路失败
        std::fs::create_dir_all(&next.tmp_dir).map_err(|e| bad(format!("临时目录不可用: {e}")))?;
        tempfile::NamedTempFile::new_in(&next.tmp_dir)
            .map_err(|e| bad(format!("临时目录不可写: {e}")))?;
        Ok(())
    }

    /// 把变化的项写入 settings 表,返回变化的键名
    async fn persist(
        &self,
        prev: &RuntimeCfg,
        next: &RuntimeCfg,
        new_secret: Option<&str>,
    ) -> AppResult<Vec<String>> {
        let mut changed = Vec::new();
        let mut sets = Vec::new();
        for (k, _) in KEYS {
            let (a, b) = (prev.value_of(k), next.value_of(k));
            if a != b {
                sets.push((format!("{P}{k}"), b));
                changed.push(k.to_string());
            }
        }
        if let Some(secret) = new_secret {
            sets.push((crate::auth::SECRET_KEY.to_string(), secret.to_string()));
        }
        self.db.settings_apply(&sets, &[]).await?;
        Ok(changed)
    }

    /// 推送到各热更句柄并更新当前值;返回账号是否变化
    fn install(
        &self,
        prev: &RuntimeCfg,
        next: RuntimeCfg,
        tg_changed: bool,
        new_secret: Option<&str>,
        proxy_update: Option<Arc<ProxySnapshot>>,
    ) -> AppResult<bool> {
        if let Some(snapshot) = proxy_update {
            self.tg.set_proxy(snapshot);
        }
        if tg_changed {
            self.tg
                .set_targets(next.storage_target_id, next.upload_target_ids.clone())?;
            *self.bot_name.write().unwrap() = None;
        }
        if next.chunk_size != prev.chunk_size {
            self.knobs.set_chunk_size(next.chunk_size);
        }
        if next.tmp_dir != prev.tmp_dir {
            self.knobs.set_tmp_dir(next.tmp_dir.clone());
        }
        if next.upload_concurrency != prev.upload_concurrency
            || next.upload_target_ids != prev.upload_target_ids
        {
            self.knobs.set_concurrency(
                next.upload_concurrency
                    .saturating_mul(next.upload_target_ids.len().max(1)),
            );
        }
        if next.send_video != prev.send_video {
            self.knobs.set_send_video(next.send_video);
        }
        if next.download_concurrency != prev.download_concurrency {
            self.tg.set_download_concurrency(next.download_concurrency);
        }
        let account_changed = next.username != prev.username || next.password != prev.password;
        if account_changed {
            // 密钥已与账号配置在同一事务里写入；这里仅同步内存状态。
            let secret = new_secret
                .ok_or_else(|| AppError::Internal(anyhow::anyhow!("账号变化时缺少新的会话密钥")))?;
            self.auth
                .set_account_and_secret(&next.username, &next.password, secret);
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
                "local_data_root",
                self.env.local_data_root.display().to_string(),
            ),
            ("cookie_secure", self.env.cookie_secure.to_string()),
            (
                "static_dir",
                crate::config::static_dir().unwrap_or_else(|| "(自动探测)".into()),
            ),
            (
                "log_level",
                crate::config::log_filter().unwrap_or_else(|| "info".into()),
            ),
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
            let value = c.value_of(k);
            if *k == "password" && crate::password::is_hash(&value) {
                // 哈希里的 `$` 会被 Compose 当成变量插值，单引号保持原样
                s.push_str("# 密码以 argon2 哈希导出，tgdrive 可直接识别\n");
                s.push_str(&format!("{envk}='{value}'\n"));
                continue;
            }
            s.push_str(&format!("{envk}={value}\n"));
        }
        s.push_str(&format!("TGDRIVE_LISTEN={}\n", self.env.listen));
        s.push_str(&format!("TGDRIVE_DB_PATH={}\n", self.env.db_path));
        s.push_str(&format!(
            "TGDRIVE_LOCAL_DATA_ROOT={}\n",
            self.env.local_data_root.display()
        ));
        s.push_str(&format!(
            "TGDRIVE_COOKIE_SECURE={}\n",
            self.env.cookie_secure
        ));
        s.push_str(
            "\n# tgdrive 的 MTProto 流式下载与自建 Bot API 服务器共同使用,\n\
             # 改这两项后必须把本文件覆盖回宿主机，并重启/重建两个服务\n",
        );
        for k in LOCAL_API_KEYS {
            let envk = KEYS
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, e)| *e)
                .unwrap_or("");
            s.push_str(&format!("{envk}={}\n", c.value_of(k)));
        }
        s.push_str(
            "\n# 注意:后台改过的项以数据库为准,.env 里的同名变量不会覆盖它\n\
             # 想让 .env 重新生效,请在后台点对应项的「恢复」\n",
        );
        s
    }
}

/// 旧版把后台改过的登录密码明文存在 `cfg.password`；启动时一次性换成哈希。
pub async fn migrate_password_hash(db: &Db) -> AppResult<bool> {
    let key = format!("{P}password");
    let Some(stored) = db.setting_get(&key).await? else {
        return Ok(false);
    };
    if crate::password::is_hash(&stored) {
        return Ok(false);
    }
    let hashed = crate::password::hash_blocking(stored).await?;
    db.setting_set(&key, &hashed).await?;
    Ok(true)
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
            upload_bot_tokens: Vec::new(),
            chat_id: "-100111".into(),
            api_base: config::OFFICIAL_API_BASE.into(),
            proxy: ProxyConfig::default(),
            chunk_size: 18 * 1024 * 1024,
            upload_concurrency: 2,
            download_concurrency: config::DEFAULT_DOWNLOAD_CONCURRENCY,
            db_path: "x.db".into(),
            tmp_dir: std::env::temp_dir(),
            local_data_root: config::DEFAULT_LOCAL_DATA_ROOT.into(),
            username: "envuser".into(),
            password: "envpass".into(),
            secret: None,
            cookie_secure: false,
            trusted_proxies: Vec::new(),
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            send_video: false,
        }
    }

    async fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn db_overrides_env() {
        let (db, _g) = test_db().await;
        let env = env_cfg();
        // 没有覆盖时全部取 env
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert_eq!(c.bot_token, "envtoken");
        assert!(c.upload_bot_tokens.is_empty());
        assert_eq!(c.chunk_size, 18 * 1024 * 1024);

        db.setting_set("cfg.bot_token", "dbtoken").await.unwrap();
        db.setting_set("cfg.chunk_size", "2097152").await.unwrap();
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert_eq!(c.bot_token, "dbtoken");
        assert_eq!(c.chunk_size, 2 * 1024 * 1024);
        assert_eq!(c.username, "envuser"); // 未覆盖的仍来自 env

        db.setting_set("cfg.upload_bot_tokens", "extra1,extra2,extra1")
            .await
            .unwrap();
        let c = RuntimeCfg::load(&db, &env).await.unwrap();
        assert_eq!(c.upload_bot_tokens, vec!["extra1", "extra2"]);
    }

    #[tokio::test]
    async fn password_is_stored_as_hash_and_legacy_plaintext_is_migrated() {
        let (db, _guard) = test_db().await;
        let mut env = env_cfg();
        env.bot_token.clear();
        env.chat_id.clear();
        let env = Arc::new(env);

        // 旧版明文 → 启动迁移后变哈希，原密码仍能登录
        db.setting_set("cfg.password", "legacy-pass").await.unwrap();
        assert!(migrate_password_hash(&db).await.unwrap());
        assert!(!migrate_password_hash(&db).await.unwrap());
        let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
        assert!(crate::password::is_hash(&cfg.password));
        let tg =
            Arc::new(TgRegistry::new(&[], None, vec![], None, ProxyConfig::default()).unwrap());
        let knobs = Arc::new(UploadKnobs::new(
            cfg.chunk_size,
            cfg.tmp_dir.clone(),
            2,
            false,
        ));
        let auth = Arc::new(
            Auth::init(&cfg.username, &cfg.password, None, false, &db)
                .await
                .unwrap(),
        );
        assert!(auth.verify_login_password("envuser", "legacy-pass"));
        let rt = Runtime::new(db.clone(), env.clone(), cfg, tg, knobs, auth.clone());

        // 提交相同密码不算账号变化
        let same = rt
            .apply(ConfigPatch {
                password: Some("legacy-pass".into()),
                ..ConfigPatch::default()
            })
            .await
            .unwrap();
        assert!(!same.account_changed);

        let changed = rt
            .apply(ConfigPatch {
                password: Some("new-pass".into()),
                ..ConfigPatch::default()
            })
            .await
            .unwrap();
        assert!(changed.account_changed);
        let stored = db.setting_get("cfg.password").await.unwrap().unwrap();
        assert!(crate::password::is_hash(&stored));
        assert!(!stored.contains("new-pass"));
        assert!(auth.verify_login_password("envuser", "new-pass"));
        assert!(!auth.verify_login_password("envuser", "legacy-pass"));
        assert!(
            rt.env_export()
                .contains(&format!("TGDRIVE_PASSWORD='{stored}'\n"))
        );

        // 恢复为 .env 明文密码
        rt.reset(&["password".into()]).await.unwrap();
        assert!(auth.verify_login_password("envuser", "envpass"));
    }

    #[tokio::test]
    async fn download_concurrency_is_validated_persisted_and_applied_without_restart() {
        let (db, _guard) = test_db().await;
        let mut env = env_cfg();
        env.bot_token.clear();
        env.chat_id.clear();
        let env = Arc::new(env);
        let cfg = RuntimeCfg::load(&db, &env).await.unwrap();
        let mtproto = crate::mtproto::MtprotoConfig::from_values(db.clone(), "1", "test-hash")
            .unwrap()
            .unwrap();
        let tg = Arc::new(
            TgRegistry::new(
                &[],
                None,
                vec![],
                Some(mtproto.clone()),
                ProxyConfig::default(),
            )
            .unwrap(),
        );
        let knobs = Arc::new(UploadKnobs::new(
            cfg.chunk_size,
            cfg.tmp_dir.clone(),
            2,
            false,
        ));
        let auth = Arc::new(
            Auth::init(&cfg.username, &cfg.password, None, false, &db)
                .await
                .unwrap(),
        );
        let rt = Runtime::new(db.clone(), env.clone(), cfg, tg, knobs, auth);

        for concurrency in [1, 2, 3] {
            rt.apply(ConfigPatch {
                download_concurrency: Some(concurrency),
                ..ConfigPatch::default()
            })
            .await
            .unwrap();
            assert_eq!(mtproto.download_concurrency(), concurrency);
            assert_eq!(rt.current().download_concurrency, concurrency);
            assert_eq!(
                RuntimeCfg::load(&db, &env)
                    .await
                    .unwrap()
                    .download_concurrency,
                concurrency
            );
        }

        let result = rt
            .apply(ConfigPatch {
                download_concurrency: Some(8),
                ..ConfigPatch::default()
            })
            .await
            .unwrap();
        assert_eq!(result.changed, vec!["download_concurrency"]);
        assert!(!result.needs_local_api_restart);
        assert_eq!(mtproto.download_concurrency(), 8);
        assert_eq!(rt.current().download_concurrency, 8);
        assert_eq!(
            RuntimeCfg::load(&db, &env)
                .await
                .unwrap()
                .download_concurrency,
            8
        );
        assert_eq!(rt.sources().await.unwrap()["download_concurrency"], "db");
        assert!(rt.env_export().contains("TGDRIVE_DOWNLOAD_CONCURRENCY=8\n"));

        for invalid in [0, 9, usize::MAX] {
            assert!(
                rt.apply(ConfigPatch {
                    download_concurrency: Some(invalid),
                    ..ConfigPatch::default()
                })
                .await
                .is_err()
            );
            assert_eq!(mtproto.download_concurrency(), 8);
            assert_eq!(rt.current().download_concurrency, 8);
        }
        assert_eq!(
            db.setting_get("cfg.download_concurrency")
                .await
                .unwrap()
                .as_deref(),
            Some("8")
        );
        rt.reset(&["download_concurrency".into()]).await.unwrap();
        assert_eq!(mtproto.download_concurrency(), 4);
        assert_eq!(rt.current().download_concurrency, 4);
        assert!(
            db.setting_get("cfg.download_concurrency")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn send_video_default_follows_endpoint() {
        let (db, _g) = test_db().await;
        // 官方端点默认关
        let c = RuntimeCfg::load(&db, &env_cfg()).await.unwrap();
        assert!(!c.send_video);

        // 端点改成自建(db 覆盖)→ 默认开
        db.setting_set("cfg.api_base", "http://tg:8081")
            .await
            .unwrap();
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
        db.setting_set("cfg.chunk_size", &(512u64 * 1024 * 1024).to_string())
            .await
            .unwrap();
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
