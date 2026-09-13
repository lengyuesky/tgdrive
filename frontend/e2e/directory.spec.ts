import { expect, test, type Page } from '@playwright/test'

const TOTAL = 767697
const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64')

function file(id: number, extension = 'jpg') {
  return { id: id + 1, name: `图片${String(id).padStart(7, '0')}.${extension}`, is_dir: false,
    size: 100, created_at: 1, modified_at: 1700000000 - Math.floor(id / 300) * 86400,
    sha256: null, favorite: false, has_thumbnail: false }
}

async function mockDrive(page: Page, options: { media?: boolean; failSecond?: boolean; slow?: boolean } = {}) {
  const requests: URL[] = []
  const files: string[] = []
  const errors: string[] = []
  let active = 0
  let peak = 0
  let fail = options.failSecond
  page.on('pageerror', (error) => errors.push(error.message))
  // 所有 API 在浏览器层拦截，绝不访问生产数据库和用户原图。
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === '/api/fs/entries') {
      requests.push(url)
      if (url.searchParams.get('kind') === 'dir') {
        return route.fulfill({ json: { path: '/', entries: [{ ...file(900000), name: '大目录', is_dir: true }], has_more: false, next_cursor: null, total: 1 } })
      }
      const start = Number(url.searchParams.get('cursor') ?? 0)
      if (start && fail) { fail = false; return route.fulfill({ status: 503, json: { error: '测试续载失败' } }) }
      if (options.slow && url.searchParams.get('q') === '慢') await new Promise((resolve) => setTimeout(resolve, 800))
      const q = url.searchParams.get('q') ?? ''
      const count = q ? 1 : 200
      const entries = Array.from({ length: count }, (_, i) => {
        const item = file(start + i, options.media ? 'jpg' : 'txt')
        if (q) item.name = q + '.txt'
        // 超限图片与视频只显示图标，不能回退下载原视频。
        if (options.media && i === 5) item.size = 21 * 1024 * 1024
        if (options.media && i === 6) { item.name = '测试.mp4'; item.has_thumbnail = true }
        return item
      })
      return route.fulfill({ json: { path: url.searchParams.get('path'), entries,
        has_more: !q && start + count < TOTAL, next_cursor: q ? null : String(start + count), ...(start === 0 ? { total: q ? 1 : TOTAL } : {}) } })
    }
    if (url.pathname === '/api/fs/download' || url.pathname === '/api/fs/thumbnail') {
      files.push(url.pathname + '?' + url.searchParams.toString())
      active++; peak = Math.max(peak, active)
      await new Promise((resolve) => setTimeout(resolve, 25))
      try {
        if (url.pathname === '/api/fs/thumbnail') return await route.fulfill({ status: 404, body: '' })
        await route.fulfill({ contentType: 'image/png', body: png })
      } finally { active-- }
      return
    }
    if (url.pathname === '/api/info') return route.fulfill({ json: { username: '测试', files: TOTAL,
      dirs: 1, total_size: 1000000, configured: true, official_api: false, api_base: '', chunk_size: 100 } })
    if (url.pathname === '/api/uploads') return route.fulfill({ json: { sessions: [] } })
    if (url.pathname === '/api/auth/me') return route.fulfill({ json: { username: '测试' } })
    if (url.pathname === '/api/fs/list') throw new Error('主目录不应调用旧全量接口')
    return route.fulfill({ json: { ok: true, items: [] } })
  })
  return { requests, files, errors, peak: () => peak }
}

const viewport = (page: Page) => page.locator('.panel [data-testid="virtual-viewport"]')
const cells = (page: Page) => page.locator('.panel [data-entry-id]')
async function bottom(page: Page) {
  await viewport(page).evaluate((el) => { el.scrollTop = el.scrollHeight })
}

test('76万条目录首屏有界、连续加载、已加载全选、四视图与响应式锚点', async ({ page }) => {
  const state = await mockDrive(page)
  const begin = Date.now()
  await page.goto('/files/大目录')
  await expect(page.locator('.crumb-count')).toHaveText(`已加载 200 / 匹配 ${TOTAL}`)
  expect(Date.now() - begin).toBeLessThan(5000)
  expect(state.requests.filter((u) => u.searchParams.get('kind') !== 'dir')).toHaveLength(1)
  expect(await cells(page).count()).toBeLessThanOrEqual(200)
  await page.locator('.panel [aria-label="全选已加载项目"]').check()
  await bottom(page)
  await expect(page.locator('.crumb-count')).toContainText('已加载 400')
  const next = page.locator('.panel [data-entry-id="201"] input[type="checkbox"]')
  await expect(next).not.toBeChecked()
  await viewport(page).evaluate((el) => { el.scrollTop = 0 })
  await expect(page.locator('.panel [data-entry-id="1"] input')).toBeChecked()
  for (const label of ['网格视图', '大图视图', '时间线(按日期看图)', '列表视图']) {
    await page.getByRole('button', { name: label, exact: true }).click()
    await expect(cells(page).first()).toBeVisible()
    expect(await cells(page).count()).toBeLessThanOrEqual(200)
  }
  expect(state.requests.some((u) => u.searchParams.get('kind') === 'file' && u.searchParams.get('sort') === 'mtime' && u.searchParams.get('order') === 'desc')).toBe(true)
  await page.getByRole('button', { name: '网格视图', exact: true }).click()
  await viewport(page).evaluate((el) => { el.scrollTop = 1000 })
  await page.setViewportSize({ width: 3600, height: 1600 })
  expect(await cells(page).count()).toBeLessThanOrEqual(200)
  await page.setViewportSize({ width: 650, height: 800 })
  await expect(cells(page).first()).toBeAttached()
  expect(state.errors).toEqual([])
})

