/** 单用户事件流：进程内单连接、跨视图共享；应用页把相关事件转发进沙箱端口。 */

export interface HostEventPayload {
  kind: 'app.storage' | 'app.settings' | 'app.scope' | 'files' | 'sync.hint'
  app_id?: string
  key?: string
  /** 文件事件涉及的目录（变更列表所在目录）；缺省表示范围未知，需整体刷新。 */
  paths?: string[]
}

/** 服务端单个事件最多携带的目录数；更多时服务端已退化为范围未知。 */
const MAX_EVENT_PATHS = 64

const listeners = new Set<(event: HostEventPayload) => void>()
let source: EventSource | null = null
let wasConnected = false

function dispatch(event: HostEventPayload) {
  for (const listener of [...listeners]) {
    try { listener(event) } catch { /* 单个监听器异常不影响其他监听。 */ }
  }
}

function connect() {
  if (source || typeof EventSource === 'undefined') return
  const stream = new EventSource('/api/events')
  source = stream
  stream.onopen = () => {
    if (wasConnected) dispatch({ kind: 'sync.hint' })
    wasConnected = true
  }
  for (const name of ['app.storage', 'app.settings', 'app.scope', 'files'] as const) {
    stream.addEventListener(name, (event) => {
      try {
        const raw = (event as MessageEvent<string>).data
        const data = raw ? JSON.parse(raw) as Record<string, unknown> : {}
        if (typeof data !== 'object' || data === null) return
        const paths = Array.isArray(data.paths) && data.paths.length <= MAX_EVENT_PATHS && data.paths.every((path) => typeof path === 'string')
          ? data.paths as string[] : undefined
        dispatch({ kind: name, app_id: typeof data.app_id === 'string' ? data.app_id : undefined, key: typeof data.key === 'string' ? data.key : undefined, paths })
      } catch { /* 忽略畸形载荷。 */ }
    })
  }
}

/** 订阅宿主事件；返回取消函数。首个订阅时建立 SSE 连接，页面卸载由浏览器自动断开。 */
export function onHostEvent(listener: (event: HostEventPayload) => void): () => void {
  listeners.add(listener)
  connect()
  return () => { listeners.delete(listener) }
}
