//! 固定的影视服务能力。页面只管理服务，协议与所有文件读取由宿主控制。

mod covers;
pub(crate) mod data;
mod frame;
mod index;
mod probe;
mod protocol;

use super::{InstalledApp, installed, scope};
use crate::{
    api::AppState,
    db::Node,
    error::{AppError, AppResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub const APP_ID: &str = "fntv";
const LOGIN_TTL: i64 = 6 * 60 * 60;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub verifier: String,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub running: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Library {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub root_id: i64,
}

#[derive(Clone)]
pub(super) struct Lease {
    pub epoch: u64,
    pub cancelled: CancellationToken,
}
impl Lease {
    pub fn check(&self) -> AppResult<()> {
        if self.cancelled.is_cancelled() {
            Err(AppError::Forbidden("影视服务连接已失效，请重新登录".into()))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub(super) struct Context {
    pub app: InstalledApp,
    pub config: Config,
    pub config_revision: Option<String>,
    pub lease: Lease,
}

#[derive(Clone)]
struct Login {
    expires: i64,
    lease: Lease,
}

#[derive(Clone, Default)]
pub(super) struct Play {
    pub serial: u64,
    pub link: Option<String>,
    pub read: bool,
    pub ended: bool,
    pub ambiguous: bool,
    pub hint: Option<String>,
}

#[derive(Default, Serialize)]
struct ProtocolStats {
    records: u64,
    link_changes: u64,
    unbound_records: u64,
    hint_matches: u64,
    hint_mismatches: u64,
    saved: u64,
    stale: u64,
}

struct Inner {
    epoch: u64,
    cancellation: CancellationToken,
    sessions: HashMap<String, Login>,
    plays: HashMap<(String, String), Play>,
    links: HashMap<(String, String), (String, u64)>,
    worker_epoch: u64,
    cover_worker_epoch: u64,
    cover_task: Option<CancellationToken>,
    foreground: usize,
    foreground_until: Instant,
    index_state: String,
    index_error: String,
    indexed: usize,
    login_window: Instant,
    login_count: u16,
    stats: ProtocolStats,
}

/// 生命周期写入在提交后同步撤销；响应体和探测都订阅同一取消信号。
pub(crate) struct Service {
    inner: Mutex<Inner>,
    pub(super) index_lock: AsyncMutex<()>,
    pub(super) play_lock: AsyncMutex<()>,
    pub(super) probe_slots: Arc<Semaphore>,
    pub(super) cover_lock: AsyncMutex<()>,
    pub(super) cover_wake: Notify,
}
impl Default for Service {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                epoch: 1,
                cancellation: CancellationToken::new(),
                sessions: HashMap::new(),
                plays: HashMap::new(),
                links: HashMap::new(),
                worker_epoch: 0,
                cover_worker_epoch: 0,
                cover_task: None,
                foreground: 0,
                foreground_until: Instant::now(),
                index_state: "idle".into(),
                index_error: String::new(),
                indexed: 0,
                login_window: Instant::now(),
                login_count: 0,
                stats: ProtocolStats::default(),
            }),
            index_lock: AsyncMutex::new(()),
            play_lock: AsyncMutex::new(()),
            probe_slots: Arc::new(Semaphore::new(2)),
            cover_lock: AsyncMutex::new(()),
            cover_wake: Notify::new(),
        }
    }
}
impl Service {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn invalidate(&self) {
        let mut inner = self.lock();
        inner.cancellation.cancel();
        inner.epoch = inner.epoch.wrapping_add(1);
        inner.cancellation = CancellationToken::new();
        inner.sessions.clear();
        inner.plays.clear();
        inner.links.clear();
        inner.worker_epoch = 0;
        inner.cover_worker_epoch = 0;
        inner.cover_task = None;
        inner.foreground = 0;
        inner.foreground_until = Instant::now();
        self.cover_wake.notify_one();
    }

    pub(super) fn begin_foreground(&self) -> u64 {
        let mut inner = self.lock();
        inner.foreground += 1;
        if let Some(task) = &inner.cover_task {
            task.cancel();
        }
        inner.epoch
    }

    pub(super) fn end_foreground(&self, epoch: u64) {
        let mut inner = self.lock();
        if inner.epoch != epoch {
            return;
        }
        inner.foreground = inner.foreground.saturating_sub(1);
        // 连续 Range 请求之间短暂让出连接时，不立刻重复启动解码进程。
        inner.foreground_until = Instant::now() + std::time::Duration::from_secs(2);
        self.cover_wake.notify_one();
    }

    pub(super) fn background_context(&self, context: &Context) -> Option<Context> {
        let mut inner = self.lock();
        if inner.epoch != context.lease.epoch
            || inner.foreground > 0
            || inner.foreground_until > Instant::now()
        {
            return None;
        }
        let mut context = context.clone();
        context.lease.cancelled = context.lease.cancelled.child_token();
        inner.cover_task = Some(context.lease.cancelled.clone());
        Some(context)
    }
    fn lease(&self) -> Lease {
        let inner = self.lock();
        Lease {
            epoch: inner.epoch,
            cancelled: inner.cancellation.clone(),
        }
    }
    pub(super) fn login_budget(&self) -> AppResult<()> {
        let mut inner = self.lock();
        if inner.login_window.elapsed().as_secs() >= 60 {
            inner.login_window = Instant::now();
            inner.login_count = 0;
        }
        inner.login_count += 1;
        if inner.login_count > 20 {
            return Err(AppError::BadRequest(
                "登录尝试过于频繁，请一分钟后重试".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn login(&self, lease: &Lease) -> AppResult<String> {
        let mut inner = self.lock();
        lease.check()?;
        if inner.epoch != lease.epoch {
            return Err(AppError::AppChanged);
        }
        inner
            .sessions
            .retain(|_, s| s.expires > crate::db::now_ts());
        let active = inner
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        inner.plays.retain(|(token, _), _| active.contains(token));
        inner.links.retain(|(token, _), _| active.contains(token));
        if inner.sessions.len() >= 32 {
            return Err(AppError::BadRequest(
                "影视登录会话已达到上限，请退出旧连接".into(),
            ));
        }
        let token = format!(
            "{:032x}{:016x}",
            rand::random::<u128>(),
            rand::random::<u64>()
        );
        inner.sessions.insert(
            token.clone(),
            Login {
                expires: crate::db::now_ts() + LOGIN_TTL,
                lease: Lease {
                    epoch: lease.epoch,
                    cancelled: lease.cancelled.child_token(),
                },
            },
        );
        Ok(token)
    }
    pub(super) fn authenticate(&self, token: &str, lease: &Lease) -> AppResult<Lease> {
        let inner = self.lock();
        lease.check()?;
        if token.len() > 128
            || !inner.sessions.get(token).is_some_and(|s| {
                s.expires > crate::db::now_ts()
                    && s.lease.epoch == lease.epoch
                    && !s.lease.cancelled.is_cancelled()
            })
        {
            return Err(AppError::Forbidden("请使用影视连接账号登录".into()));
        }
        Ok(inner.sessions.get(token).unwrap().lease.clone())
    }
    pub(super) fn logout(&self, token: &str) {
        let mut inner = self.lock();
        if let Some(login) = inner.sessions.remove(token) {
            login.lease.cancelled.cancel();
        }
        inner.plays.retain(|(owner, _), _| owner != token);
        inner.links.retain(|(owner, _), _| owner != token);
    }
    pub(super) fn index_status(&self) -> Value {
        let i = self.lock();
        json!({"state":i.index_state,"error":i.index_error,"entries":i.indexed})
    }
    pub(super) fn set_index_status(&self, lease: &Lease, state: &str, error: &str, count: usize) {
        let mut i = self.lock();
        if lease.epoch == i.epoch {
            i.index_state = state.into();
            i.index_error = error.into();
            i.indexed = count;
        }
    }
    pub(super) fn select_play(&self, token: &str, media: &str, hint: &str) -> AppResult<()> {
        let mut i = self.lock();
        i.plays.retain(|_, p| !p.ended);
        if i.plays.len() >= 128 && !i.plays.contains_key(&(token.into(), media.into())) {
            return Err(AppError::BadRequest("进行中的播放会话过多".into()));
        }
        let key = (token.into(), media.into());
        let previous = i.plays.get(&key);
        // 两次未绑定的起播发生重叠时，不猜测迟到客户端标识的归属。
        let ambiguous = previous.is_some_and(|p| p.read && p.link.is_none() && !p.ended);
        i.plays.insert(
            key,
            Play {
                ambiguous,
                hint: (!hint.is_empty() && hint.len() <= 128).then(|| guid(&["play-link", hint])),
                ..Play::default()
            },
        );
        Ok(())
    }
    pub(super) fn play(&self, token: &str, media: &str) -> Option<Play> {
        self.lock()
            .plays
            .get(&(token.into(), media.into()))
            .cloned()
    }
    pub(super) fn activate(
        &self,
        token: &str,
        media: &str,
        serial: u64,
        lease: &Lease,
    ) -> AppResult<()> {
        let mut i = self.lock();
        lease.check()?;
        if i.epoch != lease.epoch {
            return Err(AppError::AppChanged);
        }
        let play = i.plays.entry((token.into(), media.into())).or_default();
        if play.ended {
            *play = Play::default();
        }
        play.serial = serial;
        play.read = true;
        Ok(())
    }
    pub(super) fn bind_link(&self, token: &str, media: &str, link: &str) -> AppResult<Option<u64>> {
        let mut i = self.lock();
        i.stats.records += 1;
        let link_key = (token.into(), guid(&["play-link", link]));
        let key = (token.into(), media.into());
        let Some(play) = i.plays.get(&key).cloned() else {
            return Ok(None);
        };
        if !play.read || play.ended || play.ambiguous {
            i.stats.unbound_records += 1;
            return Ok(None);
        }
        if let Some(hint) = &play.hint {
            if hint == &link_key.1 {
                i.stats.hint_matches += 1;
            } else {
                i.stats.hint_mismatches += 1;
            }
        }
        if let Some((bound_media, serial)) = i.links.get(&link_key) {
            return Ok((bound_media == media && *serial == play.serial).then_some(*serial));
        }
        if play.link.is_some() {
            i.stats.link_changes += 1;
            return Ok(None);
        }
        if i.links.len() >= 512 {
            return Err(AppError::Conflict(
                "播放关联记录达到上限，请重新登录影视账号".into(),
            ));
        }
        i.links.insert(link_key, (media.into(), play.serial));
        i.plays.get_mut(&key).unwrap().link = Some(guid(&["play-link", link]));
        Ok(Some(play.serial))
    }
    pub(super) fn quit(&self, token: &str, link: &str) {
        let mut i = self.lock();
        let key = (token.into(), guid(&["play-link", link]));
        if let Some((media, serial)) = i.links.get(&key).cloned()
            && let Some(play) = i.plays.get_mut(&(token.into(), media))
            && play.serial == serial
        {
            play.ended = true;
        }
    }
    pub(super) fn record_result(&self, saved: bool) {
        let mut i = self.lock();
        if saved {
            i.stats.saved += 1;
        } else {
            i.stats.stale += 1;
        }
    }
    #[cfg(test)]
    pub(super) fn protocol_stats(&self) -> Value {
        serde_json::to_value(&self.lock().stats).unwrap()
    }
}

pub(super) fn guid(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    crate::engine::upload::to_hex(&Sha256::digest(parts.join("\0").as_bytes()))[..32].to_owned()
}

pub(super) async fn context(state: &AppState, must_run: bool) -> AppResult<Context> {
    state.vfs.db.require_background_enabled()?;
    let lease = state.vfs.db.fntv.lease();
    let app = installed(state, APP_ID).await?;
    if !app.enabled {
        return Err(AppError::Forbidden("影视插件已停用".into()));
    }
    app.manifest.require("fntv.service")?;
    let (config, config_revision) = data::config(&state.vfs.db).await?;
    if must_run && !config.running {
        return Err(AppError::Forbidden("影视服务尚未启动".into()));
    }
    lease.check()?;
    Ok(Context {
        app,
        config,
        config_revision,
        lease,
    })
}

pub(super) async fn roots(
    state: &AppState,
    context: &Context,
) -> AppResult<Vec<(Library, String)>> {
    let mut roots = Vec::new();
    for library in &context.config.libraries {
        if let Some(path) = state.vfs.db.full_path_if_visible(library.root_id).await?
            && state.vfs.db.node(library.root_id).await?.is_dir
            && scope::check(state, &context.app, &path).await.is_ok()
        {
            roots.push((library.clone(), path));
        }
    }
    context.lease.check()?;
    Ok(roots)
}

pub(super) async fn authorized(
    state: &AppState,
    context: &Context,
    id: i64,
    version: &str,
) -> AppResult<(Node, String)> {
    context.lease.check()?;
    let current = installed(state, APP_ID).await?;
    if !current.enabled || current.revision != context.app.revision {
        return Err(AppError::AppChanged);
    }
    current.manifest.require("fntv.service")?;
    let path = state
        .vfs
        .db
        .full_path_if_visible(id)
        .await?
        .ok_or(AppError::NotFound)?;
    scope::check(state, &current, &path).await?;
    let node = state.vfs.db.node(id).await?;
    if node.is_dir || node.content_version() != version {
        return Err(AppError::Conflict("文件内容已改变，请刷新媒体库".into()));
    }
    let libraries = roots(state, context).await?;
    if !libraries
        .iter()
        .any(|(_, root)| scope::in_scope(std::slice::from_ref(root), &path))
    {
        return Err(AppError::Forbidden("文件已移出影视媒体库".into()));
    }
    context.lease.check()?;
    Ok((node, path))
}

pub(crate) fn router(state: AppState) -> axum::Router {
    // 恢复已由用户启动的服务；新安装和已停止配置不会触发索引或取帧。
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let startup = state.clone();
        runtime.spawn(async move {
            if !startup.restore.shutdown.is_cancelled()
                && context(&startup, true).await.is_ok()
                && !startup.restore.shutdown.is_cancelled()
            {
                index::start_worker(&startup);
            }
        });
    }
    axum::Router::new()
        .route("/trimcon", axum::routing::any(protocol::handle))
        .route("/v", axum::routing::any(protocol::handle))
        .route("/v/{*path}", axum::routing::any(protocol::handle))
        .route(
            "/tgdrive-poster/{guid}",
            axum::routing::any(protocol::handle),
        )
        .with_state(state)
}

pub(super) async fn invoke(
    state: &AppState,
    app: &InstalledApp,
    method: &str,
    params: Value,
) -> AppResult<Value> {
    if app.manifest.id != APP_ID {
        return Err(AppError::Forbidden("服务能力归属无效".into()));
    }
    let _guard = super::INSTALL_LOCK.lock().await;
    let context = context(state, false).await?;
    if context.app.revision != app.revision {
        return Err(AppError::AppChanged);
    }
    match method {
        "fntv.config.get" => {
            let available = roots(state, &context).await?;
            let libraries: Vec<_> = context.config.libraries.iter().map(|l| {
                let path = available.iter().find(|(r, _)| r.id == l.id).map(|(_, p)| p);
                json!({"id":l.id,"name":l.name,"kind":l.kind,"root_id":l.root_id,"path":path,"authorized":path.is_some()})
            }).collect();
            Ok(
                json!({"username":context.config.username,"account_configured":!context.config.verifier.is_empty(),"libraries":libraries,"running":context.config.running,"limits":probe::limits()}),
            )
        }
        "fntv.service.status" => Ok(
            json!({"running":context.config.running,"index":state.vfs.db.fntv.index_status(),"probe_available":probe::available().await,"limits":probe::limits(),"covers":covers::status(state,&context).await?}),
        ),
        "fntv.account.set" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Account {
                username: String,
                password: String,
            }
            let account: Account = super::files::parse(params)?;
            if account.username.trim().is_empty()
                || account.username.len() > 64
                || account.username.chars().any(char::is_control)
                || !(8..=128).contains(&account.password.len())
                || account.password.chars().any(char::is_control)
            {
                return Err(AppError::BadRequest(
                    "账号需为 1 至 64 字节，密码需为 8 至 128 字节且不含控制字符".into(),
                ));
            }
            use sha2::{Digest, Sha256};
            let digest =
                crate::engine::upload::to_hex(&Sha256::digest(account.password.as_bytes()));
            let verifier = crate::password::hash_blocking(digest).await?;
            let mut config = context.config.clone();
            config.username = account.username.trim().into();
            config.verifier = verifier;
            data::save_config(&state.vfs.db, &context, &config).await?;
            state.vfs.db.fntv.invalidate();
            if config.running {
                index::start_worker(state);
            }
            Ok(json!({"ok":true}))
        }
        "fntv.libraries.set" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Libraries {
                libraries: Vec<Library>,
            }
            let mut input: Libraries = super::files::parse(params)?;
            if input.libraries.len() > 32 {
                return Err(AppError::BadRequest("媒体库最多 32 个".into()));
            }
            let mut ids = std::collections::HashSet::new();
            for l in &mut input.libraries {
                if l.id.is_empty() {
                    l.id = guid(&[&crate::db::random_token(32)]);
                }
                if l.id.len() != 32
                    || !l.id.bytes().all(|c| c.is_ascii_hexdigit())
                    || !ids.insert(l.id.clone())
                    || l.name.trim().is_empty()
                    || l.name.len() > 160
                    || l.name.chars().any(char::is_control)
                    || !["movie", "tv"].contains(&l.kind.as_str())
                    || l.root_id <= 0
                {
                    return Err(AppError::BadRequest(
                        "媒体库名称、类型、目录身份或唯一标识无效".into(),
                    ));
                }
                let path = state.vfs.db.full_path_if_visible(l.root_id).await?;
                let existing =
                    context.config.libraries.iter().any(|old| {
                        old.id == l.id && old.root_id == l.root_id && old.kind == l.kind
                    });
                if let Some(path) = path {
                    if !state.vfs.db.node(l.root_id).await?.is_dir {
                        return Err(AppError::NotDirectory);
                    }
                    if !existing {
                        scope::check(state, app, &path).await?;
                    }
                } else if !existing {
                    return Err(AppError::NotFound);
                }
                l.name = l.name.trim().into();
            }
            let mut config = context.config.clone();
            config.libraries = input.libraries;
            if config.libraries.is_empty() {
                config.running = false;
            }
            data::save_config(&state.vfs.db, &context, &config).await?;
            state.vfs.db.fntv.invalidate();
            if config.running {
                index::start_worker(state);
            }
            Ok(json!({"ok":true,"libraries":config.libraries}))
        }
        "fntv.service.start" => {
            if context.config.username.is_empty()
                || context.config.verifier.is_empty()
                || roots(state, &context).await?.is_empty()
            {
                return Err(AppError::BadRequest(
                    "请先设置影视连接账号并创建至少一个已授权媒体库".into(),
                ));
            }
            if !probe::available().await {
                return Err(AppError::BadRequest(
                    "宿主未安装 ffprobe，无法提供真实媒体信息".into(),
                ));
            }
            let mut config = context.config.clone();
            config.running = true;
            data::save_config(&state.vfs.db, &context, &config).await?;
            state.vfs.db.fntv.invalidate();
            index::start_worker(state);
            Ok(json!({"ok":true,"running":true}))
        }
        "fntv.service.stop" => {
            let mut config = context.config.clone();
            config.running = false;
            data::save_config(&state.vfs.db, &context, &config).await?;
            state.vfs.db.fntv.invalidate();
            Ok(json!({"ok":true,"running":false}))
        }
        "fntv.index.refresh" => {
            index::refresh(state, &context, None).await?;
            covers::schedule(state, &context, true).await?;
            if context.config.running {
                covers::start_worker(state);
            }
            Ok(state.vfs.db.fntv.index_status())
        }
        _ => Err(AppError::BadRequest("不支持的影视管理能力".into())),
    }
}
