import { describe, expect, it, vi } from 'vitest'
import { connectAppBridge } from './bridge'
function port() { return { onmessage: null as ((event: MessageEvent) => Promise<void>) | null, postMessage: vi.fn(), start: vi.fn(), close: vi.fn() } }
describe('应用 v2 消息通道', () => {
  it('转移字节缓冲区而非把文件编码为 JSON', async () => {
    const p = port()
    const disconnect = connectAppBridge(p as unknown as MessagePort, async () => new Uint8Array([1, 2, 3]), String)
    await p.onmessage!({ data: { type: 'request', id: 1, method: 'assets.read', params: { path: 'a.bin' } } } as MessageEvent)
    const [message, transfer] = p.postMessage.mock.calls[0]!
    expect([...message.result]).toEqual([1, 2, 3]); expect(transfer).toEqual([message.result.buffer])
    disconnect()
  })
  it('单请求取消会中止宿主请求且不回传迟到结果', async () => {
    const p = port()
    let finish!: () => void
    let signal!: AbortSignal
    const disconnect = connectAppBridge(p as unknown as MessagePort, async (_m, _p, s) => { signal = s; await new Promise<void>((r) => { finish = r }); return '旧内容' }, String)
    const running = p.onmessage!({ data: { type: 'request', id: 1, method: 'files.stat', params: { id: 2 } } } as MessageEvent)
    await p.onmessage!({ data: { type: 'cancel', id: 1 } } as MessageEvent)
    expect(signal.aborted).toBe(true); finish(); await running
    expect(p.postMessage).not.toHaveBeenCalled(); disconnect()
  })
  it('离开通知等待确认但不超过两秒', async () => {
    vi.useFakeTimers()
    try {
      const p = port()
      const disconnect = connectAppBridge(p as unknown as MessagePort, async () => null, String)
      const leaving = disconnect.beforeClose()
      expect(p.postMessage).toHaveBeenCalledWith({ type: 'closing', id: 1 })
      await p.onmessage!({ data: { type: 'closed', id: 1 } } as MessageEvent)
      await leaving; disconnect()
      const p2 = port()
      const d2 = connectAppBridge(p2 as unknown as MessagePort, async () => null, String)
      const timeout = d2.beforeClose()
      await vi.advanceTimersByTimeAsync(2000); await timeout; d2()
    } finally { vi.useRealTimers() }
  })
})
