import { defineConfig, devices } from '@playwright/test'
export default defineConfig({
  testDir: './mobile-e2e', timeout: 45_000, expect: { timeout: 10_000 }, workers: 1,
  outputDir: process.env.PLAYWRIGHT_MOBILE_OUTPUT_DIR ?? '/tmp/tgdrive-mobile-results',
  reporter: 'list', use: { baseURL: 'http://127.0.0.1:4179', trace: 'retain-on-failure' },
  projects: [
    { name: '手机 Chromium', use: { ...devices['Pixel 7'], browserName: 'chromium', launchOptions: { ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}), args: ['--disable-dev-shm-usage'] } } },
    { name: 'iPhone WebKit', use: { ...devices['iPhone 13'], browserName: 'webkit' } },
  ],
  webServer: { command: 'npm run preview -- --host 127.0.0.1 --port 4179 --strictPort', url: 'http://127.0.0.1:4179', reuseExistingServer: false, timeout: 30_000 },
})
