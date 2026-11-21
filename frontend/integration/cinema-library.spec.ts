import { expect, test, type Page } from '@playwright/test'
import { APP_CENTER_BASE, SELF_API_BASE, type FileNode, type InstalledApp, type LaunchPageResponse, type ScopeInfo } from '../src/apps/contract'

async function installed(page: Page): Promise<InstalledApp> {
  const response = await page.request.get(`${APP_CENTER_BASE}/apps/cinema`)
  expect(response.status()).toBe(200)
  return await response.json() as InstalledApp
}

async function backend(page: Page) {
  const origin = new URL(page.url()).origin
  const response = await page.request.post(`${APP_CENTER_BASE}/apps/cinema/launch`, { headers: { Origin: origin }, data: {} })
  expect(response.status()).toBe(200)
  const { bootstrap } = await response.json() as LaunchPageResponse
  expect(typeof bootstrap.session_token).toBe('string')
  expect(bootstrap.app.generation).toBe((await installed(page)).generation)
  const runtime = await page.request.get(`${SELF_API_BASE}/info`, { headers: { Authorization: `Bearer ${bootstrap.session_token}`, Origin: 'null' } })
  expect(runtime.status(), '新页面会话须通过运行信息核验').toBe(200)
  return {
    token: bootstrap.session_token,
    async request<T>(method: 'GET' | 'POST' | 'PUT', path: string, data?: unknown): Promise<T> {
      const response = await page.request.fetch(`${bootstrap.backend_base}${path}`, { method, data, headers: { Authorization: `Bearer ${bootstrap.session_token}`, Origin: 'null' } })
      expect(response.status(), path).toBe(200)
      return await response.json() as T
    },
  }
}

interface CinemaConfiguration { revision: string; running: boolean; username: string; libraries: Array<{ id: string; root_id: string; name: string }> }

