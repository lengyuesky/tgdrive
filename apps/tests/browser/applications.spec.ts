import { expect, test, type Page } from '@playwright/test'
import { execFile } from 'node:child_process'
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { promisify } from 'node:util'

const run = promisify(execFile)
let temporary: string

test.beforeAll(async () => { temporary = await mkdtemp(`${tmpdir()}/tgdrive-app-package-test-`) })
test.afterAll(async () => { await rm(temporary, { recursive: true, force: true }) })
test.beforeEach(async ({ page }) => {
  // 只有确认独立夹具身份后才执行安装、卸载等写操作。
  expect(await (await page.request.get('/__apps_fixture')).json()).toEqual({ fixture: 'tgdrive-apps-tests' })
  expect((await page.request.post('/__apps_fixture/media', { data: { enabled: false } })).ok()).toBe(true)
  expect((await page.request.post('/api/auth/login', { data: { username: 'apps-test', password: 'apps-test-only' } })).ok()).toBe(true)
  const index = await (await page.request.get('/api/apps')).json()
  for (const app of index.installed) expect((await page.request.delete(`/api/apps/${app.manifest.id}?purge_data=true`)).ok()).toBe(true)
})

async function fileSidebar(page: Page) {
  const width = page.viewportSize()!.width
  if (width < 1024) {
    const toggle = page.getByRole('button', { name: width <= 767 ? '打开目录导航' : '打开侧边栏', exact: true })
    await toggle.click()
    const drawer = page.getByRole('dialog', { name: '导航', exact: true })
    await expect(drawer).toBeVisible()
    return drawer.getByRole('complementary', { name: '目录侧边栏' })
  }
  const sidebar = page.getByRole('complementary', { name: '目录侧边栏' })
  await expect(sidebar).toBeVisible()
  return sidebar
}

async function installShorts(page: Page) {
  await page.goto('/files/')
  const sidebar = await fileSidebar(page)
  const center = sidebar.getByRole('button', { name: '应用中心', exact: true })
  await expect(center).toBeVisible()
  await expect(page.locator('.topbar').getByRole('button', { name: '应用中心', exact: true })).toHaveCount(0)
  const shareBox = await sidebar.getByRole('button', { name: '分享管理', exact: true }).boundingBox()
  const appBox = await sidebar.getByRole('region', { name: '应用', exact: true }).boundingBox()
  expect(appBox!.y).toBeGreaterThanOrEqual(shareBox!.y + shareBox!.height)
  await center.click()
  await expect(page.getByRole('tab', { name: /已安装/ })).toHaveAttribute('aria-selected', 'true')
  await page.getByRole('tab', { name: /可安装/ }).click()
  await page.locator('[data-app-id="shorts"]').getByRole('button', { name: '安装', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: '安装应用', exact: true })
  await expect(dialog).toContainText('读取文件内容')
  await dialog.getByRole('button', { name: '同意并安装' }).click()
  await expect(dialog).not.toBeVisible()
  await expect(page.locator('[data-app-id="shorts"]')).toContainText('已启用')
  // 测试库还包含横屏影视与故意损坏的文件；短视频用例只随机选取自己的竖屏夹具。
  expect((await page.request.patch('/api/apps/shorts/settings', { data: { source_dir: '/测试视频' } })).ok()).toBe(true)
}

test('应用中心默认已安装，空状态不自动跳转，列表刷新保留选择', async ({ page }) => {
  await page.goto('/apps')
  const installed = page.getByRole('tab', { name: /已安装/ }), available = page.getByRole('tab', { name: /可安装/ })
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await expect(page.getByText('还没有安装应用', { exact: true })).toBeVisible()
  await expect(page.locator('[data-app-id]')).toHaveCount(0)
  await page.getByRole('button', { name: '浏览可安装应用' }).click()
  await expect(available).toHaveAttribute('aria-selected', 'true')
  await page.getByRole('button', { name: '刷新', exact: true }).click()
  await expect(page.locator('[data-app-id="books"]')).toBeVisible()
  await expect(available).toHaveAttribute('aria-selected', 'true')
  await page.reload(); await expect(installed).toHaveAttribute('aria-selected', 'true')
  await installShorts(page)
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await page.getByPlaceholder('搜索应用').fill('不存在的应用')
  await expect(page.getByText('没有匹配的应用', { exact: true })).toBeVisible()
  await page.getByPlaceholder('搜索应用').fill('')
  await page.reload()
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await expect(page.locator('[data-app-id="shorts"]')).toBeVisible()
})

