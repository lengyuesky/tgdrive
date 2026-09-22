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

pub type EventBus = broadcast::Sender<HostEvent>;

/// 容量覆盖秒级突发足够；无订阅者时发送失败是正常情况，直接丢弃。
pub fn bus() -> EventBus {
    broadcast::channel(256).0
}

pub fn emit(bus: &EventBus, event: HostEvent) {
    let _ = bus.send(event);
}
