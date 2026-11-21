import { createApp } from 'vue'
import 'element-plus/theme-chalk/dark/css-vars.css'
import 'element-plus/es/components/message/style/css'
import App from './App.vue'
import { router } from './router'
import './style.css'
import './theme'
import './mobile/style.css'
import { installViewport } from './mobile/viewport'

const disposeViewport = installViewport()
if (import.meta.hot) import.meta.hot.dispose(disposeViewport)
createApp(App).use(router).mount('#app')
