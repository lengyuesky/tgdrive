import { createRouter, createWebHistory } from 'vue-router'
import { closeAllOverlays, closeTopOverlay } from './mobile/overlays'
const SessionBoundary = () => import('./components/SessionBoundary.vue')
const Transfers = () => import('./views/Transfers.vue')
const Me = () => import('./views/Me.vue')
const AppCenter = () => import('./apps/AppCenter.vue')
const AppContainer = () => import('./apps/AppContainer.vue')

// 页面按路由拆包；公开分享页、后台与主文件页不再挤进同一个首屏脚本。
const Login = () => import('./views/Login.vue')
const Files = () => import('./views/Files.vue')
const Admin = () => import('./views/Admin.vue')
const Share = () => import('./views/Share.vue')

export const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes: [
    { path: '/login', component: Login },
    { path: '/', component: SessionBoundary, children: [
      { path: '', redirect: '/files/' },
      // /files 之后的部分即当前目录路径，登录外壳跨页面保留上传任务。
      { path: 'files/:path(.*)*', component: Files },
      { path: 'settings', component: Admin },
      { path: 'transfers', component: Transfers },
      { path: 'me', component: Me },
      { path: 'apps', component: AppCenter },
      { path: 'apps/:appId', component: AppContainer },
    ] },
    // 公开分享页,免登录
    { path: '/s/:token', component: Share },
    { path: '/:catchall(.*)*', redirect: '/files/' },
  ],
})

router.beforeEach((to) => {
  // 认证失效必须能离开，不能被上传冲突弹层拦住。
  if (to.path === '/login') { closeAllOverlays(); return true }
  if (closeTopOverlay()) return false
  return true
})
