import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppLaunch } from './types'

const mocks = vi.hoisted(() => ({ launch: vi.fn() }))
vi.mock('@drive/api', () => ({ api: { apps: { launch: mocks.launch } } }))

const { prefetchLaunch, takeLaunch, clearPrefetched } = await import('./prefetch')

const data = (session: string): AppLaunch => ({
  app: {
    manifest: { id: 'sample', name: '测试应用', version: '1.0.0', api_version: 2, min_host_version: '0.1.0', description: 'd', author: 'a', entry: 'index.html', permissions: [], settings: [] },
    digest: 'd'.repeat(64), revision: 'rev1', enabled: true, pinned: false, source: 'catalog', installed_at: 1, updated_at: 1, scope: [],
  },
  session, frame_url: '/api/apps/runtime/sample/rev1/index.html', expires_at: 1_000_000_000,
})

describe('应用启动预取', () => {
  beforeEach(() => {
    clearPrefetched()
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: query.includes('(hover: hover)') }))
    vi.useFakeTimers()
  })
  afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); mocks.launch.mockReset() })

  it('桌面端悬停发起 launch，AppHost 一次性取走后不再命中', async () => {
    mocks.launch.mockResolvedValue({ data: data('s1') })
    prefetchLaunch('sample')
    await vi.advanceTimersByTimeAsync(0)
    expect(mocks.launch).toHaveBeenCalledExactlyOnceWith('sample')
    expect(takeLaunch('sample')?.session).toBe('s1')
    expect(takeLaunch('sample')).toBeNull()
    // 取走后再次悬停会重新预取。
    prefetchLaunch('sample')
    await vi.advanceTimersByTimeAsync(0)
    expect(mocks.launch).toHaveBeenCalledTimes(2)
  })

  it('非桌面（无悬停）设备不发起预取', () => {
    vi.stubGlobal('matchMedia', () => ({ matches: false }))
    prefetchLaunch('sample')
    expect(mocks.launch).not.toHaveBeenCalled()
    expect(takeLaunch('sample')).toBeNull()
  })

  it('预取失败静默，超过 60 秒的结果视为过期', async () => {
    mocks.launch.mockRejectedValueOnce(new Error('网络故障')).mockResolvedValue({ data: data('s2') })
    prefetchLaunch('sample')
    await vi.advanceTimersByTimeAsync(0)
    expect(takeLaunch('sample')).toBeNull() // 失败不留缓存，也不抛错
    prefetchLaunch('sample')
    await vi.advanceTimersByTimeAsync(0)
    expect(takeLaunch('sample')?.session).toBe('s2')
    prefetchLaunch('sample')
    await vi.advanceTimersByTimeAsync(0)
    await vi.advanceTimersByTimeAsync(60_001)
    expect(takeLaunch('sample')).toBeNull()
  })

  it('重复悬停去重，未完成前不会重复请求', () => {
    mocks.launch.mockReturnValue(new Promise(() => {}))
    prefetchLaunch('sample')
    prefetchLaunch('sample')
    prefetchLaunch('sample')
    expect(mocks.launch).toHaveBeenCalledTimes(1)
  })
})
