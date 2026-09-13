import { expect, test, type Page } from '@playwright/test'

function entry(id: number) { return { id, name: `测试文件${id}很长的名称.txt`, is_dir: false, size: 128, created_at: 1, modified_at: 1700000000, sha256: null, favorite: false, has_thumbnail: false } }
async function mock(page: Page, options: { slowUpload?: boolean } = {}) {
  const errors: string[] = []
  const uploads: { dir: string }[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.addInitScript(() => localStorage.setItem('tgdrive.install-dismissed', String(Date.now())))
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === '/api/auth/me') return route.fulfill({ json: { username: '手机测试' } })
    if (url.pathname === '/api/info') return route.fulfill({ json: { username: '手机测试', configured: true, files: 10000, dirs: 2, total_size: 1280000 } })
    if (url.pathname === '/api/fs/entries') {
      const start = Number(url.searchParams.get('cursor') || 0)
      const entries = url.searchParams.get('kind') === 'dir' ? [] : Array.from({ length: 200 }, (_, index) => entry(start + index + 1))
      return route.fulfill({ json: { path: url.searchParams.get('path'), entries, total: 10000, has_more: entries.length > 0, next_cursor: String(start + 200) } })
    }
    if (url.pathname === '/api/uploads') return route.fulfill({ json: { sessions: [], concurrency: 2, bot_count: 1, retry_after: 0 } })
    if (url.pathname === '/api/fs/exists') return route.fulfill({ json: { existing: [] } })
    if (url.pathname === '/api/fs/instant') {
      uploads.push(route.request().postDataJSON())
      if (options.slowUpload) await new Promise((resolve) => setTimeout(resolve, 2000))
      return route.fulfill({ json: { linked: true, pending: false, entry: entry(1) } })
    }
    if (url.pathname === '/api/apps') return route.fulfill({ json: { installed: [], available: [] } })
    if (url.pathname === '/api/shares') return route.fulfill({ json: { items: [{ id: 1, name: '很长的分享文件名'.repeat(8), path: '/文件.txt', token: 'mobile-test', downloads: 0 }] } })
    if (url.pathname === '/api/trash') return route.fulfill({ json: { items: [{ id: 1, name: '已删除文件.txt', orig_path: '/文件夹/已删除文件.txt', size: 128, trashed_at: 1700000000 }] } })
    if (url.pathname === '/api/fs/search') return route.fulfill({ json: { results: [{ ...entry(1), path: '/测试/收藏文件.txt' }] } })
    return route.fulfill({ json: { ok: true } })
  })
  return { errors, uploads }
}
async function noOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true)
}
const list = (page: Page) => page.locator('.panel [data-testid="virtual-viewport"]')

