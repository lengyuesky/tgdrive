//! SSE 与数据库提交观察。事件只提示本应用的变化，不取代持久文件日志与实时授权。

use super::runtime::CallerSession;
use super::store::{self, Installation};
use super::{AppPlatform, PlatformError, PlatformResult};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tgdrive_contract::{error_code, events::*, files::DirectoryScope, http::*};
use tokio::sync::{OwnedSemaphorePermit, broadcast};

struct Subscription {
    platform: Arc<AppPlatform>,
    session: Arc<CallerSession>,
    pending: VecDeque<HostEvent>,
    receiver: broadcast::Receiver<HostEvent>,
    _permit: OwnedSemaphorePermit,
    epoch: u64,
    revocations: tokio::sync::watch::Receiver<u64>,
}

pub(crate) async fn response(
    platform: Arc<AppPlatform>,
    session: Arc<CallerSession>,
    headers: &HeaderMap,
) -> PlatformResult<Response> {
    let permit = session.events.clone().try_acquire_owned().map_err(|_| {
        PlatformError::new(error_code::QUOTA_EXCEEDED, "当前页面事件连接已达到上限")
    })?;
    let last = headers.get("last-event-id").and_then(|v| v.to_str().ok());
    if last.is_some_and(|v| v.len() > MAX_CURSOR_BYTES as usize) {
        return Err(PlatformError::new(
            error_code::INVALID_CURSOR,
            "事件游标超过上限",
        ));
    }
    let (pending, receiver) = platform.events.subscribe(&session.app_id, last)?;
    let stream = futures_util::stream::unfold(
        Subscription {
            revocations: platform.host.auth.revocation_signal(),
            platform,
            session,
            pending,
            receiver,
            _permit: permit,
            epoch: u64::MAX,
        },
        |mut state| async move {
            loop {
                if state.session.cancelled.is_cancelled()
                    || state.session.expires_at.load(Ordering::Acquire) <= super::now().0
                    || (state.session.is_page()
                        && state.session.owner_epoch != state.platform.host.auth.device_epoch())
                {
                    return None;
                }
                let epoch = state.platform.host.vfs.db.app_epoch.load(Ordering::Acquire);
                if epoch != state.epoch {
                    let mut connection = match state.platform.host.vfs.db.pool.acquire().await {
                        Ok(connection) => connection,
                        Err(_) => return None,
                    };
                    match store::installation(&mut connection, &state.session.app_id).await {
                        Ok(app) if app.check(&state.session.generation).is_ok() => {}
                        _ => return None,
                    }
                    state.epoch = epoch;
                }
                if let Some(event) = state.pending.pop_front() {
                    let event = Event::default()
                        .id(&event.cursor)
                        .event("host")
                        .json_data(event)
                        .expect("事件由宿主序列化");
                    return Some((Ok::<_, std::convert::Infallible>(event), state));
                }
                let cancelled = state.session.cancelled.clone();
                tokio::select! {
                    _=state.revocations.changed(),if state.session.is_page()=>return None,
                    _=cancelled.cancelled()=>return None,
                    _=state.platform.host.restore.shutdown.cancelled()=>return None,
                    received=state.receiver.recv()=>match received {
                        Ok(event)=>state.pending.push_back(event),
                        Err(broadcast::error::RecvError::Closed)=>return None,
                        Err(broadcast::error::RecvError::Lagged(_))=>{
                            let sync=state.platform.events.sync(&state.session.app_id,"overflow").ok()?;
                            let (mut events,receiver)=state.platform.events.subscribe(&state.session.app_id,Some(&sync.cursor)).ok()?;
                            events.push_front(sync);state.pending=events;state.receiver=receiver;
                        }
                    },
                    _=tokio::time::sleep(Duration::from_secs(SSE_HEARTBEAT_SECONDS))=>{},
                }
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(SSE_HEARTBEAT_SECONDS))
                .text("保持连接"),
        )
        .into_response())
}

