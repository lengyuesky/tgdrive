/** 能力白名单、限流、取消和有界退出握手；凭据始终由宿主持有。 */
const METHODS = new Set([
  'app.ping', 'files.search', 'media.url', 'favorites.set', 'settings.get', 'settings.patch',
  'ui.pickDirectory', 'ui.openSettings', 'ui.download', 'ui.close', 'ui.setImmersive',
  'files.list', 'files.searchPage', 'files.stat', 'files.readRange', 'assets.read',
  'storage.get', 'storage.set', 'storage.list', 'storage.delete',
])
export interface AppRequest { type: 'request'; id: number; method: string; params: Record<string, unknown> }
export function isAppRequest(value: unknown): value is AppRequest {
  if (!value || typeof value !== 'object') return false
  const request = value as Partial<AppRequest>
  if (request.type !== 'request' || !Number.isSafeInteger(request.id) || Number(request.id) < 1
    || typeof request.method !== 'string' || !METHODS.has(request.method)
    || !request.params || typeof request.params !== 'object' || Array.isArray(request.params)) return false
  try { return new TextEncoder().encode(JSON.stringify(request.params)).length <= 64 * 1024 } catch { return false }
}
export type AppBridge = (() => void) & { beforeClose: () => Promise<void> }
export function connectAppBridge(port: MessagePort, invoke: (method: string, params: Record<string, unknown>, signal: AbortSignal) => Promise<unknown>, message: (error: unknown) => string): AppBridge {
  let disposed = false
  let windowAt = Date.now()
  let count = 0
  let closeSequence = 0
  let finishClose: (() => void) | undefined
  let closing: Promise<void> | undefined
  const pending = new Map<number, AbortController>()
  port.onmessage = async (event: MessageEvent<unknown>) => {
    if (disposed) return
    const raw = event.data as Partial<AppRequest> | null
    if (!raw || typeof raw !== 'object' || !Number.isSafeInteger(raw.id)) return
    if ((raw.type as string) === 'cancel') { pending.get(raw.id!)?.abort(); return }
    if ((raw.type as string) === 'closed') { if (raw.id === closeSequence) finishClose?.(); return }
    const reply = (body: Record<string, unknown>, transfer?: Transferable[]) => {
      if (!disposed) {
        const data = { type: 'response', id: raw.id, ...body }
        if (transfer?.length) port.postMessage(data, transfer)
        else port.postMessage(data)
      }
    }
    if (Date.now() - windowAt > 10_000) { windowAt = Date.now(); count = 0 }
    if (++count > 120 || pending.size >= 16) { reply({ error: '应用请求过于频繁，请稍后重试', code: 'rate_limited' }); return }
    if (!isAppRequest(raw)) { reply({ error: '不支持的应用请求或参数过大' }); return }
    if (pending.has(raw.id)) { reply({ error: '应用请求编号重复' }); return }
    const controller = new AbortController()
    pending.set(raw.id, controller)
    try {
      const result = await invoke(raw.method, raw.params, controller.signal)
      if (!controller.signal.aborted) {
        if (result instanceof Uint8Array) {
          const bytes = result.slice()
          reply({ result: bytes }, [bytes.buffer])
        } else reply({ result })
      }
    } catch (error) {
      if (!controller.signal.aborted) {
        const e = error as { code?: string; response?: { status?: number; data?: { code?: string } } }
        reply({ error: message(error), code: e.response?.data?.code ?? e.code, status: e.response?.status })
      }
    } finally { pending.delete(raw.id) }
  }
  port.start()
  return Object.assign(() => {
    disposed = true
    finishClose?.()
    for (const controller of pending.values()) controller.abort()
    pending.clear()
    port.onmessage = null
    port.close()
  }, {
    beforeClose() {
      if (disposed) return Promise.resolve()
      if (!closing) closing = new Promise<void>((resolve) => {
        const timer = setTimeout(() => finishClose?.(), 2000)
        finishClose = () => { clearTimeout(timer); resolve(); finishClose = undefined }
        port.postMessage({ type: 'closing', id: ++closeSequence })
      })
      return closing
    },
  })
}
