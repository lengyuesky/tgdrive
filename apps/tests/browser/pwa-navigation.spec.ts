import { expect, test, type Page, type Response } from '@playwright/test'

test.use({ viewport: { width: 393, height: 852 }, isMobile: true, hasTouch: true })

test.beforeEach(async ({ page }) => {
  expect(await (await page.request.get('/__apps_fixture')).json()).toEqual({ fixture: 'tgdrive-apps-tests' })
  expect((await page.request.post('/api/auth/login', { data: { username: 'apps-test', password: 'apps-test-only' } })).ok()).toBe(true)
})

test('主屏幕入口和前端路由的缓存校验返回 304，而不是 0 字节的 404 文件', async ({ page }) => {
  const manifest = await (await page.request.get('/manifest.webmanifest')).json()
  expect(manifest.display).toBe('standalone')
  for (const path of [manifest.start_url, '/files', '/files/%E6%B5%8B%E8%AF%95', '/login', '/apps/books', '/transfers', '/me']) {
    const first = await page.request.get(path)
    expect(first.status(), `${path} 首次打开`).toBe(200)
    expect(first.headers()['content-type']).toContain('text/html')
    expect(await first.text()).toContain('<div id="app">')
    expect(first.headers()['content-disposition']).toBeUndefined()
    for (const [name, value] of [
      ['if-none-match', first.headers().etag],
      ['if-modified-since', first.headers()['last-modified']],
    ]) {
      expect(value, `${path} 缓存校验标识`).toBeTruthy()
      const cached = await page.request.get(path, { headers: { [name!]: value! } })
      expect(cached.status(), `${path} 的 ${name} 校验`).toBe(304)
      expect((await cached.body()).length).toBe(0)
    }
  }
})

test('模拟 iOS 主屏幕文件页反复重载、切回前台和重新打开后仍显示网盘', async ({ page, context }, info) => {
  await context.addInitScript(() => Object.defineProperty(navigator, 'standalone', { configurable: true, value: true }))
  const errors: string[] = [], downloads: string[] = []
  const observe = (target: Page) => {
    target.on('pageerror', (error) => errors.push(error.message))
    target.on('download', (download) => downloads.push(download.suggestedFilename()))
  }
  observe(page); context.on('page', observe)
  const check = async (target: Page, response: Response | null) => {
    expect(response).not.toBeNull()
    expect([200, 304]).toContain(response!.status())
    await expect(target.locator('html')).toHaveClass(/is-standalone/)
    await expect(target.getByRole('navigation', { name: '主导航' })).toBeVisible()
    await expect(target.locator('[data-entry-id]').first()).toBeVisible()
    await expect(target).toHaveURL(/\/files\/$/)
  }
  const first = await page.goto('/files/')
  expect(first?.status()).toBe(200)
  await check(page, first)
  for (let attempt = 0; attempt < 3; attempt++) {
    const background = await context.newPage()
    await background.goto('about:blank'); await background.bringToFront()
    await page.bringToFront(); await background.close()
    await check(page, await page.reload())
  }
  await page.close()
  const reopened = await context.newPage()
  await check(reopened, await reopened.goto('/files/'))
  await reopened.screenshot({ path: info.outputPath('iOS主屏幕-缓存重载后文件页.png') })
  expect(downloads).toEqual([])
  expect(errors).toEqual([])
})
