/** 等待宿主完成一帧绘制，让跨进程 iframe 的合成与命中区域先就绪。 */
export function afterHostPaint(signal: AbortSignal): Promise<void> {
  signal.throwIfAborted()
  // 后台标签页会暂停动画帧，但仍应允许 SDK 握手和数据请求。
  if (document.visibilityState !== 'visible') return Promise.resolve()
  return new Promise((resolve, reject) => {
    let frame: number | undefined
    const cleanup = () => {
      if (frame !== undefined) cancelAnimationFrame(frame)
      document.removeEventListener('visibilitychange', visibilityChanged)
      signal.removeEventListener('abort', abort)
    }
    const finish = () => { cleanup(); resolve() }
    const abort = () => { cleanup(); reject(signal.reason) }
    const visibilityChanged = () => { if (document.visibilityState !== 'visible') finish() }
    signal.addEventListener('abort', abort, { once: true })
    document.addEventListener('visibilitychange', visibilityChanged)
    // 第一次回调在绘制前执行；第二次回调才跨过宿主的首次绘制。
    frame = requestAnimationFrame(() => { frame = requestAnimationFrame(finish) })
  })
}
