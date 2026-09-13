import { fileURLToPath } from 'node:url'

/** 应用中心与网盘界面均为宿主源码，共用同一份运行时依赖。 */
export const appResolve = {
  alias: {
    '@apps': fileURLToPath(new URL('./src/apps', import.meta.url)),
    '@drive': fileURLToPath(new URL('./src', import.meta.url)),
  },
  dedupe: ['vue', 'vue-router', 'element-plus', '@element-plus/icons-vue', 'axios', 'vitest'],
}

export const appFileAccess = [fileURLToPath(new URL('.', import.meta.url))]