async function upgrade(page: Page) {
  const api = await backend(page)
  const config = await api.request<CinemaConfiguration>('GET', '/owner/config')
  const library = config.libraries.find((library) => library.name === '珍藏电影')!
  const catalog = await api.request<{ items: FileNode[] }>('POST', '/owner/catalog', { ancestor: library.id, raw_files: true, limit: 48 })
  const file = catalog.items.find((file) => file.name === '合成影片.mp4')!
  expect(file).toBeDefined()
  const playback = await api.request<{ play_id: string }>('POST', '/owner/play/start', { id: file.id })
  await api.request('POST', '/owner/storage/set', { key: `progress:${file.id}`, value: { file, seconds: 1.5, duration: 4, completed: false, subtitle: '' }, revision: null, play_id: playback.play_id })
  await api.request('POST', '/owner/play/quit', playback)
  await api.request('POST', '/owner/storage/set', { key: `favorite:${file.id}`, value: { file }, revision: null })
  const flag = await api.request<{ revision: string }>('POST', '/owner/flags/get', { id: file.id })
  await api.request('POST', '/owner/flags/watched', { id: file.id, watched: true, revision: flag.revision })
  await api.request('POST', '/owner/storage/set', { key: 'preferences', value: { speed: 1.25, volume: 0.6 }, revision: null })
  const account = await api.request<CinemaConfiguration>('PUT', '/owner/account', { revision: config.revision, username: 'synthetic', password: 'Synthetic-Upgrade-Only' })
  const records = [`progress:${file.id}`, `favorite:${file.id}`, 'preferences']
  const beforeRecords = await Promise.all(records.map((key) => api.request('POST', '/owner/storage/get', { key })))
  const beforeFlag = await api.request('POST', '/owner/flags/get', { id: file.id })
  await api.request('POST', '/owner/service/start', { revision: account.revision })
  const checkpointResponse = await page.request.post(`${SELF_API_BASE}/files/changes`, { headers: { Authorization: `Bearer ${api.token}`, Origin: 'null' }, data: { cursor: null, limit: 500 } })
  expect(checkpointResponse.status()).toBe(200)
  const checkpoint = await checkpointResponse.json() as { next_cursor: string }

  await page.getByRole('button', { name: '关闭应用', exact: true }).click()
  await expect(page).toHaveURL(/\/admin\/apps$/)
  await page.getByRole('button', { name: '固定', exact: true }).click()
  await expect(page.getByRole('button', { name: '取消固定', exact: true })).toBeVisible()
  const before = await installed(page)
  await page.getByRole('button', { name: /^一键更新/ }).click()
  await expect(page.locator('.update-results')).toContainText('更新完成')
  await expect(page.getByRole('dialog', { name: '更新预览', exact: true })).not.toBeVisible()
  const after = await installed(page)
  expect(after.manifest.version).toBe(process.env.TGDRIVE_CINEMA_UPGRADE_VERSION)
  expect(after.generation).not.toBe(before.generation)
  expect(after.package.target).toBe(before.package.target)
  expect(after.enabled).toBe(before.enabled)
  expect(after.pinned).toBe(true)
  expect(after.scope.scope).toEqual(before.scope.scope)
  expect(after.scope.roots).toEqual(before.scope.roots)
  await page.getByRole('button', { name: '打开', exact: true }).click()
  const app = page.frameLocator('iframe')
  await expect(app.locator('#app')).toBeVisible()
  await expect(app.locator('.library-card')).toHaveCount(2)
  await expect(app.locator('.library-card')).toContainText(['珍藏电影', '珍藏电视剧'])
  const current = await backend(page)
  const stale = await page.request.post(`${SELF_API_BASE}/files/changes`, { headers: { Authorization: `Bearer ${current.token}`, Origin: 'null' }, data: { cursor: checkpoint.next_cursor, limit: 500 } })
  expect(stale.status()).toBe(409)
  expect((await stale.json()).error.code).toBe('app_changed')
  expect(await Promise.all(records.map((key) => current.request('POST', '/owner/storage/get', { key })))).toEqual(beforeRecords)
  expect(await current.request('POST', '/owner/flags/get', { id: file.id })).toEqual(beforeFlag)
  const updated = await current.request<CinemaConfiguration>('GET', '/owner/config')
  expect(updated.libraries).toEqual(config.libraries)
  expect(updated.username).toBe('synthetic')
  expect(updated.running).toBe(true)
}

