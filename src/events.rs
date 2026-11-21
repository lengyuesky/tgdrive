//! 宿主事件总线：单用户进程内广播，SSE 通道向各浏览器会话转发。
//! 事件只携带粗粒度标识，不携带文件内容。

use serde::Serialize;
use tokio::sync::broadcast;

/// 单个文件事件最多携带的目录数；超过则退化为范围未知（整体重扫）。
pub const MAX_EVENT_PATHS: usize = 64;

/// 变更事件。`kind` 为稳定协议名，前端按 `kind` 过滤。
#[derive(Clone, Debug, Serialize)]
pub struct HostEvent {
    pub kind: &'static str,
    /// 文件树事件涉及的目录（发生变化的列表所在目录，规范化绝对路径）。
    /// `None` 表示范围未知，客户端应整体刷新。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl HostEvent {
    /// 文件树粗粒度变更：客户端失效本地列表并按需重拉。
    pub fn files() -> Self {
        Self {
            kind: "files",
            paths: None,
        }
    }

    /// 已知变更目录的文件树事件。
    pub fn files_at(mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        Self {
            paths: (paths.len() <= MAX_EVENT_PATHS).then_some(paths),
            ..Self::files()
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<HostEvent>,
}

impl EventBus {
    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.sender.subscribe()
    }
}

/// 容量覆盖秒级突发足够；无订阅者时发送失败是正常情况，直接丢弃。
pub fn bus() -> EventBus {
    EventBus {
        sender: broadcast::channel(256).0,
    }
}

pub fn emit(bus: &EventBus, event: HostEvent) {
    let _ = bus.sender.send(event);
}
