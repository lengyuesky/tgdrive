import { flushPromises, shallowMount } from '@vue/test-utils'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppLaunch, AppManifest, InstalledApp } from '../types'
import AppHost from './AppHost.vue'

const mocks = vi.hoisted(() => ({
  launch: vi.fn(), rpc: vi.fn(), get: vi.fn(), connect: vi.fn(), push: vi.fn(),
  onLeave: vi.fn(), onUpdate: vi.fn(), onHostEvent: vi.fn(),
  route: {} as { params: { id: string }, query: Record<string, string> },
}))

vi.mock('vue-router', () => ({
  useRoute: () => mocks.route,
  useRouter: () => ({ push: mocks.push }),
  onBeforeRouteLeave: mocks.onLeave,
  onBeforeRouteUpdate: mocks.onUpdate,
}))
vi.mock('@drive/api', () => ({
  api: { apps: { launch: mocks.launch, rpc: mocks.rpc, get: mocks.get } },
  errMsg: (error: unknown) => (error as Error).message,
}))
vi.mock('../bridge', () => ({ connectAppBridge: mocks.connect }))
vi.mock('../paint', () => ({ afterHostPaint: async () => {} }))
vi.mock('../events', () => ({ onHostEvent: mocks.onHostEvent }))

const manifest: AppManifest = {
  id: 'sample', name: '测试应用', version: '1.0.0', api_version: 2, min_host_version: '0.1.0',
  description: '合成清单', author: '测试作者', entry: 'index.html', permissions: ['files.read'], settings: [],
}

function launchData(overrides: { revision?: string; session?: string; expires_in?: number } = {}): AppLaunch {
  const revision = overrides.revision ?? 'rev1'
  const app: InstalledApp = {
    manifest, digest: 'a'.repeat(64), revision, enabled: true, pinned: false,
    source: 'catalog', installed_at: 1, updated_at: 1, scope: [],
  }
  return {
    app, session: overrides.session ?? 's1',
    frame_url: `/api/apps/runtime/sample/${revision}/index.html`,
    expires_at: Math.floor(Date.now() / 1000) + (overrides.expires_in ?? 6 * 3600),
  }
}

const sessionExpired = { response: { status: 403, data: { code: 'session_expired', error: '应用会话已过期，请重新打开' } } }

let wrapper: ReturnType<typeof shallowMount> | undefined
let capturedInvoke: ((method: string, params: Record<string, unknown>, signal: AbortSignal) => Promise<unknown>) | undefined

beforeEach(() => {
  vi.useFakeTimers()
  mocks.route = { params: { id: 'sample' }, query: {} }
  mocks.launch.mockReset().mockResolvedValue({ data: launchData() })
  mocks.rpc.mockReset().mockResolvedValue({ data: { ok: true } })
  mocks.get.mockReset().mockResolvedValue({ data: launchData().app })
  mocks.connect.mockReset().mockImplementation((port: unknown, invoke: typeof capturedInvoke) => {
    capturedInvoke = invoke
    return Object.assign(() => {}, { beforeClose: async () => {} })
  })
  mocks.push.mockReset()
  mocks.onHostEvent.mockReset().mockReturnValue(() => {})
})
afterEach(() => { wrapper?.unmount(); wrapper = undefined; vi.useRealTimers() })

async function render() {
  wrapper = shallowMount(AppHost, { global: {
    plugins: [ElementPlus],
    stubs: {
      AppSettingsDialog: true,
      AppIcon: true,
      DirPicker: {
        name: 'DirPicker',
        setup: (_, { expose }) => { expose({ close: () => {}, open: async () => null }) },
        template: '<div />',
      },
    },
  } })
  await flushPromises()
  return wrapper
}

/** 模拟沙箱 iframe 的 ready 握手，让宿主进入已连接状态。 */
async function connectSandbox() {
  const frame = wrapper!.find('iframe').element as HTMLIFrameElement
  window.dispatchEvent(new MessageEvent('message', {
    origin: 'null', source: frame.contentWindow,
    data: { channel: 'tgdrive-app-v1', type: 'ready' },
  }))
  await flushPromises()
  expect(mocks.connect).toHaveBeenCalledOnce()
  expect(wrapper!.find('.host-status').exists()).toBe(false)
}

