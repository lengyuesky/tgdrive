import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import Components from 'unplugin-vue-components/vite'
import { ElementPlusResolver } from 'unplugin-vue-components/resolvers'
import { appFileAccess, appResolve } from './app-paths'

// dev 模式下 API 与 WebDAV 都代理到本地后端
export default defineConfig({
  resolve: appResolve,
  plugins: [
    vue(),
    Components({
      resolvers: [ElementPlusResolver({ importStyle: 'css' })],
      dts: 'src/components.d.ts',
    }),
  ],
  server: {
    fs: { allow: appFileAccess },
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/dav': 'http://127.0.0.1:8080',
    },
  },
  build: {
    outDir: 'dist',
    chunkSizeWarningLimit: 1500,
  },
})
