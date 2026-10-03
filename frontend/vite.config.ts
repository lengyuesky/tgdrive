import { brotliCompressSync, gzipSync } from 'node:zlib'
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'
import { defineConfig, type Plugin } from 'vite'
import vue from '@vitejs/plugin-vue'
import Components from 'unplugin-vue-components/vite'
import { ElementPlusResolver } from 'unplugin-vue-components/resolvers'
import { appFileAccess, appResolve } from './app-paths'

// 为构建产物生成 .gz / .br 预压缩变体，服务端 ServeDir 直发、零运行时压缩开销
//（未命中变体的小文件仍由服务端 CompressionLayer 兜底）。
// Vite 在 buildStart 就把 public/ 拷进 outDir，因此 writeBundle 时磁盘上
// bundle 产物与 public 文件都已就绪，统一扫描一遍即可。
const PRECOMPRESS_EXTENSIONS = new Set([
  '.js', '.mjs', '.css', '.html', '.svg', '.json', '.webmanifest', '.xml', '.txt',
])
const PRECOMPRESS_MIN_BYTES = 256

function precompress(): Plugin {
  let outDir = 'dist'
  return {
    name: 'tgdrive-precompress',
    apply: 'build',
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir)
    },
    writeBundle() {
      const walk = (dir: string) => {
        for (const entry of readdirSync(dir)) {
          const full = join(dir, entry)
          if (statSync(full).isDirectory()) {
            walk(full)
            continue
          }
          if (!PRECOMPRESS_EXTENSIONS.has(extname(entry))) continue
          if (existsSync(`${full}.gz`) || existsSync(`${full}.br`)) continue
          const data = readFileSync(full)
          if (data.byteLength < PRECOMPRESS_MIN_BYTES) continue
          writeFileSync(`${full}.gz`, gzipSync(data, { level: 9 }))
          writeFileSync(`${full}.br`, brotliCompressSync(data))
        }
      }
      walk(outDir)
    },
  }
}

// 第三方运行时（Vue/路由/网络层/虚拟滚动）固定成独立 vendor 块：
// 业务代码迭代不会改变这些块的哈希，老用户升级后仍能命中浏览器缓存；
// Element Plus 保持按组件随路由分块，不强制合入首屏 vendor。
function vendorChunks(id: string): string | undefined {
  if (!id.includes('node_modules')) return undefined
  if (/[\\/]node_modules[\\/](vue|vue-router)([\\/]|$)/.test(id)) return 'vendor-vue'
  if (/[\\/]node_modules[\\/]@vue[\\/][a-z-]+[\\/]/.test(id)) return 'vendor-vue'
  if (/[\\/]node_modules[\\/]axios[\\/]/.test(id)) return 'vendor-net'
  if (/[\\/]node_modules[\\/]@tanstack[\\/]/.test(id)) return 'vendor-virtual'
  return undefined
}

// dev 模式下 API 与 WebDAV 都代理到本地后端
export default defineConfig({
  resolve: appResolve,
  plugins: [
    vue(),
    Components({
      resolvers: [ElementPlusResolver({ importStyle: 'css' })],
      dts: 'src/components.d.ts',
    }),
    precompress(),
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
    rollupOptions: {
      output: {
        manualChunks: vendorChunks,
      },
    },
  },
})
