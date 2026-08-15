import { createApp } from 'vue'
import 'element-plus/theme-chalk/dark/css-vars.css'
import App from './App.vue'
import { router } from './router'
import './style.css'
import './theme'

createApp(App).use(router).mount('#app')
