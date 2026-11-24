import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { APP_CENTER_BASE } from './contract'

vi.mock('../session', () => ({ invalidateSession: vi.fn() }))
const fetchMock = vi.fn()
function response(value: unknown) { return { ok: true, status: 200, json: async () => value } as Response }

beforeEach(() => { vi.resetModules(); fetchMock.mockReset(); vi.stubGlobal('fetch', fetchMock) })
afterEach(() => { vi.unstubAllGlobals() })

describe('仅主人使用的后端管理客户端', () => {
  it('合并安装列表请求，同时读取根地址选择', async () => {
    fetchMock.mockResolvedValue(response({ apps: [], root_app: { app_id: 'example-backend', revision: 'root-1' } }))
    const client = await import('./client')
    const [first, second] = await Promise.all([client.loadInstalled(true), client.loadInstalled(true)])
    expect(first).toEqual(second)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(client.rootSelection.value).toEqual({ app_id: 'example-backend', revision: 'root-1' })
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: 'same-origin', redirect: 'error', method: 'GET' })
  })

  it('根地址清除使用修订 CAS，日志与重启沿用主人身份', async () => {
    fetchMock.mockResolvedValue(response({ app_id: null, revision: 'root-2' }))
    const { center } = await import('./client')
    await center.setRoot({ expected_revision: 'root-1', app_id: null })
    expect(fetchMock.mock.calls[0][0]).toBe(`${APP_CENTER_BASE}/root-app`)
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ method: 'PUT', credentials: 'same-origin', redirect: 'error', body: JSON.stringify({ expected_revision: 'root-1', app_id: null }) })
    await center.logs('example-backend')
    await center.restart('example-backend')
    expect(fetchMock.mock.calls.slice(1).map(([url, options]) => [url, options.method])).toEqual([
      [`${APP_CENTER_BASE}/apps/example-backend/backend/logs`, 'GET'],
      [`${APP_CENTER_BASE}/apps/example-backend/backend/restart`, 'POST'],
    ])
    for (const [, options] of fetchMock.mock.calls) {
      expect(options).toMatchObject({ mode: 'same-origin', referrerPolicy: 'same-origin', credentials: 'same-origin', redirect: 'error' })
      expect(options.headers.has('Authorization')).toBe(false)
    }
  })

  it('不提供单应用后端资源管理方法或内部接口', async () => {
    const { center, endpoint } = await import('./client')
    expect(center).not.toHaveProperty('backend')
    expect(() => endpoint('center.app.backend', { app_id: 'example-backend' })).toThrow('宿主契约未提供所需接口')
    expect(fetchMock).not.toHaveBeenCalled()
  })
})
