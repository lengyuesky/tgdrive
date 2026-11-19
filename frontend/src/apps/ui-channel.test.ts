import { afterEach, describe, expect, it, vi } from 'vitest'
import { createUiChannel, type UiHandlers } from './ui-channel'
import { UI_CHANNEL, LIMITS, type UiMessage } from './contract'
function fixture() {
  const post = vi.fn(), frame = { postMessage: post } as unknown as Window
  const handlers: UiHandlers = { immersive: vi.fn(async () => undefined), title: vi.fn(), pick: vi.fn(async () => null), scope: vi.fn(async () => null), task: vi.fn(), closeSession: vi.fn(async () => undefined), leave: vi.fn(async () => undefined) }
  const channel = createUiChannel(() => frame, 'nonce', handlers)
  const send = (method: string, params?: unknown, extra: Record<string, unknown> = {}) => channel.receive(new MessageEvent('message', { source: frame, origin: 'null', data: { type: 'request', channel: UI_CHANNEL, nonce: 'nonce', id: 'request', request: { method, ...(params === undefined ? {} : { params }) }, ...extra } }))
  return { frame, post, handlers, channel, send }
}
afterEach(() => vi.useRealTimers())
describe('隔离页面界面通道', () => {
  it('同时绑定当前窗口、不透明源与 nonce，不能伪装其他窗口', async () => {
    const state = fixture()
    await state.send('ui.set_title', { title: '新标题' }, { nonce: 'old' })
    await state.channel.receive(new MessageEvent('message', { source: window, origin: 'null', data: { type: 'request', channel: UI_CHANNEL, nonce: 'nonce', id: 'request', request: { method: 'ui.set_title', params: { title: '错误窗口' } } } }))
    expect(state.post).not.toHaveBeenCalled(); expect(state.handlers.title).not.toHaveBeenCalled()
    await state.send('ui.set_title', { title: '新标题' }); expect(state.handlers.title).toHaveBeenCalledWith('新标题')
    expect(state.post).toHaveBeenLastCalledWith(expect.objectContaining({ type: 'response', result: { status: 'ok', value: { type: 'acknowledged' } } }), '*')
  })
  it('通道没有文件 RPC 或任意 fetch 方法，背景也不能注入 CSS', async () => {
    const state = fixture()
    await state.send('files.bytes', { url: 'https://outside.invalid', body: '不能转发' })
    expect((state.post.mock.calls[0][0] as UiMessage)).toMatchObject({ result: { status: 'error', error: { code: 'capability_unavailable' } } })
    await state.send('ui.set_immersive', { active: true, background: 'url(https://outside.invalid)' }); expect(state.handlers.immersive).not.toHaveBeenCalled()
    await state.send('ui.set_title', { title: '汉'.repeat(41) }); expect(state.handlers.title).not.toHaveBeenCalled()
    await state.send('ui.pick_directory', []); expect(state.handlers.pick).not.toHaveBeenCalled()
  })
  it('目录选择可取消，不把选择当成授权；同一时间只开一个选择窗口', async () => {
    const state = fixture(); let selected: ((value: null) => void) | undefined
    state.handlers.pick = vi.fn(() => new Promise<null>((resolve) => { selected = resolve }))
    const picking = state.send('ui.pick_directory', {}, { id: 'pick' })
    await state.send('ui.request_scope', { reason: '请求读取', preferred_access: 'read_only' }, { id: 'scope' })
    expect(state.handlers.scope).not.toHaveBeenCalled(); expect(state.post).toHaveBeenCalledWith(expect.objectContaining({ result: expect.objectContaining({ status: 'error' }) }), '*')
    selected?.(null); await picking
    expect(state.post).toHaveBeenLastCalledWith(expect.objectContaining({ result: { status: 'ok', value: { type: 'directory', directory: null } } }), '*')
  })
  it('关闭先通知保存，收到当前 close_id 后撤销 HTTP 会话，迟到消息不改变外壳', async () => {
    vi.useFakeTimers(); const state = fixture(); const closing = state.channel.beforeClose()
    const notice = state.post.mock.calls[0][0] as Extract<UiMessage, { type: 'notice' }>
    expect(notice.notice).toMatchObject({ event: 'ui.before_close', data: { deadline_ms: LIMITS.UI_CLOSE_GRACE_MILLIS } })
    expect(state.handlers.closeSession).not.toHaveBeenCalled()
    const id = (notice.notice as { data: { close_id: string } }).data.close_id
    await state.send('ui.closed', { close_id: id }); await closing
    expect(state.handlers.closeSession).toHaveBeenCalledTimes(1)
    state.channel.dispose(); await state.send('ui.set_immersive', { active: true }); expect(state.handlers.immersive).not.toHaveBeenCalled()
  })
  it('保存超出宽限后仍关闭，任务完成数不能超过总数', async () => {
    vi.useFakeTimers(); const state = fixture(); const closing = state.channel.beforeClose(); await vi.advanceTimersByTimeAsync(LIMITS.UI_CLOSE_GRACE_MILLIS); await closing
    expect(state.handlers.closeSession).toHaveBeenCalledTimes(1)
    await state.send('ui.task', { id: 'task', title: '任务', completed: 2, total: 1, state: 'running', cancellable: true }); expect(state.handlers.task).not.toHaveBeenCalled()
  })
})
