//! 宿主事件总线：单用户进程内广播，SSE 通道向各浏览器会话转发。
//! 事件只携带粗粒度标识，不携带文件内容或应用数据值。

use serde::Serialize;
use tokio::sync::broadcast;

/// 变更事件。`kind` 为稳定协议名，前端按 `kind` 过滤转发。
#[derive(Clone, Debug, Serialize)]
pub struct HostEvent {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl HostEvent {
    /// 文件树粗粒度变更：不携带路径，插件据此失效本地列表并按需重拉。
    pub fn files() -> Self {
        Self {
            kind: "files",
            app_id: None,
            key: None,
        }
    }

    pub fn app(kind: &'static str, app_id: &str, key: Option<String>) -> Self {
        Self {
            kind,
            app_id: Some(app_id.into()),
            key,
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
