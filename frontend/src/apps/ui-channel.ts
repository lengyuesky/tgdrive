import { UI_CHANNEL, LIMITS, ERROR_CODES, type ApiError, type UiMessage, type UiRequest, type UiNotice, type UiValue,
  type FileNode, type ScopeInfo, type ScopeAuthorizationRequest, type TaskProgress } from './contract'

export interface UiHandlers {
  immersive(active: boolean, background?: string): Promise<void>
  title(value: string): void
  pick(initial: string | undefined, signal: AbortSignal): Promise<FileNode | null>
  scope(request: ScopeAuthorizationRequest, signal: AbortSignal): Promise<ScopeInfo | null>
  task(progress: TaskProgress): void
  closeSession(): Promise<void>
  leave(): Promise<void>
}
const bytes = (text: string) => new TextEncoder().encode(text).length
const plain = (value: unknown, maximum: number): value is string => typeof value === 'string' && !!value.trim() && bytes(value) <= maximum && !/\p{Cc}/u.test(value)
const opaque = (value: unknown, maximum: number = LIMITS.MAX_OPAQUE_ID_BYTES): value is string => typeof value === 'string' && new RegExp(`^[A-Za-z0-9_-]{1,${maximum}}$`).test(value)
const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value)
function failure(code: keyof typeof ERROR_CODES, message: string, id: string): ApiError { return { code, message, request_id: id } }

