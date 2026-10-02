//! 宿主事件总线：单用户进程内广播，SSE 通道向各浏览器会话转发。
//! 事件只携带粗粒度标识，不携带文件内容或应用数据值。

use serde::Serialize;
use tokio::sync::broadcast;

/// 单个文件事件最多携带的目录数；超过则退化为范围未知（整体重扫）。
pub const MAX_EVENT_PATHS: usize = 64;

/// 变更事件。`kind` 为稳定协议名，前端按 `kind` 过滤转发。
#[derive(Clone, Debug, Serialize)]
pub struct HostEvent {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 文件树事件涉及的目录（发生变化的列表所在目录，规范化绝对路径）。
    /// `None` 表示范围未知：宿主页无法按应用目录范围过滤，插件应整体重扫。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl HostEvent {
    /// 文件树粗粒度变更：不携带路径，插件据此失效本地列表并按需重拉。
    pub fn files() -> Self {
        Self {
            kind: "files",
            app_id: None,
            key: None,
            paths: None,
        }
    }

    /// 已知变更目录的文件树事件；宿主页据此只转发给目录范围相关的应用。
    pub fn files_at(mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        Self {
            paths: (paths.len() <= MAX_EVENT_PATHS).then_some(paths),
            ..Self::files()
        }
    }

    pub fn app(kind: &'static str, app_id: &str, key: Option<String>) -> Self {
        Self {
            kind,
            app_id: Some(app_id.into()),
            key,
            paths: None,
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<HostEvent>,
    journal: std::sync::Arc<std::sync::Mutex<FileJournal>>,
}
struct FileJournal {
    epoch: String,
    sequence: u64,
    entries: std::collections::VecDeque<(u64, Option<Vec<String>>)>,
}
impl EventBus {
    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.sender.subscribe()
    }
    pub fn file_changes(&self, cursor: Option<&str>) -> (String, Option<Vec<String>>) {
        let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
        let current = format!("{}:{}", journal.epoch, journal.sequence);
        let after = cursor
            .and_then(|c| c.split_once(':'))
            .filter(|(epoch, _)| *epoch == journal.epoch)
            .and_then(|(_, seq)| seq.parse::<u64>().ok());
        let Some(after) = after.filter(|seq| {
            *seq <= journal.sequence
                && journal
                    .entries
                    .front()
                    .is_none_or(|(first, _)| seq.saturating_add(1) >= *first)
        }) else {
            return (current, None);
        };
        let mut paths = std::collections::BTreeSet::new();
        for (_, changed) in journal.entries.iter().filter(|(seq, _)| *seq > after) {
            let Some(changed) = changed else {
                return (current, None);
            };
            paths.extend(changed.iter().cloned());
            if paths.len() > MAX_EVENT_PATHS {
                return (current, None);
            }
        }
        (current, Some(paths.into_iter().collect()))
    }
}

/// 容量覆盖秒级突发足够；无订阅者时发送失败是正常情况，直接丢弃。
pub fn bus() -> EventBus {
    EventBus {
        sender: broadcast::channel(256).0,
        journal: std::sync::Arc::new(std::sync::Mutex::new(FileJournal {
            epoch: format!("{:032x}", rand::random::<u128>()),
            sequence: 0,
            entries: Default::default(),
        })),
    }
}

pub fn emit(bus: &EventBus, event: HostEvent) {
    if event.kind == "files" {
        let mut journal = bus.journal.lock().unwrap_or_else(|e| e.into_inner());
        journal.sequence += 1;
        let sequence = journal.sequence;
        journal.entries.push_back((sequence, event.paths.clone()));
        while journal.entries.len() > 256 {
            journal.entries.pop_front();
        }
    }
    let _ = bus.sender.send(event);
}