test('小屏四标签、视图、多选、操作面板与窄屏管理卡片', async ({ page }, info) => {
  const state = await mock(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/files/测试')
  await expect(page.getByRole('navigation', { name: '主导航' })).toBeVisible()
  await expect(page.locator('[data-entry-id]').first()).toBeVisible()
  for (const width of [320, 375, 390, 430]) {
    await page.setViewportSize({ width, height: 844 })
    await noOverflow(page)
    expect((await list(page).boundingBox())!.height).toBeGreaterThan(450)
    await expect(page.getByRole('button', { name: '上传或新建' })).toBeVisible()
  }
  await page.getByRole('button', { name: '选择', exact: true }).click()
  await page.locator('[data-entry-id="1"] input').check()
  await expect(page.locator('.mobile-selection-bar')).toContainText('1 项')
  await expect(page.locator('.mobile-tabs')).not.toBeVisible()
  await page.getByRole('button', { name: '完成', exact: true }).click()
  await page.getByRole('button', { name: '视图', exact: true }).click()
  await page.getByRole('button', { name: '网格视图', exact: true }).click()
  await page.getByRole('button', { name: '完成', exact: true }).click()
  await expect(page.locator('.directory-row.grid').first()).toBeVisible()
  await page.getByRole('button', { name: /^更多操作 / }).first().click()
  await expect(page.getByRole('button', { name: '重命名', exact: true })).toBeVisible()
  await noOverflow(page)
  await page.keyboard.press('Escape')
  await page.getByRole('navigation').getByText('我的', { exact: true }).click()
  await page.getByRole('button', { name: /分享管理/ }).click()
  await expect(page.locator('.management-card')).toBeVisible()
  await noOverflow(page)
  await page.screenshot({ path: info.outputPath('手机分享管理.png'), animations: 'disabled' })
  await page.keyboard.press('Escape')
  await page.getByRole('button', { name: /回收站/ }).click()
  await expect(page.getByRole('button', { name: '恢复', exact: true })).toBeVisible()
  expect(state.errors).toEqual([])
})

test('标签切换保留文件滚动与视图，常驻上传保持目标目录', async ({ page }, info) => {
  const state = await mock(page, { slowUpload: true })
  await page.goto('/files/目标目录')
  await expect(page.locator('[data-entry-id]').first()).toBeVisible()
  await list(page).evaluate((element) => { element.scrollTop = 900 })
  const offset = await list(page).evaluate((element) => element.scrollTop)
  await page.getByRole('button', { name: '上传或新建' }).click()
  const chooser = page.waitForEvent('filechooser')
  await page.getByRole('button', { name: '上传文件或照片' }).click()
  await (await chooser).setFiles({ name: '跨页面上传.txt', mimeType: 'text/plain', buffer: Buffer.from('测试上传') })
  await expect(page.getByRole('dialog', { name: '上传队列' })).toBeVisible()
  await page.keyboard.press('Escape')
  await page.getByRole('navigation').getByText('传输', { exact: true }).click()
  await expect(page.locator('#transfers-panel')).toContainText('跨页面上传.txt')
  await expect(page.locator('#transfers-panel')).toContainText('秒传成功')
  expect(state.uploads).toHaveLength(1)
  expect(state.uploads[0]!.dir).toBe('/目标目录')
  await page.getByRole('navigation').getByText('应用', { exact: true }).click()
  await expect(page).toHaveURL(/\/apps$/)
  await page.getByRole('navigation').getByText('文件', { exact: true }).click()
  await expect(page).toHaveURL(/\/files\//)
  await expect.poll(() => list(page).evaluate((element) => element.scrollTop)).toBe(offset)
  await noOverflow(page)
  await page.screenshot({ path: info.outputPath('手机文件页.png'), animations: 'disabled' })
  expect(state.errors).toEqual([])
})

test('主屏幕配置、暗色、断网提示和安装说明', async ({ page }) => {
  const state = await mock(page)
  await page.goto('/me')
  expect(await page.locator('meta[name="viewport"]').getAttribute('content')).toContain('viewport-fit=cover')
  await page.getByRole('button', { name: '添加到主屏幕' }).click()
  await expect(page.getByRole('dialog')).toContainText('Safari')
  await page.getByRole('button', { name: '知道了' }).click()
  await page.getByRole('button', { name: '切换暗色' }).click()
  await expect(page.locator('html')).toHaveClass(/dark/)
  await page.context().setOffline(true)
  await expect(page.getByRole('status')).toContainText('网络已断开')
  await page.context().setOffline(false)
  await noOverflow(page)
  expect(state.errors).toEqual([])
})

test('平板、桌面和手机横屏没有横向溢出，模拟安全区不遮挡列表', async ({ page }) => {
  const state = await mock(page)
  await page.goto('/files/适配目录')
  for (const [width, height] of [[768, 1024], [844, 390], [1024, 768], [1440, 1000], [390, 844]] as const) {
    await page.setViewportSize({ width, height })
    await expect(page.locator('.mobile-file-header')).toHaveCount(width <= 767 ? 1 : 0)
    await expect(page.locator('[data-entry-id]').first()).toBeVisible()
    await noOverflow(page)
    expect((await list(page).boundingBox())!.height).toBeGreaterThan(100)
  }
  await page.evaluate(() => {
    for (const [edge, value] of Object.entries({ top: 47, bottom: 34, left: 12, right: 12 })) {
      document.documentElement.style.setProperty(`--safe-${edge}`, `${value}px`)
    }
  })
  const header = (await page.locator('.mobile-file-header').boundingBox())!
  const tabs = (await page.getByRole('navigation', { name: '主导航' }).boundingBox())!
  const viewport = (await list(page).boundingBox())!
  expect(header.y).toBeGreaterThanOrEqual(47)
  expect(header.x).toBeGreaterThanOrEqual(12)
  expect(tabs.height).toBeGreaterThanOrEqual(90)
  expect(viewport.y + viewport.height).toBeLessThanOrEqual(tabs.y + 1)
  await noOverflow(page)
  expect(state.errors).toEqual([])
})

test('嵌套弹层约束焦点，浏览器返回逐层关闭且不增加历史记录', async ({ page }) => {
  const state = await mock(page)
  await page.goto('/files/')
  await page.getByRole('navigation', { name: '主导航' }).getByText('我的', { exact: true }).click()
  const trigger = page.getByRole('button', { name: /分享管理/ })
  await trigger.click()
  const parent = page.getByRole('dialog', { name: '分享管理', exact: true })
  const remove = parent.getByRole('button', { name: '取消分享', exact: true })
  await remove.click()
  const child = page.getByRole('dialog', { name: '取消分享', exact: true })
  await expect(child).toBeVisible()
  await trigger.evaluate((element) => element.focus())
  await expect.poll(() => child.evaluate((element) => element.contains(document.activeElement))).toBe(true)
  for (let i = 0; i < 4; i++) {
    await page.keyboard.press('Tab')
    expect(await child.evaluate((element) => element.contains(document.activeElement))).toBe(true)
  }
  const historyLength = await page.evaluate(() => history.length)
  await page.goBack()
  await expect(child).not.toBeVisible()
  await expect(parent).toBeVisible()
  await expect(page).toHaveURL(/\/me$/)
  await expect(remove).toBeFocused()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('hidden')
  await page.goBack()
  await expect(parent).not.toBeVisible()
  await expect(page).toHaveURL(/\/me$/)
  await expect(trigger).toBeFocused()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('')
  expect(await page.evaluate(() => history.length)).toBe(historyLength)
  await page.goBack()
  await expect(page).toHaveURL(/\/files\/$/)
  expect(state.errors).toEqual([])
})

test('模拟软键盘隐藏底栏并保持表单可见，输入法确认不误提交', async ({ page }) => {
  const state = await mock(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.addInitScript(() => {
    let height = innerHeight
    const viewport = window.visualViewport!
    Object.defineProperty(viewport, 'height', { get: () => height })
    Object.defineProperty(viewport, 'offsetTop', { get: () => 0 })
    ;(window as unknown as { setTestViewport: (value: number) => void }).setTestViewport = (value) => {
      height = value
      viewport.dispatchEvent(new Event('resize'))
    }
  })
  const visualHeight = (height: number) => page.evaluate((value) => {
    ;(window as unknown as { setTestViewport: (value: number) => void }).setTestViewport(value)
  }, height)
  let created = 0
  page.on('request', (request) => { if (new URL(request.url()).pathname === '/api/fs/mkdir') created++ })
  await page.goto('/files/')
  await visualHeight(480)
  await expect(page.locator('html')).not.toHaveClass(/keyboard-open/)
  await page.getByRole('button', { name: '搜索文件', exact: true }).click()
  const search = page.getByPlaceholder('搜索文件', { exact: true })
  await search.focus()
  await expect(page.locator('html')).toHaveClass(/keyboard-open/)
  await expect(page.getByRole('navigation', { name: '主导航' })).not.toBeVisible()
  await expect(search).toHaveCSS('font-size', '16px')
  const viewport = (await list(page).boundingBox())!
  expect(viewport.y + viewport.height).toBeLessThanOrEqual(481)
  await page.getByRole('button', { name: '上传或新建' }).click()
  await page.getByRole('button', { name: '新建文件夹', exact: true }).click()
  const prompt = page.getByRole('dialog', { name: '新建文件夹', exact: true })
  const input = prompt.getByRole('textbox')
  await input.fill('中文文件夹')
  await expect(page.locator('html')).toHaveClass(/keyboard-open/)
  const box = (await prompt.boundingBox())!
  expect(box.y).toBeGreaterThanOrEqual(0)
  expect(box.y + box.height).toBeLessThanOrEqual(481)
  await input.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true, keyCode: 229 })
  await expect(prompt).toBeVisible()
  expect(created).toBe(0)
  await input.press('Enter')
  await expect(prompt).not.toBeVisible()
  await expect.poll(() => created).toBe(1)
  await visualHeight(844)
  await expect(page.getByRole('navigation', { name: '主导航' })).toBeVisible()
  await noOverflow(page)
  expect(state.errors).toEqual([])
})

test('注销立即关闭上传与确认弹层，重新登录不接收旧会话的迟到结果', async ({ page }) => {
  const state = await mock(page)
  let release!: () => void
  const pending = new Promise<void>((resolve) => { release = resolve })
  let started = false
  await page.route('**/api/fs/instant', async (route) => {
    started = true
    await pending
    await route.fulfill({ json: { linked: true, pending: false, entry: entry(1) } }).catch(() => {})
  })
  try {
    await page.goto('/files/注销前目录')
    await page.getByRole('button', { name: '上传或新建' }).click()
    const chooser = page.waitForEvent('filechooser')
    await page.getByRole('button', { name: '上传文件或照片' }).click()
    await (await chooser).setFiles({ name: '旧会话文件.txt', mimeType: 'text/plain', buffer: Buffer.from('旧会话') })
    await expect.poll(() => started).toBe(true)
    await page.keyboard.press('Escape')
    await page.getByRole('navigation').getByText('我的', { exact: true }).click()
    await page.getByRole('button', { name: /退出登录/ }).click()
    const dialog = page.getByRole('dialog', { name: '退出登录', exact: true })
    await dialog.getByRole('button', { name: '退出登录', exact: true }).click()
    await expect(page).toHaveURL(/\/login$/)
    await expect(page.getByRole('dialog')).not.toBeVisible()
    expect(await page.evaluate(() => document.body.style.overflow)).toBe('')
    release()
    await page.getByRole('textbox', { name: '用户名' }).fill('重新登录')
    await page.getByRole('textbox', { name: '密码' }).fill('测试密码')
    await page.getByRole('button', { name: '登录', exact: true }).click()
    await page.getByRole('navigation').getByText('传输', { exact: true }).click()
    await expect(page.locator('#transfers-panel')).toContainText('暂无上传任务')
    await expect(page.locator('#transfers-panel')).not.toContainText('旧会话文件.txt')
    expect(state.errors).toEqual([])
  } finally { release() }
})

test('管理接口 401 不被弹层阻挡，直接回到无滚动锁的登录页', async ({ page }) => {
  const state = await mock(page)
  await page.route('**/api/shares/1', (route) => route.fulfill({ status: 401, json: { error: '会话已失效' } }))
  await page.goto('/me')
  await page.getByRole('button', { name: /分享管理/ }).click()
  await page.getByRole('dialog', { name: '分享管理', exact: true }).getByRole('button', { name: '取消分享' }).click()
  await page.getByRole('dialog', { name: '取消分享', exact: true }).getByRole('button', { name: '取消分享' }).click()
  await expect(page).toHaveURL(/\/login$/)
  await expect(page.getByRole('dialog')).not.toBeVisible()
  expect(await page.evaluate(() => [document.body.style.overflow, document.documentElement.style.overflow])).toEqual(['', ''])
  expect(state.errors).toEqual([])
})

test('公开分享的错误提取码不跳登录，中文输入与窄屏目录卡片可用', async ({ page }) => {
  const state = await mock(page)
  let attempts = 0
  await page.route('**/api/share/**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/authorize')) {
      attempts++
      return route.fulfill(route.request().postDataJSON().password === '正确提取码'
        ? { json: { expires_at: 0 } }
        : { status: 401, json: { error: '提取码错误' } })
    }
    if (path.endsWith('/list')) return route.fulfill({ json: { entries: [{ ...entry(1), name: '很长的分享文件名'.repeat(12) + '.txt' }] } })
    return route.fulfill({ json: { name: '手机公开分享', is_dir: true, need_password: true, downloads: 0 } })
  })
  await page.setViewportSize({ width: 320, height: 700 })
  await page.goto('/s/mobile-test')
  await expect(page.getByRole('navigation')).not.toBeVisible()
  const input = page.getByPlaceholder('请输入提取码')
  await input.fill('错误提取码')
  await input.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true, keyCode: 229 })
  expect(attempts).toBe(0)
  await page.getByRole('button', { name: '提取文件' }).click()
  await expect(page.getByText('提取码错误', { exact: true })).toBeVisible()
  await expect(page).toHaveURL(/\/s\/mobile-test$/)
  await input.fill('正确提取码')
  await input.press('Enter')
  await expect(page.locator('.management-card')).toBeVisible()
  await expect(page.getByRole('button', { name: '下载', exact: true })).toBeVisible()
  await noOverflow(page)
  expect(state.errors).toEqual([])
})

