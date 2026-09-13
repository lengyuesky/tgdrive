import { fileURLToPath } from 'node:url'

/** 根目录 apps 的界面源码复用主站依赖，确保只有一份 Vue 运行时。 */
export const appResolve = {
  alias: {
    '@apps': fileURLToPath(new URL('../apps/frontend', import.meta.url)),
    '@drive': fileURLToPath(new URL('./src', import.meta.url)),
    'mediabunny': fileURLToPath(new URL('./node_modules/mediabunny/dist/modules/src/index.js', import.meta.url)),
    'dompurify': fileURLToPath(new URL('./node_modules/dompurify/dist/purify.es.mjs', import.meta.url)),
    '@zip.js/zip.js': fileURLToPath(new URL('./node_modules/@zip.js/zip.js', import.meta.url)),
  },
  dedupe: ['vue', 'vue-router', 'element-plus', '@element-plus/icons-vue', 'axios', 'vitest'],
}

export const appFileAccess = [
  fileURLToPath(new URL('.', import.meta.url)),
  appResolve.alias['@apps'],
  ...['reader', 'books', 'comics', 'cinema', 'sdk', 'tests'].map((name) => fileURLToPath(new URL(`../apps/${name}`, import.meta.url))),
]