test('短视频安装、实际播放、收藏、设置与移动端布局', async ({ page }, testInfo) => {
  const pageErrors: string[] = []
  const rangeResponses: number[] = []
  page.on('pageerror', (error) => pageErrors.push(error.message))
  page.on('response', (response) => {
    if (response.url().includes('/api/apps/media/') && response.status() === 206) rangeResponses.push(response.status())
  })
  await installShorts(page)
  await page.screenshot({ path: testInfo.outputPath('应用中心.png'), fullPage: true })
  await page.getByRole('button', { name: '打开', exact: true }).click()
  const frame = page.frameLocator('iframe')
  await expect(frame.locator('#video')).toBeVisible()
  await expect.poll(() => frame.locator('#video').evaluate((node) => (node as HTMLVideoElement).currentTime)).toBeGreaterThan(0.1)
  const dimensions = await frame.locator('#video').evaluate((node) => {
    const video = node as HTMLVideoElement
    return { width: video.videoWidth, height: video.videoHeight, boxHeight: video.getBoundingClientRect().height, viewport: innerHeight }
  })
  expect(dimensions.width).toBe(320)
  expect(dimensions.height).toBe(568)
  expect(dimensions.boxHeight).toBeLessThanOrEqual(dimensions.viewport)
  expect(rangeResponses.length).toBeGreaterThan(0)
  expect(await frame.locator('body').evaluate(() => { try { void parent.document.title; return false } catch { return true } })).toBe(true)
  const favorite = frame.locator('#favorite')
  const oldFavorite = await favorite.getAttribute('aria-pressed')
  await favorite.click()
  await expect(favorite).toHaveAttribute('aria-pressed', oldFavorite === 'true' ? 'false' : 'true')
  const previousName = await frame.locator('#video-name').innerText()
  await frame.getByRole('button', { name: '下一个视频', exact: true }).click()
  await expect(frame.locator('#video-name')).not.toHaveText(previousName)
  await page.getByRole('button', { name: '应用设置', exact: true }).click()
  const settings = page.getByRole('dialog', { name: '短视频设置', exact: true })
  await settings.getByRole('button', { name: '选择目录', exact: true }).click()
  const picker = page.getByRole('dialog', { name: '选择取材文件夹', exact: true })
  await picker.getByText('空目录', { exact: true }).click()
  await picker.getByRole('button', { name: '确定', exact: true }).click()
  await settings.getByRole('button', { name: '保存设置', exact: true }).click()
  await expect(frame.locator('#empty')).toBeVisible()
  await expect(frame.locator('#empty-description')).toContainText('/空目录')
  await frame.getByRole('button', { name: '选择取材文件夹' }).click()
  await expect(settings.locator('#app-setting-source_dir')).toHaveValue('/空目录')
  await settings.locator('#app-setting-source_dir').fill('/测试视频')
  await settings.getByRole('button', { name: '保存设置', exact: true }).click()
  await expect(settings).not.toBeVisible()
  await expect(frame.locator('#video')).toBeVisible()
  await expect.poll(() => frame.locator('#video').evaluate((node) => (node as HTMLVideoElement).currentTime)).toBeGreaterThan(0.1)
  await page.screenshot({ path: testInfo.outputPath('短视频桌面.png') })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(frame.getByRole('button', { name: '下一个视频', exact: true })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  expect(await frame.locator('#video').evaluate((node) => node.getBoundingClientRect().height <= innerHeight)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('短视频手机.png') })

  await expect(page.locator('.host-toolbar')).not.toBeVisible()
  await page.frameLocator('iframe').getByRole('button', { name: '退出短视频', exact: true }).click()
  await expect(page).toHaveURL(/\/files\//)
  await expect(page.locator('iframe')).toHaveCount(0)
  expect(pageErrors).toEqual([])
})

test('退出短视频会在文件传输完成前释放服务端读取', async ({ page }) => {
  await installShorts(page)
  await page.request.post('/__apps_fixture/media', { data: { enabled: true } })
  await page.getByRole('button', { name: '打开', exact: true }).click()
  const stats = async () => (await page.request.get('/__apps_fixture/media')).json()
  await expect.poll(async () => (await stats()).active).toBeGreaterThan(0)
  await expect.poll(async () => (await stats()).bytes_sent).toBeGreaterThan(0)
  await page.getByRole('button', { name: '返回文件', exact: true }).click()
  await expect(page.locator('iframe')).toHaveCount(0)
  await expect.poll(async () => (await stats()).active, { timeout: 3000 }).toBe(0)
  const result = await stats()
  expect(result.bytes_sent).toBeLessThan(result.file_size)
})

async function packagePath(version: string, permissions: string[]) {
  const source = `${temporary}/source-${version}`
  await mkdir(source, { recursive: true })
  await writeFile(`${source}/app.json`, JSON.stringify({
    id: 'sample-viewer', name: '独立测试应用', version, api_version: 1, min_host_version: '0.1.0',
    description: '验证独立导入和版本更新。', author: '测试夹具', entry: 'index.html', permissions,
    settings: [{ key: 'note', label: '说明文字', description: '更新后保留。', type: 'string', default: '初始值' }],
  }))
  await writeFile(`${source}/index.html`, '<!doctype html><html lang="zh-CN"><head><meta charset="UTF-8"><script defer src="./tgdrive-sdk.js"></script><script defer src="./app.js"></script></head><body><h1 id="version"></h1><p id="note"></p><p id="permission"></p></body></html>')
  await writeFile(`${source}/app.js`, `window.tgdrive.ready.then(async (context) => {
    document.getElementById('version').textContent = '独立运行 ' + context.version;
    document.getElementById('note').textContent = (await window.tgdrive.settings.get()).note;
    try { await window.tgdrive.files.search({}); document.getElementById('permission').textContent = '允许读取'; }
    catch (error) { document.getElementById('permission').textContent = error.message; }
  });`)
  await run(process.execPath, [resolve('../apps/package.mjs'), source, `${temporary}/packages`])
  return `${temporary}/packages/sample-viewer-${version}.tgapp`
}

async function importApp(page: Page, path: string, update = false) {
  await page.locator('input[type="file"][accept=".tgapp,.zip"]').setInputFiles(path)
  const dialog = page.getByRole('dialog', { name: update ? '更新应用' : '安装应用', exact: true })
  await expect(dialog).toBeVisible()
  return dialog
}

async function uninstallApp(page: Page, purge: boolean) {
  await page.locator('[data-app-id="sample-viewer"]').getByRole('button', { name: '更多应用操作' }).click()
  await page.getByRole('menuitem', { name: '卸载应用', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: '卸载应用', exact: true })
  if (purge) {
    await dialog.getByText('同时清除这个应用的数据（设置、阅读记录和书签）', { exact: true }).click()
    await expect(dialog.getByRole('checkbox', { name: '同时清除这个应用的数据（设置、阅读记录和书签）' })).toBeChecked()
  }
  await dialog.getByRole('button', { name: '卸载', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  await expect(page.locator('[data-app-id="sample-viewer"]')).toHaveCount(0)
}

test('独立插件导入、权限限制、更新撤销旧页面、停用、固定入口与保留配置卸载', async ({ page }, testInfo) => {
  const first = await packagePath('1.0.0', [])
  const second = await packagePath('1.1.0', ['files.read'])
  await page.goto('/apps')
  await (await importApp(page, first)).getByRole('button', { name: '同意并安装' }).click()
  const card = page.locator('[data-app-id="sample-viewer"]')
  await expect(card).toContainText('已启用')
  await card.getByRole('button', { name: '设置', exact: true }).click()
  const settings = page.getByRole('dialog', { name: '独立测试应用设置' })
  await expect(settings.locator('#app-setting-note')).toHaveValue('初始值')
  await settings.locator('#app-setting-note').fill('保留的配置')
  await settings.getByRole('button', { name: '保存设置' }).click()
  await expect(settings).not.toBeVisible()
  const oldPage = await page.context().newPage()
  await oldPage.goto('/apps/sample-viewer')
  await expect(oldPage.frameLocator('iframe').locator('#version')).toHaveText('独立运行 1.0.0')
  await expect(oldPage.frameLocator('iframe').locator('#permission')).toContainText('没有 files.read 权限')
  const update = await importApp(page, second, true)
  await expect(update).toContainText('新增')
  await update.getByRole('button', { name: '同意并更新' }).click()
  await expect(update).not.toBeVisible()
  await expect(card).toContainText('v1.1.0')
  await expect(oldPage.locator('iframe')).toHaveCount(0)
  await expect(oldPage.locator('body')).toContainText('更新')
  await oldPage.getByRole('button', { name: '重新打开' }).click()
  await expect(oldPage.frameLocator('iframe').locator('#version')).toHaveText('独立运行 1.1.0')
  await expect(oldPage.frameLocator('iframe').locator('#note')).toHaveText('保留的配置')
  await expect(oldPage.frameLocator('iframe').locator('#permission')).toHaveText('允许读取')
  await card.getByRole('button', { name: '更多应用操作' }).click()
  await page.getByRole('menuitem', { name: '停用应用', exact: true }).click()
  await expect(card).toContainText('已停用')
  await expect(card.getByRole('button', { name: '打开', exact: true })).toBeDisabled()
  await expect(oldPage.locator('iframe')).toHaveCount(0)
  await card.getByRole('button', { name: '更多应用操作' }).click()
  await page.getByRole('menuitem', { name: '启用应用', exact: true }).click()
  await expect(card).toContainText('已启用')
  await card.getByRole('button', { name: '更多应用操作' }).click()
  await page.getByRole('menuitem', { name: '固定到侧边栏', exact: true }).click()
  await expect(card).toContainText('已固定')
  await page.getByRole('button', { name: '返回文件', exact: true }).click()
  const sidebar = await fileSidebar(page)
  await expect(sidebar.getByRole('button', { name: '独立测试应用', exact: true })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('侧边栏应用区块.png'), fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  const mobileSidebar = await fileSidebar(page)
  await expect(mobileSidebar.getByRole('button', { name: '应用中心', exact: true })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('手机侧边栏应用区块.png'), fullPage: true, animations: 'disabled' })
  await mobileSidebar.getByRole('button', { name: '独立测试应用', exact: true }).click()
  await expect(page.getByRole('dialog', { name: '导航', exact: true })).not.toBeVisible()
  await expect(page.frameLocator('iframe').locator('#note')).toHaveText('保留的配置')
  await page.getByRole('button', { name: '应用中心', exact: true }).click()
  await expect(page.getByRole('tab', { name: /已安装/ })).toHaveAttribute('aria-selected', 'true')
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('应用中心手机.png'), fullPage: true })
  await uninstallApp(page, false)
  await (await importApp(page, first)).getByRole('button', { name: '同意并安装' }).click()
  await expect(card).toContainText('v1.0.0')
  await card.getByRole('button', { name: '设置', exact: true }).click()
  await expect(settings.locator('#app-setting-note')).toHaveValue('保留的配置')
  await settings.getByRole('button', { name: '取消', exact: true }).click()
  await uninstallApp(page, true)
  await (await importApp(page, first)).getByRole('button', { name: '同意并安装' }).click()
  await card.getByRole('button', { name: '设置', exact: true }).click()
  await expect(settings.locator('#app-setting-note')).toHaveValue('初始值')
  await settings.getByRole('button', { name: '取消', exact: true }).click()
  const corrupt = `${temporary}/broken.tgapp`
  await writeFile(corrupt, (await readFile(first)).subarray(0, 20))
  await page.locator('input[type="file"][accept=".tgapp,.zip"]').setInputFiles(corrupt)
  await expect(page.getByRole('alert').filter({ hasText: '不是有效的 ZIP' })).toBeVisible()
  await expect(card).toContainText('已启用')
  await oldPage.close()
})