test('视频全屏能力缺失时提供可退出的页面沉浸，返回只退出顶层', async ({ page }) => {
  const state = await mock(page)
  await page.addInitScript(() => {
    Object.defineProperty(HTMLElement.prototype, 'requestFullscreen', { value: undefined, configurable: true })
    Object.defineProperty(HTMLVideoElement.prototype, 'webkitEnterFullscreen', { value: undefined, configurable: true })
  })
  await page.route('**/api/fs/entries?**', (route) => {
    const url = new URL(route.request().url())
    return route.fulfill({ json: { path: url.searchParams.get('path'), entries: url.searchParams.get('kind') === 'dir' ? [] : [{ ...entry(1), name: '测试视频.mp4' }], total: 1, has_more: false, next_cursor: null } })
  })
  let mediaRequests = 0
  page.on('request', (request) => { if (request.url().includes('/api/fs/download')) mediaRequests++ })
  await page.goto('/files/')
  await page.locator('[data-entry-id="1"] .name-cell').click()
  const preview = page.getByRole('dialog', { name: '测试视频.mp4', exact: true })
  await expect(preview).toBeVisible()
  await expect(page.getByRole('button', { name: '点击播放', exact: true })).toBeVisible()
  expect(mediaRequests).toBe(0)
  await page.getByRole('button', { name: '全屏播放', exact: true }).click()
  await expect(page.locator('.vp-immersive')).toBeVisible()
  await expect(page.getByRole('button', { name: '退出全屏', exact: true })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.locator('.vp-immersive')).not.toBeVisible()
  await expect(preview).toBeVisible()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('hidden')
  // 播放器快捷键不能截走按钮的原生空格激活，也不能因此启动下载。
  await page.getByRole('button', { name: '全屏播放', exact: true }).focus()
  await page.keyboard.press('Space')
  await expect(page.locator('.vp-immersive')).toBeVisible()
  await page.getByRole('button', { name: '退出全屏', exact: true }).click()
  await expect(page.locator('.vp-immersive')).not.toBeVisible()
  await page.keyboard.press('Escape')
  await expect(preview).not.toBeVisible()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('')
  expect(mediaRequests).toBe(0)
  expect(state.errors).toEqual([])
})

test('注销请求失败时保留当前页面并明确提示，不假装已退出', async ({ page }) => {
  const state = await mock(page)
  await page.route('**/api/auth/logout', (route) => route.fulfill({ status: 503, json: { error: '服务暂时不可用' } }))
  await page.goto('/me')
  await page.getByRole('button', { name: /退出登录/ }).click()
  await page.getByRole('dialog', { name: '退出登录', exact: true }).getByRole('button', { name: '退出登录', exact: true }).click()
  await expect(page.getByText('退出登录失败：服务暂时不可用', { exact: true })).toBeVisible()
  await expect(page).toHaveURL(/\/me$/)
  await expect(page.getByRole('navigation', { name: '主导航' })).toBeVisible()
  expect(state.errors).toEqual([])
})
