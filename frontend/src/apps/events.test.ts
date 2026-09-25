import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

/** 用受控的假 EventSource 驱动订阅与转发逻辑。 */
class FakeEventSource {
  static instances: FakeEventSource[] = []
  static readonly CLOSED = 2
  readyState = 0
  onopen: (() => void) | null = null
  onerror: (() => void) | null = null
  handlers = new Map<string, (event: { data: string }) => void>()
  constructor(public url: string) { FakeEventSource.instances.push(this) }
  addEventListener(name: string, handler: (event: { data: string }) => void) { this.handlers.set(name, handler) }
  close() { /* 由测试直接丢弃实例。 */ }
  emit(name: string, payload: unknown) { this.handlers.get(name)?.({ data: JSON.stringify(payload) }) }
}

describe('宿主事件流', () => {
  beforeEach(() => { vi.resetModules(); FakeEventSource.instances = []; vi.stubGlobal('EventSource', FakeEventSource) })
  afterEach(() => { vi.unstubAllGlobals() })
  it('首个订阅建立连接，事件按需分发并过滤畸形载荷', async () => {
    const { onHostEvent } = await import('./events')
    const seen: unknown[] = []
    const off = onHostEvent((event) => seen.push(event))
    expect(FakeEventSource.instances).toHaveLength(1)
    expect(FakeEventSource.instances[0].url).toBe('/api/events')
    const source = FakeEventSource.instances[0]
    source.emit('app.storage', { kind: 'app.storage', app_id: 'books', key: 'progress:1' })
    source.emit('files', { kind: 'files' })
    source.emit('files', { kind: 'files', paths: ['/上传', '/书'] })
    // 畸形或超限的路径列表退化为范围未知。
    source.emit('files', { kind: 'files', paths: ['/a', 1] })
    source.emit('files', { kind: 'files', paths: Array.from({ length: 65 }, (_, i) => `/d${i}`) })
    source.emit('app.storage', '不是对象')
    expect(seen).toEqual([
      { kind: 'app.storage', app_id: 'books', key: 'progress:1' },
      { kind: 'files', app_id: undefined, key: undefined },
      { kind: 'files', app_id: undefined, key: undefined, paths: ['/上传', '/书'] },
      { kind: 'files', app_id: undefined, key: undefined },
      { kind: 'files', app_id: undefined, key: undefined },
    ])
    off()
  })
  it('断线重连后向监听器发出 sync.hint', async () => {
    const { onHostEvent } = await import('./events')
    const hints: number[] = []
    const off = onHostEvent((event) => { if (event.kind === 'sync.hint') hints.push(1) })
    const source = FakeEventSource.instances[0]
    source.onopen?.()
    expect(hints).toHaveLength(0) // 首次连接触发不算重连。
    source.onopen?.()
    expect(hints).toHaveLength(1)
    off()
  })
  it('服务端关闭连接后按指数退避重建，恢复后发出 sync.hint', async () => {
    vi.useFakeTimers()
    try {
      const { onHostEvent } = await import('./events')
      const hints: number[] = []
      const off = onHostEvent((event) => { if (event.kind === 'sync.hint') hints.push(1) })
      const first = FakeEventSource.instances[0]
      first.onopen?.()
      // 网络抖动时浏览器会自行重连（readyState 仍为 CONNECTING），不应另建连接。
      first.onerror?.()
      vi.advanceTimersByTime(60_000)
      expect(FakeEventSource.instances).toHaveLength(1)
      // 401/502 时连接被关闭：1 秒后重建。
      first.readyState = FakeEventSource.CLOSED
      first.onerror?.()
      vi.advanceTimersByTime(999)
      expect(FakeEventSource.instances).toHaveLength(1)
      vi.advanceTimersByTime(1)
      expect(FakeEventSource.instances).toHaveLength(2)
      // 连续失败时间隔翻倍。
      const second = FakeEventSource.instances[1]
      second.readyState = FakeEventSource.CLOSED
      second.onerror?.()
      vi.advanceTimersByTime(1_999)
      expect(FakeEventSource.instances).toHaveLength(2)
      vi.advanceTimersByTime(1)
      expect(FakeEventSource.instances).toHaveLength(3)
      FakeEventSource.instances[2].onopen?.()
      expect(hints).toHaveLength(1)
      off()
    } finally { vi.useRealTimers() }
  })
})