describe('应用宿主会话续签', () => {
  it('RPC 遇 session_expired 静默续签并重放一次，iframe 不重建', async () => {
    const view = await render()
    const firstSrc = view.find('iframe').attributes('src')
    await connectSandbox()
    expect(firstSrc).toContain('/rev1/index.html')

    mocks.rpc.mockRejectedValueOnce(sessionExpired).mockResolvedValueOnce({ data: { results: [] } })
    mocks.launch.mockClear().mockResolvedValueOnce({ data: launchData({ session: 's2' }) })
    const result = await capturedInvoke!('files.search', {}, new AbortController().signal)
    expect(result).toEqual({ results: [] })
    // 第一次失败用旧会话，重放使用续签后的新会话。
    expect(mocks.rpc.mock.calls.map((call) => call[1])).toEqual(['s1', 's2'])
    // 修订未变，续签不重建 iframe。
    expect(view.find('iframe').attributes('src')).toBe(firstSrc)
  })

  it('续签发现修订轮换时按 app_changed 语义失败，不无限重试', async () => {
    await render()
    await connectSandbox()
    mocks.rpc.mockRejectedValueOnce(sessionExpired)
    mocks.launch.mockClear().mockResolvedValueOnce({ data: launchData({ revision: 'rev2', session: 's2' }) })
    await expect(capturedInvoke!('files.search', {}, new AbortController().signal))
      .rejects.toThrow('应用已停用、卸载或更新，请重新打开')
    // 只重放了一次：第二次失败不再续签。
    expect(mocks.rpc).toHaveBeenCalledTimes(1)
  })

  it('会话临期时轮询主动续签，后续 ping 使用新会话', async () => {
    mocks.launch.mockReset()
      .mockResolvedValueOnce({ data: launchData({ session: 's1', expires_in: 800 }) })
      .mockResolvedValueOnce({ data: launchData({ session: 's2', expires_in: 6 * 3600 }) })
    const view = await render()
    await connectSandbox()
    const frameSrc = view.find('iframe').attributes('src')
    await vi.advanceTimersByTimeAsync(15_000)
    expect(mocks.launch).toHaveBeenCalledTimes(2)
    // 修订未变：iframe 原地保留。
    expect(view.find('iframe').attributes('src')).toBe(frameSrc)
    expect(mocks.rpc).toHaveBeenLastCalledWith('sample', 's2', 'app.ping', {}, expect.anything())
    // 无错误页：续签失败路径未触发。
    expect(view.text()).not.toContain('应用会话已过期')
  })

  it('休眠唤醒后 ping 报 session_expired：先续签恢复，后续轮询使用新会话', async () => {
    mocks.launch.mockReset()
      .mockResolvedValueOnce({ data: launchData({ session: 's1' }) })
      .mockResolvedValueOnce({ data: launchData({ session: 's2' }) })
    await render()
    await connectSandbox()
    mocks.rpc.mockReset().mockRejectedValueOnce(sessionExpired)
    await vi.advanceTimersByTimeAsync(15_000)
    expect(mocks.launch).toHaveBeenCalledTimes(2)
    expect(mocks.rpc).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(15_000)
    expect(mocks.rpc).toHaveBeenCalledTimes(2)
    expect(mocks.rpc.mock.calls[1]![1]).toBe('s2')
    expect(wrapper!.findComponent({ name: 'ElEmpty' }).exists()).toBe(false)
  })

  it('404 卸载与修订轮换的 ping 失败仍走原有错误提示', async () => {
    const view = await render()
    await connectSandbox()
    mocks.rpc.mockReset().mockRejectedValue({ response: { status: 404, data: { code: null, error: '未找到' } } })
    await vi.advanceTimersByTimeAsync(15_000)
    expect(view.findComponent({ name: 'ElEmpty' }).props('description'))
      .toBe('应用已卸载，请前往应用中心重新安装')
  })
})
