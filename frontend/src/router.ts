import { createRouter, createWebHistory } from 'vue-router'

// 页面按路由拆包；公开分享页、后台与主文件页不再挤进同一个首屏脚本。
const Login = () => import('./views/Login.vue')
const Files = () => import('./views/Files.vue')
const Admin = () => import('./views/Admin.vue')
const Share = () => import('./views/Share.vue')

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/login', component: Login },
    // /files 之后的部分即当前目录路径
    { path: '/files/:path(.*)*', component: Files },
    { path: '/admin', component: Admin },
    // 公开分享页,免登录
    { path: '/s/:token', component: Share },
    { path: '/', redirect: '/files/' },
    { path: '/:catchall(.*)*', redirect: '/files/' },
  ],
})
