import { createRouter, createWebHistory } from 'vue-router'
import Login from './views/Login.vue'
import Files from './views/Files.vue'
import Admin from './views/Admin.vue'
import Share from './views/Share.vue'

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
