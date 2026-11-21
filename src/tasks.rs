//! 后台任务监管：心跳、异常重启与统一取消。

use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Serialize)]
pub struct TaskSnapshot {
    pub name: String,
    pub running: bool,
    pub started_at: i64,
    pub last_heartbeat: i64,
    pub restarts: u64,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct TaskState {
    running: bool,
    started_at: i64,
    last_heartbeat: i64,
    launches: u64,
    last_error: Option<String>,
}

#[derive(Clone)]
pub struct TaskManager {
    states: Arc<Mutex<HashMap<&'static str, TaskState>>>,
    cancel: CancellationToken,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            cancel: CancellationToken::new(),
        }
    }

    pub fn supervise<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn(TaskReporter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let manager = self.clone();
        let factory = Arc::new(factory);
        tokio::spawn(async move {
            loop {
                if manager.cancel.is_cancelled() {
                    break;
                }
                let reporter = manager.reporter(name);
                reporter.started();
                let mut child = tokio::spawn(factory(reporter.clone()));
                let ended_unexpectedly = tokio::select! {
                    _ = manager.cancel.cancelled() => {
                        child.abort();
                        let _ = child.await;
                        false
                    }
                    result = &mut child => {
                        match result {
                            Ok(()) => tracing::warn!(task = name, "后台任务意外退出，将自动重启"),
                            Err(error) => tracing::error!(task = name, "后台任务崩溃，将自动重启: {error}"),
                        }
                        true
                    }
                };
                reporter.stopped();
                if !ended_unexpectedly || !reporter.sleep(Duration::from_secs(5)).await {
                    break;
                }
            }
        });
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn snapshots(&self) -> Vec<TaskSnapshot> {
        let mut items: Vec<_> = self
            .states
            .lock()
            .unwrap()
            .iter()
            .map(|(name, state)| TaskSnapshot {
                name: (*name).to_string(),
                running: state.running,
                started_at: state.started_at,
                last_heartbeat: state.last_heartbeat,
                restarts: state.launches.saturating_sub(1),
                last_error: state.last_error.clone(),
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    pub fn healthy(&self, required: &[&str], max_stale_secs: i64) -> bool {
        let now = crate::db::now_ts();
        let states = self.states.lock().unwrap();
        required.iter().all(|name| {
            states
                .get(name)
                .is_some_and(|state| state.running && now - state.last_heartbeat <= max_stale_secs)
        })
    }

    fn reporter(&self, name: &'static str) -> TaskReporter {
        TaskReporter {
            name,
            states: self.states.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TaskReporter {
    name: &'static str,
    states: Arc<Mutex<HashMap<&'static str, TaskState>>>,
    cancel: CancellationToken,
}

impl TaskReporter {
    fn started(&self) {
        let now = crate::db::now_ts();
        let mut states = self.states.lock().unwrap();
        let state = states.entry(self.name).or_default();
        state.running = true;
        state.started_at = now;
        state.last_heartbeat = now;
        state.launches += 1;
    }

    fn stopped(&self) {
        if let Some(state) = self.states.lock().unwrap().get_mut(self.name) {
            state.running = false;
        }
    }

    pub fn heartbeat(&self) {
        if let Some(state) = self.states.lock().unwrap().get_mut(self.name) {
            state.last_heartbeat = crate::db::now_ts();
        }
    }

    pub fn report_error(&self, error: &str) {
        if let Some(state) = self.states.lock().unwrap().get_mut(self.name) {
            state.last_heartbeat = crate::db::now_ts();
            state.last_error = Some(error.chars().take(300).collect());
        }
    }

    pub fn clear_error(&self) {
        if let Some(state) = self.states.lock().unwrap().get_mut(self.name) {
            state.last_error = None;
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 可取消的休眠；长等待期间每 30 秒刷新一次心跳。
    pub async fn sleep(&self, duration: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return !self.cancel.is_cancelled();
            }
            let wait = (deadline - now).min(HEARTBEAT_INTERVAL);
            tokio::select! {
                _ = self.cancel.cancelled() => return false,
                _ = tokio::time::sleep(wait) => self.heartbeat(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reporter_updates_health_and_honors_cancel() {
        let manager = TaskManager::new();
        let reporter = manager.reporter("test");
        reporter.started();
        assert!(manager.healthy(&["test"], 1));
        reporter.report_error("示例错误");
        assert_eq!(
            manager.snapshots()[0].last_error.as_deref(),
            Some("示例错误")
        );
        manager.cancel();
        assert!(!reporter.sleep(Duration::from_secs(60)).await);
    }
}
