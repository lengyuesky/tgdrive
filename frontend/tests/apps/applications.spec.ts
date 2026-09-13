import { expect, test, type Page } from '@playwright/test'
import { mkdtemp, rm, writeFile, readFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { crc32 } from 'node:zlib'

let temporary: string

test.beforeAll(async () => {
  temporary = await mkdtemp(`${tmpdir()}/tgdrive-app-package-test-`)
})
test.afterAll(async () => {
  await rm(temporary, { recursive: true, force: true })
})

test.beforeEach(async ({ page }) => {
  // 只有确认独立夹具身份后才执行安装、卸载等写操作。
  expect(await (await page.request.get('/__apps_fixture')).json()).toEqual({
    fixture: 'tgdrive-apps-tests',
  })
  expect(
    (await page.request.post('/__apps_fixture/media', { data: { enabled: false } })).ok()
  ).toBe(true)
  expect(
    (
      await page.request.post('/api/auth/login', {
        data: { username: 'apps-test', password: 'apps-test-only' },
      })
    ).ok()
  ).toBe(true)
  const index = await (await page.request.get('/api/apps')).json()
  for (const app of index.installed) {
    expect(
      (await page.request.delete(`/api/apps/${app.manifest.id}?purge_data=true`)).ok()
    ).toBe(true)
  }
})

function makeZip(files: Record<string, string | Uint8Array>): Buffer {
  const entries: Array<{ nameBuf: Buffer; crc: number; size: number; offset: number }> = []
  let offset = 0
  const localChunks: Buffer[] = []
  for (const [name, content] of Object.entries(files)) {
    const data = typeof content === 'string' ? Buffer.from(content, 'utf-8') : Buffer.from(content)
    const nameBuf = Buffer.from(name, 'utf-8')
    const crc = crc32(data)
    const size = data.length
    const header = Buffer.alloc(30)
    header.writeUInt32LE(0x04034b50, 0)
    header.writeUInt16LE(20, 4)
    header.writeUInt16LE(0, 6)
    header.writeUInt16LE(0, 8)
    header.writeUInt16LE(0, 10)
    header.writeUInt16LE(0, 12)
    header.writeUInt32LE(crc, 14)
    header.writeUInt32LE(size, 18)
    header.writeUInt32LE(size, 22)
    header.writeUInt16LE(nameBuf.length, 26)
    header.writeUInt16LE(0, 28)
    localChunks.push(header, nameBuf, data)
    entries.push({ nameBuf, crc, size, offset })
    offset += header.length + nameBuf.length + data.length
  }
  const cdChunks: Buffer[] = []
  let cdOffset = offset
  let cdSize = 0
  for (const entry of entries) {
    const cd = Buffer.alloc(46)
    cd.writeUInt32LE(0x02014b50, 0)
    cd.writeUInt16LE(0x031e, 4)
    cd.writeUInt16LE(20, 6)
    cd.writeUInt16LE(0, 8)
    cd.writeUInt16LE(0, 10)
    cd.writeUInt16LE(0, 12)
    cd.writeUInt32LE(entry.crc, 16)
    cd.writeUInt32LE(entry.size, 20)
    cd.writeUInt32LE(entry.size, 24)
    cd.writeUInt16LE(entry.nameBuf.length, 28)
    cd.writeUInt16LE(0, 30)
    cd.writeUInt16LE(0, 32)
    cd.writeUInt16LE(0, 34)
    cd.writeUInt16LE(0, 36)
    cd.writeUInt32LE((0o100644 << 16) >>> 0, 38)
    cd.writeUInt32LE(entry.offset, 42)
    cdChunks.push(cd, entry.nameBuf)
    cdSize += cd.length + entry.nameBuf.length
  }
  const eocd = Buffer.alloc(22)
  eocd.writeUInt32LE(0x06054b50, 0)
  eocd.writeUInt16LE(0, 4)
  eocd.writeUInt16LE(0, 6)
  eocd.writeUInt16LE(entries.length, 8)
  eocd.writeUInt16LE(entries.length, 10)
  eocd.writeUInt32LE(cdSize, 12)
  eocd.writeUInt32LE(cdOffset, 16)
  eocd.writeUInt16LE(0, 20)
  return Buffer.concat([...localChunks, ...cdChunks, eocd])
}

async function packagePath(version: string, permissions: string[]) {
  const jsContent = `
    let port = null;
    let reqId = 0;
    const pending = new Map();
    function invoke(method, params = {}) {
      return new Promise((resolve, reject) => {
        const id = ++reqId;
        pending.set(id, { resolve, reject });
        port.postMessage({ type: 'request', id, method, params });
      });
    }
    window.addEventListener('message', async (e) => {
      if (e.data?.type === 'connect' && e.ports?.[0]) {
        clearInterval(timer);
        port = e.ports[0];
        port.onmessage = (msg) => {
          const res = msg.data;
          if (res?.type === 'response' && pending.has(res.id)) {
            const { resolve, reject } = pending.get(res.id);
            pending.delete(res.id);
            if (res.error) reject(new Error(res.error));
            else resolve(res.result);
          }
        };
        const ctx = e.data.context;
        document.getElementById('version').textContent = '独立运行 ' + ctx.version;
        try {
          const s = await invoke('settings.get');
          document.getElementById('note').textContent = s.note || '';
        } catch (err) {
          document.getElementById('note').textContent = err.message;
        }
        try {
          await invoke('files.search', {});
          document.getElementById('permission').textContent = '允许读取';
        } catch (err) {
          document.getElementById('permission').textContent = err.message;
        }
      }
    });
    const announce = () => parent.postMessage({ channel: 'tgdrive-app-v1', type: 'ready' }, '*');
    const timer = setInterval(announce, 300);
    announce();
  `
  const zip = makeZip({
    'app.json': JSON.stringify({
      id: 'sample-viewer',
      name: '独立测试应用',
      version,
      api_version: 1,
      min_host_version: '0.1.0',
      description: '验证独立导入和版本更新。',
      author: '测试夹具',
      entry: 'index.html',
      permissions,
      settings: [
        {
          key: 'note',
          label: '说明文字',
          description: '更新后保留。',
          type: 'string',
          default: '初始值',
        },
      ],
    }),
    'index.html':
      '<!doctype html><html lang="zh-CN"><head><meta charset="UTF-8"><script defer src="./app.js"></script></head><body><h1 id="version"></h1><p id="note"></p><p id="permission"></p></body></html>',
    'app.js': jsContent,
  })
  const dest = `${temporary}/sample-viewer-${version}.tgapp`
  await writeFile(dest, zip)
  return dest
}

async function fileSidebar(page: Page) {
  const width = page.viewportSize()!.width
  if (width < 1024) {
    const toggle = page.getByRole('button', {
      name: width <= 767 ? '打开目录导航' : '打开侧边栏',
      exact: true,
    })
    await toggle.click()
    const drawer = page.getByRole('dialog', { name: '导航', exact: true })
    await expect(drawer).toBeVisible()
    return drawer.getByRole('complementary', { name: '目录侧边栏' })
  }
  const sidebar = page.getByRole('complementary', { name: '目录侧边栏' })
  await expect(sidebar).toBeVisible()
  return sidebar
}

async function importApp(page: Page, path: string, update = false) {
  await page.locator('input[type="file"][accept=".tgapp,.zip"]').setInputFiles(path)
  const dialog = page.getByRole('dialog', {
    name: update ? '更新应用' : '安装应用',
    exact: true,
  })
  await expect(dialog).toBeVisible()
  return dialog
}

async function uninstallApp(page: Page, purge: boolean) {
  await page
    .locator('[data-app-id="sample-viewer"]')
    .getByRole('button', { name: '更多应用操作' })
    .click()
  await page.getByRole('menuitem', { name: '卸载应用', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: '卸载应用', exact: true })
  if (purge) {
    await dialog
      .getByText('同时清除这个应用的数据（设置、阅读记录和书签）', { exact: true })
      .click()
    await expect(
      dialog.getByRole('checkbox', {
        name: '同时清除这个应用的数据（设置、阅读记录和书签）',
      })
    ).toBeChecked()
  }
  await dialog.getByRole('button', { name: '卸载', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  await expect(page.locator('[data-app-id="sample-viewer"]')).toHaveCount(0)
}

test('应用中心默认已安装，空状态不自动跳转，列表刷新保留选择', async ({ page }) => {
  await page.goto('/apps')
  const installed = page.getByRole('tab', { name: /已安装/ })
  const available = page.getByRole('tab', { name: /可安装/ })
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await expect(page.getByText('还没有安装应用', { exact: true })).toBeVisible()
  await expect(page.locator('[data-app-id]')).toHaveCount(0)
  await page.getByRole('button', { name: '浏览可安装应用' }).click()
  await expect(available).toHaveAttribute('aria-selected', 'true')
  await page.getByRole('button', { name: '刷新', exact: true }).click()
  await expect(page.locator('[data-app-id="books"]')).toBeVisible()
  await expect(available).toHaveAttribute('aria-selected', 'true')
  await page.reload()
  await expect(installed).toHaveAttribute('aria-selected', 'true')

  const pkg = await packagePath('1.0.0', [])
  await (await importApp(page, pkg)).getByRole('button', { name: '同意并安装' }).click()
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await page.getByPlaceholder('搜索应用').fill('不存在的应用')
  await expect(page.getByText('没有匹配的应用', { exact: true })).toBeVisible()
  await page.getByPlaceholder('搜索应用').fill('')
  await page.reload()
  await expect(installed).toHaveAttribute('aria-selected', 'true')
  await expect(page.locator('[data-app-id="sample-viewer"]')).toBeVisible()
})

test('独立插件导入、权限限制、更新撤销旧页面、停用、固定入口与保留配置卸载', async ({
  page,
}, testInfo) => {
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
  await expect(oldPage.frameLocator('iframe').locator('#version')).toHaveText(
    '独立运行 1.0.0'
  )
  await expect(oldPage.frameLocator('iframe').locator('#permission')).toContainText(
    '没有 files.read 权限'
  )

  const update = await importApp(page, second, true)
  await expect(update).toContainText('新增')
  await update.getByRole('button', { name: '同意并更新' }).click()
  await expect(update).not.toBeVisible()
  await expect(card).toContainText('v1.1.0')
  await expect(oldPage.locator('iframe')).toHaveCount(0)
  await expect(oldPage.locator('body')).toContainText('更新')

  await oldPage.getByRole('button', { name: '重新打开' }).click()
  await expect(oldPage.frameLocator('iframe').locator('#version')).toHaveText(
    '独立运行 1.1.0'
  )
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
  await expect(
    sidebar.getByRole('button', { name: '独立测试应用', exact: true })
  ).toBeVisible()
  await page.screenshot({
    path: testInfo.outputPath('侧边栏应用区块.png'),
    fullPage: true,
  })

  await page.setViewportSize({ width: 390, height: 844 })
  const mobileSidebar = await fileSidebar(page)
  await expect(
    mobileSidebar.getByRole('button', { name: '应用中心', exact: true })
  ).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
    true
  )
  await page.screenshot({
    path: testInfo.outputPath('手机侧边栏应用区块.png'),
    fullPage: true,
    animations: 'disabled',
  })

  await mobileSidebar
    .getByRole('button', { name: '独立测试应用', exact: true })
    .click()
  await expect(page.getByRole('dialog', { name: '导航', exact: true })).not.toBeVisible()
  await expect(page.frameLocator('iframe').locator('#note')).toHaveText('保留的配置')

  await page.getByRole('button', { name: '应用中心', exact: true }).click()
  await expect(page.getByRole('tab', { name: /已安装/ })).toHaveAttribute(
    'aria-selected',
    'true'
  )
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
    true
  )
  await page.screenshot({
    path: testInfo.outputPath('应用中心手机.png'),
    fullPage: true,
  })

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