test('失败续批可重试，服务器过滤及旧请求隔离', async ({ page }) => {
  const state = await mockDrive(page, { failSecond: true, slow: true })
  await page.goto('/files/大目录')
  await expect(page.locator('.crumb-count')).toContainText('已加载 200')
  await bottom(page)
  await expect(page.locator('.directory-status')).toContainText('加载失败')
  await page.locator('.directory-status').getByRole('button', { name: '重试' }).click()
  await expect(page.locator('.crumb-count')).toContainText('已加载 400')
  const search = page.getByPlaceholder('过滤当前目录，回车全局搜索')
  await search.fill('慢')
  await expect.poll(() => state.requests.some((u) => u.searchParams.get('q') === '慢')).toBe(true)
  await search.fill('新')
  await expect(page.locator('.entry-name')).toHaveText('新.txt')
  await expect(page.locator('.crumb-count')).toContainText('已加载 1 / 匹配 1')
  // 等待慢响应有机会返回，确认它不能覆盖新代。
  await page.waitForTimeout(900)
  await expect(page.locator('.entry-name')).toHaveText('新.txt')
  expect(state.errors).toEqual([])
})

test('原图限流、超限降级、视频无原片、预览最多三张以及往返滚动资源稳定', async ({ page }, testInfo) => {
  const state = await mockDrive(page, { media: true })
  await page.goto('/files/大目录')
  await expect(page.locator('.directory-media img').first()).toBeVisible()
  expect(state.peak()).toBeLessThanOrEqual(4)
  expect(state.files.some((u) => decodeURIComponent(u).includes('0000005.jpg'))).toBe(false)
  expect(state.files.some((u) => u.startsWith('/api/fs/download') && decodeURIComponent(u).includes('.mp4'))).toBe(false)
  await page.locator('.panel [data-entry-id="3"] .name-cell').click()
  await expect(page.getByRole('dialog', { name: '图片预览' })).toBeVisible()
  expect(await page.locator('.image-preview img').count()).toBe(3)
  await page.getByRole('button', { name: '下一张图片' }).click()
  expect(await page.locator('.image-preview img').count()).toBe(3)
  await page.keyboard.press('Escape')
  await expect(page.locator('.image-preview')).toHaveCount(0)
  const sample = () => page.evaluate(() => ({
    nodes: document.getElementsByTagName('*').length,
    cells: document.querySelectorAll('.panel [data-entry-id]').length,
    imgs: document.querySelectorAll('.directory-media img').length,
  }))
  // 先暖同一范围，随后比较往返期间的 DOM 与主动 GC 后 JS 堆，避免将首次缓存算成泄漏。
  await viewport(page).evaluate((el) => { el.scrollTop = 1800 })
  await page.waitForTimeout(500)
  await viewport(page).evaluate((el) => { el.scrollTop = 0 })
  await page.waitForTimeout(500)
  const cdp = await page.context().newCDPSession(page)
  await cdp.send('HeapProfiler.collectGarbage')
  const before = await sample()
  const heapBefore = await cdp.send('Runtime.getHeapUsage')
  for (let i = 0; i < 20; i++) {
    await viewport(page).evaluate((el) => { el.scrollTop = 1800 })
    await page.waitForTimeout(30)
    await viewport(page).evaluate((el) => { el.scrollTop = 0 })
    await page.waitForTimeout(30)
  }
  await page.waitForTimeout(500)
  await cdp.send('HeapProfiler.collectGarbage')
  const heapAfter = await cdp.send('Runtime.getHeapUsage')
  const end = await sample()
  expect(heapAfter.usedSize - heapBefore.usedSize).toBeLessThan(20 * 1024 * 1024)
  expect(end.nodes - before.nodes).toBeLessThan(200)
  await testInfo.attach('往返资源测量', { body: JSON.stringify({ before, end, heapBefore, heapAfter }), contentType: 'application/json' })
  await page.screenshot({ path: testInfo.outputPath('文件列表.png'), fullPage: true })
  expect(end.cells).toBeLessThanOrEqual(200)
  expect(end.imgs).toBeLessThanOrEqual(end.cells)
  expect(end.nodes).toBeLessThan(15000)
  expect(state.errors).toEqual([])
})