test('真实影视包的媒体库、授权和升级生命周期', async ({ page }, testInfo) => {
  const outside: string[] = [], errors: string[] = [], granted: ScopeInfo[] = []
  const origin = new URL(testInfo.project.use.baseURL as string).origin
  page.on('request', request => { if (new URL(request.url()).origin !== origin) outside.push(request.url()) })
  page.on('pageerror', error => errors.push(error.message))
  page.on('response', response => {
    if (response.url().endsWith('/apps/cinema/scope') && response.request().method() === 'PUT' && response.ok()) void response.json().then(value => granted.push(value as ScopeInfo))
  })
  await page.goto('/admin/login')
  await page.locator('input[autocomplete="username"]').fill('test')
  await page.locator('input[autocomplete="current-password"]').fill('test-password')
  await page.getByRole('button', { name: '登录', exact: true }).click()
  await expect(page).toHaveURL(/\/admin\/files\//)
  expect((await installed(page)).scope.scope).toEqual({ mode: 'none' })
  if (process.env.TGDRIVE_CINEMA_INITIAL_VERSION) expect((await installed(page)).manifest.version).toBe(process.env.TGDRIVE_CINEMA_INITIAL_VERSION)
  await page.goto('/admin/apps/cinema')
  const app = page.frameLocator('iframe'), dialog = app.getByRole('dialog', { name: '新建媒体库', exact: true })
  await expect(app.locator('#app')).toBeVisible()
  await expect(page.locator('.app-toolbar')).toContainText('尚未授权文件夹')
  await app.locator('#new-library').click()
  await dialog.locator('#library-name').fill('稍后添加')
  await dialog.locator('#library-kind').selectOption('tv')
  await dialog.getByRole('button', { name: '选择文件夹', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  const picker = page.getByRole('dialog', { name: '选择文件夹', exact: true })
  await expect(picker).toBeVisible()
  await picker.getByText('电视剧', { exact: true }).click()
  await picker.getByRole('button', { name: '确定', exact: true }).click()
  const authorization = page.getByRole('dialog', { name: '目录授权', exact: true })
  await expect(authorization).toBeVisible()
  await expect(authorization.locator('.grant-list li')).toHaveCount(1)
  await expect(authorization.locator('.grant-list')).toContainText('/电视剧')
  await expect(authorization.getByRole('button', { name: '确认授权', exact: true })).toBeEnabled()
  await authorization.getByRole('button', { name: '取消', exact: true }).click()
  await expect(dialog).toBeVisible()
  await expect(dialog.locator('#library-name')).toHaveValue('稍后添加')
  await expect(dialog.locator('#library-kind')).toHaveValue('tv')
  await expect(dialog.locator('#library-edit-status')).toContainText('尚未授予')
  expect((await installed(page)).scope.scope).toEqual({ mode: 'none' })
  await dialog.getByRole('button', { name: '取消', exact: true }).click()

  for (const library of [{ name: '珍藏电影', directory: '电影', kind: 'movie' }, { name: '珍藏电视剧', directory: '电视剧', kind: 'tv' }]) {
    await app.locator('#new-library').click()
    await expect(dialog).toBeVisible()
    await dialog.locator('#library-name').fill(library.name)
    await dialog.locator('#library-kind').selectOption(library.kind)
    await dialog.getByRole('button', { name: '选择文件夹', exact: true }).click()
    await expect(picker).toBeVisible()
    await picker.getByText(library.directory, { exact: true }).click()
    await picker.getByRole('button', { name: '确定', exact: true }).click()
    await expect(authorization).toBeVisible()
    await expect(authorization.locator('.grant-list')).toContainText(`/${library.directory}`)
    await authorization.getByRole('button', { name: '确认授权', exact: true }).click()
    await expect(dialog).toBeVisible()
    await expect(dialog.locator('#library-path')).toContainText(`/${library.directory}`)
    await expect(dialog.locator('#library-name')).toHaveValue(library.name)
    await expect(dialog.locator('#library-kind')).toHaveValue(library.kind)
    await dialog.getByRole('button', { name: '保存媒体库', exact: true }).click()
    await expect(dialog).not.toBeVisible()
    await expect(app.locator('.library-card')).toContainText([library.name])
  }
  await expect(app.locator('.library-card')).toHaveCount(2)
  await expect(page.locator('.app-toolbar')).toContainText('已授权 2 个文件夹')
  const scope = (await installed(page)).scope
  expect(scope.scope.mode).toBe('selected')
  expect(scope.roots.map(root => [root.node?.path, root.access]).sort()).toEqual([['/电影', 'read_only'], ['/电视剧', 'read_only']].sort())
  expect(granted).toHaveLength(2)
  expect(granted.map(value => value.roots.length)).toEqual([1, 2])
  await page.getByRole('button', { name: '关闭应用', exact: true }).click()
  await expect(page).toHaveURL(/\/admin\/apps$/)
  await page.goto('/admin/apps/cinema')
  await expect(app.locator('#app')).toBeVisible()
  await expect(app.locator('.library-card')).toHaveCount(2)
  await expect(app.locator('.library-card')).toContainText(['珍藏电影', '珍藏电视剧'])
  if (process.env.TGDRIVE_CINEMA_UPGRADE_VERSION) await upgrade(page)
  await page.screenshot({ path: testInfo.outputPath('saved-libraries.png'), fullPage: true })
  expect(outside).toEqual([])
  expect(errors).toEqual([])
  expect(page.url()).not.toContain('session_token')
  expect(await page.locator('iframe').getAttribute('sandbox')).toBe('allow-scripts allow-downloads')
})
