import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'
import { appFileAccess, appResolve } from './app-paths'

export default defineConfig({
  resolve: appResolve,
  server: { fs: { allow: appFileAccess } },
  plugins: [vue()],
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.ts', '../apps/frontend/**/*.test.ts', '../apps/reader/**/*.test.ts', '../apps/books/**/*.test.ts', '../apps/comics/**/*.test.ts', '../apps/cinema/**/*.test.ts'],
    restoreMocks: true,
  },
})
