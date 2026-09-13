import { expect, test } from '@playwright/test'

test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true })
test.beforeEach(async ({ page }) => {
  // 只访问隔离夹具，以下测试不会保存账号修改。
  expect(await (await page.request.get('/__apps_fixture')).json()).toEqual({ fixture: 'tgdrive-apps-tests' })
  expect((await page.request.post('/api/auth/login', { data: { username: 'apps-test', password: 'apps-test-only' } })).ok()).toBe(true)
  await page.addInitScript(() => localStorage.setItem('tgdrive.install-dismissed', String(Date.now())))
})

test('手机设置保留未保存修改，确认离开不会被弹层关闭动画挡住', async ({ page }, info) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/admin?s=account')
  const username = page.locator('input[autocomplete="username"]')
  await expect(username).toHaveValue('apps-test')
  await username.fill('尚未保存的测试用户名')
  await expect(page.locator('.savebar')).toBeVisible()
  await expect(username).toHaveCSS('font-size', '16px')
  for (const width of [320, 375, 390, 430]) {
    await page.setViewportSize({ width, height: 844 })
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true)
    await expect(page.getByRole('button', { name: '保存并生效', exact: true })).toBeVisible()
  }
  const files = page.getByRole('navigation', { name: '主导航' }).getByText('文件', { exact: true })
  await files.click()
  const dialog = page.getByRole('dialog', { name: '放弃未保存的改动?', exact: true })
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: '留在本页' }).click()
  await expect(page).toHaveURL(/\/admin\?s=account$/)
  await expect(username).toHaveValue('尚未保存的测试用户名')
  await page.screenshot({ path: info.outputPath('手机设置未保存.png'), animations: 'disabled' })
  await files.click()
  await dialog.getByRole('button', { name: '直接离开' }).click()
  await expect(page).toHaveURL(/\/files\/$/)
  await expect(page.getByRole('dialog')).not.toBeVisible()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('')
  expect(errors).toEqual([])
})

test('设置有未保存修改时收到 401，认证跳转仍可立即清理页面', async ({ page }) => {
  await page.goto('/admin?s=account')
  const username = page.locator('input[autocomplete="username"]')
  await expect(username).toHaveValue('apps-test')
  await username.fill('不会被提交的用户名')
  await page.route('**/api/admin/config', (route) => route.fulfill({ status: 401, json: { error: '会话已失效' } }))
  await page.getByRole('button', { name: '刷新', exact: true }).click()
  await expect(page).toHaveURL(/\/login$/)
  await expect(page.getByRole('dialog')).not.toBeVisible()
  await expect(page.locator('.savebar')).not.toBeVisible()
  expect(await page.evaluate(() => document.body.style.overflow)).toBe('')
})
