import { defineConfig } from '@playwright/test'

const origin = process.env.TGDRIVE_SYNTHETIC_ORIGIN
if (!origin || new URL(origin).hostname !== '127.0.0.1') throw new Error('真实媒体库回归仅允许调用独立容器发布到 127.0.0.1 的合成宿主')

export default defineConfig({
  testDir: './integration',
  timeout: 90_000,
  expect: { timeout: 10_000 },
  workers: 1,
  reporter: 'list',
  outputDir: process.env.PLAYWRIGHT_OUTPUT_DIR ?? '/tmp/tgdrive-library-browser-results',
  use: {
    baseURL: origin,
    viewport: { width: 1440, height: 1000 },
    launchOptions: {
      ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}),
      args: ['--disable-dev-shm-usage'],
    },
    trace: 'retain-on-failure',
  },
})