export function createUiChannel(frame: () => Window | null, nonce: string, handlers: UiHandlers) {
  let disposed = false, dialogOpen = false
  const pending = new Map<string, AbortController>()
  let closing: Promise<void> | undefined, closeId: string | undefined, saved: (() => void) | undefined
  function post(message: UiMessage) { if (!disposed) frame()?.postMessage(message, '*') }
  function notice(value: UiNotice) { post({ type: 'notice', channel: UI_CHANNEL, nonce, notice: value }) }
  async function beforeClose() {
    if (closing) return closing
    closing = (async () => {
      closeId = crypto.randomUUID().replaceAll('-', '')
      await new Promise<void>((resolve) => {
        const timeout = setTimeout(resolve, LIMITS.UI_CLOSE_GRACE_MILLIS)
        saved = () => { clearTimeout(timeout); resolve() }
        notice({ event: 'ui.before_close', data: { close_id: closeId!, deadline_ms: LIMITS.UI_CLOSE_GRACE_MILLIS } })
      })
      saved = undefined
      await handlers.closeSession()
    })()
    return closing
  }
  async function execute(raw: Record<string, unknown>, signal: AbortSignal): Promise<UiValue> {
    if (['ui.set_immersive', 'ui.set_title', 'ui.pick_directory', 'ui.request_scope', 'ui.task', 'ui.closed'].includes(String(raw.method)) && !object(raw.params)) throw failure('invalid_request', '界面方法参数必须是对象', '')
    const request = raw as unknown as UiRequest
    const params = 'params' in raw && object(raw.params) ? raw.params : {}
    switch (request.method) {
      case 'ui.set_immersive':
        if (typeof params.active !== 'boolean' || (params.background != null && (typeof params.background !== 'string' || !new RegExp(`^#[0-9a-fA-F]{${LIMITS.IMMERSIVE_COLOR_HEX_DIGITS}}$`).test(params.background)))) throw failure('invalid_request', '沉浸背景只能使用六位十六进制颜色', '')
        await handlers.immersive(params.active, params.background as string | undefined); break
      case 'ui.set_title':
        if (!plain(params.title, LIMITS.MAX_UI_TITLE_BYTES)) throw failure('invalid_request', '标题必须是有界纯文本', '')
        handlers.title(params.title); break
      case 'ui.pick_directory':
        if (params.initial_id != null && (typeof params.initial_id !== 'string' || !new RegExp(`^[1-9][0-9]{0,${LIMITS.MAX_NODE_ID_DIGITS - 1}}$`).test(params.initial_id) || BigInt(params.initial_id) > BigInt(LIMITS.MAX_NODE_ID))) throw failure('invalid_request', '初始目录身份无效', '')
        if (dialogOpen) throw failure('rate_limited', '已有一个目录选择或授权窗口', '')
        dialogOpen = true
        try { return { type: 'directory', directory: await handlers.pick(params.initial_id as string | undefined, signal) } } finally { dialogOpen = false }
      case 'ui.request_scope':
        if (!plain(params.reason, LIMITS.MAX_AUTHORIZATION_REASON_BYTES) || !['read_only', 'read_write'].includes(String(params.preferred_access))) throw failure('invalid_request', '授权请求文本或权限级别无效', '')
        if (dialogOpen) throw failure('rate_limited', '已有一个目录选择或授权窗口', '')
        dialogOpen = true
        try { return { type: 'authorization', scope: await handlers.scope(params as unknown as ScopeAuthorizationRequest, signal) } } finally { dialogOpen = false }
      case 'ui.task': {
        const integer = (value: unknown) => typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= LIMITS.MAX_TASK_PROGRESS
        if (!opaque(params.id, LIMITS.MAX_TASK_ID_BYTES) || !plain(params.title, LIMITS.MAX_TASK_TITLE_BYTES) || !integer(params.completed) ||
          (params.total != null && (!integer(params.total) || Number(params.total) < Number(params.completed))) ||
          !['running', 'completed', 'cancelled', 'failed'].includes(String(params.state)) || (params.cancellable != null && typeof params.cancellable !== 'boolean')) throw failure('invalid_request', '任务进度格式无效', '')
        handlers.task({ ...params, cancellable: params.cancellable ?? false } as unknown as TaskProgress); break
      }
      case 'ui.close': await beforeClose(); break
      case 'ui.closed':
        if (!opaque(params.close_id) || params.close_id !== closeId) throw failure('invalid_request', '关闭确认不属于当前关闭轮次', '')
        saved?.(); break
      default: throw failure('capability_unavailable', '界面通道不提供此方法', '')
    }
    return { type: 'acknowledged' }
  }
  async function receive(event: MessageEvent) {
    const data: unknown = event.data
    // 不透明源不等于身份；必须同时核对当前 WindowProxy 与本次随机 nonce。
    if (disposed || event.source !== frame() || event.origin !== 'null' || !object(data) || data.type !== 'request' || data.channel !== UI_CHANNEL || data.nonce !== nonce || !opaque(data.id) || !object(data.request)) return
    const id = data.id
    const reply = (value: UiValue | ApiError, error = false) => post({ type: 'response', channel: UI_CHANNEL, nonce, id,
      result: error ? { status: 'error', error: { ...(value as ApiError), request_id: id } } : { status: 'ok', value: value as UiValue } })
    if (pending.has(id)) { reply(failure('invalid_request', '请求标识正在等待应答', id), true); return }
    if (pending.size >= LIMITS.MAX_UI_REQUESTS) { reply(failure('quota_exceeded', '待应答界面请求已达上限', id), true); return }
    const controller = new AbortController(); pending.set(id, controller)
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      const timeout = new Promise<never>((_, reject) => { timer = setTimeout(() => { controller.abort(); reject(failure('request_timeout', '界面请求已超时', id)) }, LIMITS.UI_TIMEOUT_SECONDS * 1000) })
      const value = await Promise.race([execute(data.request, controller.signal), timeout]); reply(value)
      if (data.request.method === 'ui.close') await handlers.leave()
    } catch (error) { reply(object(error) && typeof error.code === 'string' ? error as unknown as ApiError : failure('internal_error', '界面操作失败', id), true) }
    finally { if (timer) clearTimeout(timer); pending.delete(id) }
  }
  return { receive, notice, beforeClose,
    dispose() { disposed = true; saved?.(); for (const controller of pending.values()) controller.abort(); pending.clear() },
  }
}
