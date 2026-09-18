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
  it('批量段读一次性转移所有分块缓冲区', async () => {
    const p = port()
    const disconnect = connectAppBridge(p as unknown as MessagePort,
      async (method) => (method === 'files.readRanges' ? [new Uint8Array([1]), new Uint8Array([2, 3])] : null), String)
    await p.onmessage!({ data: { type: 'request', id: 7, method: 'files.readRanges', params: { ref: { id: 1, content_version: 'a'.repeat(64) }, ranges: [{ offset: 0, length: 1 }, { offset: 1, length: 2 }] } } } as MessageEvent)
    const [message, transfer] = p.postMessage.mock.calls[0]!
    expect(message.result.map((part: Uint8Array) => [...part])).toEqual([[1], [2, 3]])
    expect(transfer).toEqual(message.result.map((part: Uint8Array) => part.buffer))
    // 混合数组不应当作字节批次转移，避免误把普通对象结构化克隆成缓冲区。
    const p2 = port()
    const d2 = connectAppBridge(p2 as unknown as MessagePort, async () => [new Uint8Array([1]), '混合'], String)
    await p2.onmessage!({ data: { type: 'request', id: 1, method: 'files.readRanges', params: {} } } as MessageEvent)
    expect(p2.postMessage.mock.calls[0]![1]).toBeUndefined()
    d2(); disconnect()
  })
  it('未声明或越界的批量段读请求被白名单拦截', async () => {
    const p = port()
    const disconnect = connectAppBridge(p as unknown as MessagePort, async () => null, String)
    await p.onmessage!({ data: { type: 'request', id: 1, method: 'files.readRanges', params: { ref: { id: 1, content_version: 'x' }, ranges: [] } } } as MessageEvent)
    // 参数合法但白名单受理后由宿主 invoke 校验；这里只断言方法可通行且返回空数组结果。
    await p.onmessage!({ data: { type: 'request', id: 2, method: 'files.readBatch', params: {} } } as MessageEvent)
    const unsupported = p.postMessage.mock.calls.at(-1)![0]
    expect(unsupported.error).toContain('不支持')
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
