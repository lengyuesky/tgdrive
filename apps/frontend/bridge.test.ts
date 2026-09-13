import { describe, expect, it, vi } from 'vitest'
import { connectAppBridge, isAppRequest } from './bridge'

function channel() {
  const port = { onmessage: null as ((event: MessageEvent) => Promise<void>) | null, postMessage: vi.fn(), start: vi.fn(), close: vi.fn() }
  return { port, send: (data: unknown) => port.onmessage?.({ data } as MessageEvent) }
}

describe('应用能力通道', () => {
  it('拒绝后台接口、原型字段形式的方法和过大参数', () => {
    for (const method of ['/admin/config', 'constructor', 'toString']) expect(isAppRequest({ type: 'request', id: 1, method, params: {} })).toBe(false)
    expect(isAppRequest({ type: 'request', id: 1, method: 'settings.patch', params: { text: 'x'.repeat(65537) } })).toBe(false)
    expect(isAppRequest({ type: 'request', id: 1, method: 'files.search', params: [] })).toBe(false)
  })

  it('关闭应用后忽略在途调用结果', async () => {
    const { port, send } = channel()
    let finish!: (value: unknown) => void
    const invoke = vi.fn(() => new Promise((resolve) => { finish = resolve }))
    const disconnect = connectAppBridge(port as unknown as MessagePort, invoke, String)
    const pending = send({ type: 'request', id: 1, method: 'files.search', params: {} })
    disconnect()
    finish({ results: [] })
    await pending
    expect(port.postMessage).not.toHaveBeenCalled()
    expect(port.close).toHaveBeenCalledOnce()
  })

  it('限制并发并保留已发出的请求', async () => {
    const { port, send } = channel()
    const complete: (() => void)[] = []
    const invoke = vi.fn(() => new Promise((resolve) => complete.push(() => resolve({ ok: true }))))
    const disconnect = connectAppBridge(port as unknown as MessagePort, invoke, String)
    const pending = Array.from({ length: 17 }, (_, index) => send({ type: 'request', id: index + 1, method: 'app.ping', params: {} }))
    expect(invoke).toHaveBeenCalledTimes(16)
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ id: 17, error: expect.any(String) }))
    complete.forEach((finish) => finish())
    await Promise.all(pending)
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ id: 1, result: { ok: true } }))
    disconnect()
  })
})
