import { AxiosError, type AxiosAdapter, type AxiosResponse, type InternalAxiosRequestConfig } from 'axios'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { http } from './http'
import { api } from './api'
import { onSessionInvalidated } from './session'

const router = vi.hoisted(() => ({ currentRoute: { value: { path: '/files/' } }, replace: vi.fn() }))
vi.mock('./router', () => ({ router }))

const originalAdapter = http.defaults.adapter
const invalidated = vi.fn()
let unsubscribe: () => void
function adapter(status: number): AxiosAdapter {
  return async (config: InternalAxiosRequestConfig) => {
    const response: AxiosResponse = { config, data: { ok: status === 200 }, status, statusText: String(status), headers: {} }
    if (status >= 400) throw new AxiosError('测试响应', 'ERR_BAD_REQUEST', config, undefined, response)
    return response
  }
}
beforeEach(() => {
  router.currentRoute.value.path = '/files/'
  router.replace.mockReset()
  invalidated.mockReset()
  unsubscribe = onSessionInvalidated(invalidated)
})
afterEach(() => { unsubscribe(); http.defaults.adapter = originalAdapter })

describe('认证与会话清理', () => {
  it('受保护接口的 401 先同步停止本地任务，再替换到登录页', async () => {
    http.defaults.adapter = adapter(401)
    router.replace.mockImplementation(() => { expect(invalidated).toHaveBeenCalledOnce() })
    await expect(http.get('/uploads')).rejects.toBeInstanceOf(AxiosError)
    expect(router.replace).toHaveBeenCalledWith('/login')
  })

  it.each(['/share/公开链接/authorize', '/api/share/公开链接/list', '/auth/login'])(
    '%s 的提取码或登录错误不能清除已登录会话', async (url) => {
      http.defaults.adapter = adapter(401)
      await expect(http.post(url)).rejects.toBeInstanceOf(AxiosError)
      expect(invalidated).not.toHaveBeenCalled()
      expect(router.replace).not.toHaveBeenCalled()
    },
  )

  it('网络错误不会冒充认证失效', async () => {
    http.defaults.adapter = async () => { throw new AxiosError('网络已断开', 'ERR_NETWORK') }
    await expect(http.get('/uploads')).rejects.toBeInstanceOf(AxiosError)
    expect(invalidated).not.toHaveBeenCalled()
    expect(router.replace).not.toHaveBeenCalled()
  })

  it.each(['logout', 'logoutAll'] as const)('%s 成功即清理本地状态，不等待路由切换', async (action) => {
    http.defaults.adapter = adapter(200)
    await (action === 'logout' ? api.logout() : api.admin.logoutAll())
    expect(invalidated).toHaveBeenCalledOnce()
    expect(router.replace).not.toHaveBeenCalled()
  })

  it('注销失败保留当前任务，交由调用页面显示失败', async () => {
    http.defaults.adapter = adapter(500)
    await expect(api.logout()).rejects.toBeInstanceOf(AxiosError)
    expect(invalidated).not.toHaveBeenCalled()
  })
})