pub(crate) fn start(platform: &Arc<AppPlatform>) {
    let weak = Arc::downgrade(platform);
    let receiver = platform.host.vfs.db.app_changes.subscribe();
    tokio::spawn(observe(weak, receiver));
}
async fn observe(platform: Weak<AppPlatform>, mut changes: tokio::sync::watch::Receiver<u64>) {
    let mut previous = BTreeMap::<String, Installation>::new();
    let mut checkpoint = 0i64;
    let mut initialized = false;
    let mut storage = None;
    loop {
        if initialized {
            if changes.changed().await.is_err() {
                return;
            }
            // 合并同一短窗口内的已提交变化，空闲时不轮询数据库。
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let Some(platform) = platform.upgrade() else {
            return;
        };
        if platform.host.restore.shutdown.is_cancelled() {
            platform.sessions.revoke_all();
            return;
        }
        let mut tx = match platform.host.vfs.db.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => continue,
        };
        let records: Vec<Installation> = match sqlx::query_as("SELECT * FROM app_installations")
            .fetch_all(&mut *tx)
            .await
        {
            Ok(records) => records,
            Err(_) => continue,
        };
        let maximum: i64 = match sqlx::query_scalar(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='app_file_changes'),0)",
        )
        .fetch_one(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(_) => continue,
        };
        let current_storage = platform.host.rt.current().is_configured();
        let mut current = BTreeMap::new();
        for app in records {
            if let Some(old) = previous.get(&app.id) {
                if old.generation != app.generation || old.enabled != app.enabled {
                    let _ = platform.events.emit(
                        &app.id,
                        event_name::APP_CHANGED,
                        &AppChangedEvent {
                            generation: app.generation.clone(),
                            enabled: app.enabled,
                        },
                    );
                    platform
                        .sessions
                        .revoke_other_generations(&app.id, &app.generation);
                }
                if old.scope_revision != app.scope_revision {
                    let _ = platform.events.emit(
                        &app.id,
                        event_name::SCOPE_CHANGED,
                        &ScopeChangedEvent {
                            revision: app.scope_revision.clone(),
                        },
                    );
                }
            }
            if initialized && maximum > checkpoint && app.enabled {
                let related = related_changes(&mut tx, &app, checkpoint, maximum)
                    .await
                    .unwrap_or(false);
                if related
                    && let Ok(cursor) =
                        super::query::checkpoint(&mut tx, &platform.cursors, &app).await
                {
                    let _ = platform.events.emit(
                        &app.id,
                        event_name::FILES_CHANGED,
                        &FilesChangedEvent {
                            change_cursor: cursor,
                        },
                    );
                }
            }
            if storage.is_some_and(|previous| previous != current_storage) {
                let _ = platform.events.emit(
                    &app.id,
                    event_name::STORAGE_CHANGED,
                    &StorageChangedEvent {
                        readable: current_storage,
                        writable: current_storage,
                    },
                );
            }
            current.insert(app.id.clone(), app);
        }
        if tx.commit().await.is_err() {
            continue;
        }
        for id in previous.keys() {
            if !current.contains_key(id) {
                platform.sessions.revoke_app(id);
                platform.events.remove(id);
            }
        }
        previous = current;
        checkpoint = maximum;
        storage = Some(current_storage);
        initialized = true;
    }
}
async fn related_changes(
    connection: &mut sqlx::SqliteConnection,
    app: &Installation,
    after: i64,
    until: i64,
) -> PlatformResult<bool> {
    if !app.manifest()?.files.read {
        return Ok(false);
    }
    let roots = match app.directory_scope()? {
        DirectoryScope::None => return Ok(false),
        DirectoryScope::All { .. } => vec![crate::db::ROOT_ID],
        DirectoryScope::Selected { directories } => directories
            .into_iter()
            .map(|root| store::node_id(&root.directory_id))
            .collect::<PlatformResult<Vec<_>>>()?,
    };
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT EXISTS(SELECT 1 FROM app_file_changes c WHERE c.seq>",
    );
    query.push_bind(after).push(" AND c.seq<=").push_bind(until).push(" AND (EXISTS(SELECT 1 FROM app_ancestors a WHERE a.node_id=c.node_id AND a.ancestor_id IN(");
    let mut separated = query.separated(",");
    for id in &roots {
        separated.push_bind(*id);
    }
    separated.push_unseparated(")) OR EXISTS(SELECT 1 FROM json_each(c.previous_ancestors) old WHERE CAST(old.value AS INTEGER) IN(");
    let mut separated = query.separated(",");
    for id in &roots {
        separated.push_bind(*id);
    }
    separated.push_unseparated("))) LIMIT 1)");
    Ok(query.build_query_scalar().fetch_one(connection).await?)
}
