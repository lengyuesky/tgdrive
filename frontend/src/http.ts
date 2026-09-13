import axios from 'axios'
import { router } from './router'
import { invalidateSession } from './session'

/** 网盘与页面应用共用认证和未登录跳转。 */
export const http = axios.create({ baseURL: '/api' })

http.interceptors.response.use(
  (response) => response,
  (error) => {
    const path = String(error.config?.url ?? '')
    // 公开分享的提取码校验与登录错误不代表登录会话失效。
    const publicRequest = /(?:^|\/api)\/share\//.test(path) || /\/auth\/login$/.test(path)
    if (error.response?.status === 401 && !publicRequest) {
      invalidateSession()
      if (router.currentRoute.value.path !== '/login') void router.replace('/login')
    }
    return Promise.reject(error)
  },
)
