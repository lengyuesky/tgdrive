import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { afterHostPaint } from './paint'

let frames: Map<number, FrameRequestCallback>
let visibility: ReturnType<typeof vi.spyOn>
function paintFrame() {
  const current = [...frames.values()]
  frames.clear()
  for (const callback of current) callback(0)
}
beforeEach(() => {
  frames = new Map()
  let sequence = 0
  visibility = vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('visible')
  vi.stubGlobal('requestAnimationFrame', vi.fn((callback: FrameRequestCallback) => {
    frames.set(++sequence, callback)
    return sequence
  }))
  vi.stubGlobal('cancelAnimationFrame', vi.fn((id: number) => frames.delete(id)))
})
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks() })

describe('沙箱宿主的首次绘制', () => {
  it('跨过首次绘制后才继续，不把 DOM 更新或第一帧回调当作已绘制', async () => {
    const ready = vi.fn()
    const waiting = afterHostPaint(new AbortController().signal).then(ready)
    await Promise.resolve()
    expect(ready).not.toHaveBeenCalled()
    paintFrame()
    await Promise.resolve()
    expect(ready).not.toHaveBeenCalled()
    expect(frames.size).toBe(1)
    paintFrame()
    await waiting
    expect(ready).toHaveBeenCalledOnce()
    expect(frames.size).toBe(0)
  })

  it('后台标签页不等待已暂停的动画帧', async () => {
    visibility.mockReturnValue('hidden')
    await afterHostPaint(new AbortController().signal)
    expect(requestAnimationFrame).not.toHaveBeenCalled()
  })

  it('等待时转入后台会释放动画帧并完成握手', async () => {
    const waiting = afterHostPaint(new AbortController().signal)
    paintFrame()
    visibility.mockReturnValue('hidden')
    document.dispatchEvent(new Event('visibilitychange'))
    await waiting
    expect(frames.size).toBe(0)
    expect(cancelAnimationFrame).toHaveBeenCalledOnce()
  })

  it('已中止的页面不会再安排绘制回调', () => {
    const controller = new AbortController()
    controller.abort()
    expect(() => afterHostPaint(controller.signal)).toThrow()
    expect(frames.size).toBe(0)
  })

  it('关闭或切换应用会中止等待并清除后续回调', async () => {
    const controller = new AbortController()
    const reason = new Error('应用已经关闭')
    const waiting = afterHostPaint(controller.signal)
    paintFrame()
    controller.abort(reason)
    await expect(waiting).rejects.toBe(reason)
    expect(frames.size).toBe(0)
    expect(cancelAnimationFrame).toHaveBeenCalledOnce()
  })
})
