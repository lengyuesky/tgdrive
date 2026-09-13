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
    include: ['src/**/*.test.ts'],
    restoreMocks: true,
  },
})
