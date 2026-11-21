import { defineComponent } from 'vue'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useRestore, type RestoreController } from './useRestore'
import type { RestoreStatus } from '../../api'

const mocks = vi.hoisted(() => ({ status: vi.fn(), prepare: vi.fn(), cancel: vi.fn(), confirm: vi.fn(), resume: vi.fn(), release: vi.fn(), hold: vi.fn() }))
vi.mock('../../api', () => ({ api: { admin: { restore: mocks } }, errMsg: String }))
vi.mock('../../http', () => ({ holdAuthRedirect: mocks.hold }))
let view: ReturnType<typeof mount>
let r: RestoreController
let canStart = true
const state = (phase: RestoreStatus['phase'], extra = {}): RestoreStatus => ({ phase, warnings: [], safe_mode: false, ...extra })
async function render(initial = state('idle')) {
  mocks.status.mockResolvedValue({ data: initial })
  view = mount(defineComponent({ setup() { r = useRestore(() => canStart); return () => null } }))
  await flushPromises()
}
beforeEach(() => { vi.resetAllMocks(); vi.useFakeTimers(); canStart = true; mocks.hold.mockReturnValue(mocks.release) })
afterEach(() => { view?.unmount(); vi.useRealTimers() })

describe('频道恢复任务生命周期', () => {
  it('读取持久结果与安全模式，不自动恢复数据或解除安全模式', async () => {
    await render(state('succeeded', { safe_mode: true, rollback_path: '/private/rollback.db' }))
    expect(r.status.value.safe_mode).toBe(true)
    expect(mocks.prepare).not.toHaveBeenCalled()
    expect(mocks.confirm).not.toHaveBeenCalled()
    expect(mocks.resume).not.toHaveBeenCalled()
  })
  it('有设置改动时阻止打开或准备，不触发自动保存', async () => {
    await render(); canStart = false
    await r.open()
    await r.prepare({ backup_id: 'backup', acknowledge_forward: true })
    expect(r.opened.value).toBe(false)
    expect(r.error.value).toContain('先保存或放弃')
    expect(mocks.prepare).not.toHaveBeenCalled()
  })
  it('明确同意后准备，轮询 ready，取消后停止轮询，卸载清理', async () => {
    await render()
    await r.prepare({ backup_id: 'backup', acknowledge_forward: false })
    expect(mocks.prepare).not.toHaveBeenCalled()
    mocks.prepare.mockResolvedValue({ data: state('preparing', { id: 'job' }) })
    await r.prepare({ backup_id: 'backup', acknowledge_forward: true })
    expect(r.locked.value).toBe(true)
    mocks.status.mockResolvedValue({ data: state('ready', { id: 'job' }) })
    await vi.advanceTimersByTimeAsync(2000)
    expect(r.status.value.phase).toBe('ready')
    mocks.cancel.mockResolvedValue({ data: state('cancelled', { id: 'job' }) })
    await r.cancel()
    expect(mocks.cancel).toHaveBeenCalledWith('job')
    expect(r.locked.value).toBe(false)
    const calls = mocks.status.mock.calls.length
    await vi.advanceTimersByTimeAsync(10000)
    expect(mocks.status).toHaveBeenCalledTimes(calls)
    view.unmount()
    expect(vi.getTimerCount()).toBe(0)
  })
  it('确认重启后保持等待，401 不自动再次提交，卸载释放跳转保护', async () => {
    await render(state('ready', { id: 'job' }))
    mocks.confirm.mockResolvedValue({ data: state('restarting', { id: 'job' }) })
    await r.confirm({ id: 'job', password: 'current', confirmation: '恢复' })
    expect(r.waiting.value).toBe(true)
    mocks.status.mockRejectedValue({ response: { status: 401 } })
    await vi.advanceTimersByTimeAsync(2000)
    expect(r.error.value).toContain('重新登录')
    await vi.advanceTimersByTimeAsync(10000)
    expect(mocks.confirm).toHaveBeenCalledTimes(1)
    view.unmount()
    expect(mocks.release).toHaveBeenCalledTimes(2)
    expect(vi.getTimerCount()).toBe(0)
  })
  it('再次认证失败保留 ready；网络中断视为结果未知，不重复恢复', async () => {
    await render(state('ready', { id: 'job' }))
    const body = { id: 'job', password: 'wrong', confirmation: '恢复' as const }
    mocks.confirm.mockRejectedValueOnce({ response: { status: 401 } })
    await r.confirm(body)
    expect(r.waiting.value).toBe(false)
    expect(r.status.value.phase).toBe('ready')
    mocks.confirm.mockRejectedValueOnce(new Error('network'))
    await r.confirm(body)
    expect(r.waiting.value).toBe(true)
    await r.confirm(body)
    expect(mocks.confirm).toHaveBeenCalledTimes(2)
  })
  it('卸载后迟到状态响应不创建轮询或跳转保护', async () => {
    await render()
    let resolve!: (value: { data: RestoreStatus }) => void
    mocks.status.mockReturnValue(new Promise(done => { resolve = done }))
    const pending = r.refresh()
    view.unmount()
    resolve({ data: state('restarting') })
    await pending
    expect(mocks.hold).not.toHaveBeenCalled()
    expect(vi.getTimerCount()).toBe(0)
  })
})
