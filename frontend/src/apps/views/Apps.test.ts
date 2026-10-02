import { flushPromises, shallowMount } from '@vue/test-utils'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppIndex, AppManifest, CatalogApp, InstalledApp } from '../types'
import Apps from './Apps.vue'

const mocks = vi.hoisted(() => ({
  list: vi.fn(), refreshCatalog: vi.fn(), installCatalog: vi.fn(), install: vi.fn(),
  inspect: vi.fn(), state: vi.fn(), uninstall: vi.fn(), push: vi.fn(),
}))
vi.mock('vue-router', () => ({ useRoute: () => ({ query: {} }), useRouter: () => ({ push: mocks.push }) }))
vi.mock('@drive/api', () => ({
  api: { apps: { ...mocks, packageUrl: (id: string, digest: string) => `/api/apps/catalog/${id}/package?digest=${digest}` } },
  errMsg: (error: Error) => error.message,
}))

const manifest: AppManifest = {
  id: 'sample', name: '测试应用', version: '1.0.0', api_version: 2, min_host_version: '0.1.0',
  description: '合成清单', author: '测试作者', entry: 'index.html', permissions: [], settings: [],
}
const installed: InstalledApp = {
  manifest, digest: 'a'.repeat(64), revision: 'reviewed-revision', enabled: true, pinned: false,
  source: 'catalog', installed_at: 1, updated_at: 1, scope: [],
}
const available: CatalogApp = {
  manifest: { ...manifest, version: '1.1.0', permissions: ['files.read'] }, digest: 'b'.repeat(64),
  size: 1024, source: 'github', update_available: true,
  source_url: 'https://github.com/lengyuesky/tgdrive-app/tree/main/apps',
}
let snapshot: AppIndex
let wrapper: ReturnType<typeof shallowMount> | undefined

beforeEach(() => {
  vi.resetAllMocks()
  snapshot = {
    installed: [structuredClone(installed)], available: [structuredClone(available)],
    api_version: 2, host_version: '0.1.0', supported_api_versions: [1, 2],
    catalog: { source: 'github', repository: 'lengyuesky/tgdrive-app', state: 'ready', last_success_at: 1, error: null },
  }
  mocks.list.mockImplementation(async () => ({ data: structuredClone(snapshot) }))
  mocks.refreshCatalog.mockImplementation(async () => ({ data: structuredClone(snapshot) }))
  mocks.installCatalog.mockResolvedValue({ data: installed })
})
afterEach(() => { wrapper?.unmount() })

async function render() {
  wrapper = shallowMount(Apps, { global: {
    plugins: [ElementPlus], renderStubDefaultSlot: true,
    stubs: {
      ResponsivePanel: {
        props: ['modelValue', 'title'],
        template: '<section v-if="modelValue" role="dialog" :aria-label="title"><slot /><slot name="footer" /></section>',
      },
      AppSettingsDialog: true,
    },
  } })
  await flushPromises()
  return wrapper
}

async function button(label: string) {
  const target = wrapper!.findAllComponents({ name: 'ElButton' }).find((item) => item.text().trim() === label)
  expect(target, `应显示按钮：${label}`).toBeDefined()
  target!.vm.$emit('click')
  await flushPromises()
}

async function showAvailable() {
  await wrapper!.findAll('[role="tab"]')[1]!.trigger('click')
  await flushPromises()
}

async function showSettings() {
  await wrapper!.findAll('[role="tab"]')[2]!.trigger('click')
  await flushPromises()
}

function emptyText() { return wrapper!.findComponent({ name: 'ElEmpty' }).props('description') }
function alertTitles() { return wrapper!.findAllComponents({ name: 'ElAlert' }).map((item) => item.props('title')).join(' ') }

describe('应用中心远程目录', () => {
  it('ready 快照也普通刷新，联网未结束时已安装应用仍可打开', async () => {
    let finish!: (value: { data: AppIndex }) => void
    mocks.refreshCatalog.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }))
    const view = await render()
    expect(mocks.refreshCatalog).toHaveBeenCalledExactlyOnceWith(false)
    expect(view.find('[role="tab"]').attributes('aria-selected')).toBe('true')
    expect(view.find('[data-app-id="sample"]').exists()).toBe(true)
    await view.find('[data-app-id="sample"] .tile-launch').trigger('click')
    expect(mocks.push).toHaveBeenCalledWith({ path: '/apps/sample', query: { from: '/' } })
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    snapshot.catalog!.state = 'stale'
    snapshot.catalog!.error = '模拟离线'
    finish({ data: structuredClone(snapshot) })
    await flushPromises()
    await showAvailable()
    expect(alertTitles()).toContain('模拟离线')
    await button('刷新')
    expect(mocks.refreshCatalog).toHaveBeenLastCalledWith(true)
    expect(view.findAll('[role="tab"]')[1]!.attributes('aria-selected')).toBe('true')
  })

  it('手动目录刷新期间不将已有安装列表置为阻塞加载状态', async () => {
    const view = await render()
    let finish!: (value: { data: AppIndex }) => void
    mocks.refreshCatalog.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }))
    await button('刷新')
    expect(view.find('[data-app-id="sample"]').exists()).toBe(true)
    const launch = view.find('[data-app-id="sample"] .tile-launch')
    expect(launch.attributes('disabled')).toBeUndefined()
    await launch.trigger('click')
    expect(mocks.push).toHaveBeenCalledTimes(1)
    finish({ data: structuredClone(snapshot) })
    await flushPromises()
  })

  it.each(['local', 'disabled'] as const)('打开 %s 目录时不尝试后台 GitHub 请求', async (source) => {
    snapshot.catalog!.source = source
    snapshot.catalog!.state = source === 'disabled' ? 'disabled' : 'ready'
    await render()
    expect(mocks.refreshCatalog).not.toHaveBeenCalled()
  })

  it('无目录或无兼容条目不误报为全部已安装，禁用与联网失败有各自提示', async () => {
    snapshot.installed = []
    snapshot.available = []
    await render()
    expect(emptyText()).toBe('还没有安装应用')
    await showAvailable()
    expect(emptyText()).toBe('目录为空或没有兼容当前主程序的应用')
    snapshot.catalog!.state = 'disabled'
    snapshot.catalog!.source = 'disabled'
    await button('刷新')
    expect(emptyText()).toBe('应用目录已禁用')
    expect(alertTitles()).toContain('手动安装')
    snapshot.catalog!.state = 'unavailable'
    snapshot.catalog!.source = 'github'
    snapshot.catalog!.error = 'GitHub 下载返回错误状态码：403'
    await button('刷新')
    expect(emptyText()).toBe('应用目录暂不可用')
    expect(alertTitles()).toContain('403')
    await button('重试连接')
    expect(mocks.refreshCatalog).toHaveBeenLastCalledWith(true)
  })

  it('可安装卡片显示 GitHub、来源仓库与精确摘要下载链接', async () => {
    snapshot.installed = []
    const view = await render()
    await showAvailable()
    expect(view.text()).toContain('GitHub (lengyuesky/tgdrive-app)')
    const links = view.findAll('a')
    expect(links.find((item) => item.text() === '来源仓库')!.attributes('href')).toBe(available.source_url)
    expect(links.find((item) => item.text() === '下载插件包')!.attributes('href')).toContain(`digest=${available.digest}`)
    await button('安装')
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    expect(view.find('[role="dialog"]').text()).toContain('搜索文件与读取文件信息')
    await button('同意并安装')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest, undefined)
    expect(mocks.state).not.toHaveBeenCalled()
  })

  it('升级权限经过明确确认，确认后的摘要不会被刷新目录悄悄替换', async () => {
    const view = await render()
    await showSettings()
    await button('更新')
    const preview = view.find('[role="dialog"]')
    expect(preview.text()).toContain('新增')
    expect(preview.text()).toContain('搜索文件与读取文件信息')
    snapshot.available = [{ ...structuredClone(available), digest: 'c'.repeat(64), manifest: { ...manifest, version: '1.2.0', permissions: ['media.read'] } }]
    await button('刷新')
    expect(preview.text()).toContain('v1.1.0')
    expect(preview.text()).not.toContain('v1.2.0')
    mocks.installCatalog.mockRejectedValueOnce(new Error('插件包已经变化，请重新查看安装信息'))
    await button('同意并更新')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest, installed.revision)
    expect(view.find('[role="dialog"]').exists()).toBe(true)
    expect(alertTitles()).toContain('插件包已经变化')
    expect(view.find('[data-app-id="sample"]').text()).toContain('v1.0.0')
  })
})

describe('桌面式图标卡片', () => {
  it('已安装卡片保留图标、名称和详情入口，更新红点保留', async () => {
    const view = await render()
    const tile = view.find('[data-app-id="sample"]')
    expect(tile.find('.tile-launch').exists()).toBe(true)
    expect(tile.find('.tile-badge-dot').exists()).toBe(true)
    expect(tile.text()).toContain('测试应用')
    expect(tile.text()).not.toContain('v1.0.0')
    expect(tile.text()).not.toContain('已启用')
    expect(tile.text()).not.toContain('应用目录')
    expect(tile.findComponent({ name: 'ElButton' }).text()).toBe('应用详情')
    expect(tile.findComponent({ name: 'ElDropdown' }).exists()).toBe(false)
  })

  it('停用的应用卡片置灰且点击禁用', async () => {
    snapshot.installed[0]!.enabled = false
    const view = await render()
    const tile = view.find('[data-app-id="sample"]')
    expect(tile.classes()).toContain('off')
    const launch = tile.find('.tile-launch')
    expect(launch.attributes('disabled')).toBeDefined()
    await launch.trigger('click')
    expect(mocks.push).not.toHaveBeenCalled()
  })
})

describe('应用设置管理列表', () => {
  it('标签徽标计数待更新应用，列表集中更新、启停、固定与卸载', async () => {
    snapshot.installed[0]!.manifest.settings = [
      { key: 'note', label: '说明文字', description: '', type: 'string', default: '初始值' },
    ]
    const view = await render()
    const tabs = view.findAll('[role="tab"]')
    expect(tabs[2]!.text()).toContain('应用设置')
    expect(tabs[2]!.text()).toContain('1')
    await showSettings()
    const row = view.find('[data-app-id="sample"]')
    expect(row.text()).toContain('测试应用')
    expect(row.text()).toContain('v1.0.0')
    expect(row.text()).toContain('已启用')
    expect(row.text()).toContain('有新版本 v1.1.0')
    await button('更新')
    expect(view.find('[role="dialog"]').attributes('aria-label')).toBe('更新应用')
    await button('取消')
    view.findComponent({ name: 'ElDropdown' }).vm.$emit('command', 'enable')
    await flushPromises()
    expect(mocks.state).toHaveBeenCalledExactlyOnceWith('sample', { enabled: false })
    view.findComponent({ name: 'ElDropdown' }).vm.$emit('command', 'pin')
    await flushPromises()
    expect(mocks.state).toHaveBeenLastCalledWith('sample', { pinned: true })
    view.findComponent({ name: 'ElDropdown' }).vm.$emit('command', 'uninstall')
    await flushPromises()
    expect(view.find('[role="dialog"][aria-label="卸载应用"]').exists()).toBe(true)
    await button('卸载')
    expect(mocks.uninstall).toHaveBeenCalledExactlyOnceWith('sample', false)
  })

  it('无设置字段且无目录范围的应用不显示设置按钮，含 files.read 权限即可进入设置', async () => {
    const view = await render()
    await showSettings()
    expect(view.findAllComponents({ name: 'ElButton' }).some((item) => item.text().trim() === '设置')).toBe(false)
    snapshot.installed[0]!.manifest.permissions = ['files.read']
    await button('刷新')
    await showSettings()
    await button('设置')
    const dialog = view.findComponent({ name: 'AppSettingsDialog' })
    expect(dialog.props('modelValue')).toBe(true)
    expect(dialog.props('app').manifest.id).toBe('sample')
  })

  it('无安装应用时空状态引导浏览可安装', async () => {
    snapshot.installed = []
    const view = await render()
    await showSettings()
    expect(emptyText()).toBe('还没有安装应用')
    await button('浏览可安装应用')
    expect(view.findAll('[role="tab"]')[1]!.attributes('aria-selected')).toBe('true')
  })
})

describe('一键更新', () => {
  const demoInstalled: InstalledApp = {
    manifest: { ...manifest, id: 'demo', name: '演示应用', version: '2.0.0' },
    digest: 'd'.repeat(64), revision: 'demo-revision', enabled: true, pinned: false,
    source: 'import', installed_at: 2, updated_at: 2, scope: [],
  }
  const demoAvailable: CatalogApp = {
    manifest: { ...manifest, id: 'demo', name: '演示应用', version: '2.1.0', permissions: ['files.read', 'favorites.write'] },
    digest: 'e'.repeat(64), size: 2048, source: 'github', update_available: true,
  }

  function batchDialog() {
    return wrapper!.find('[role="dialog"][aria-label="一键更新"]')
  }

  it('无更新时不显示一键更新入口', async () => {
    snapshot.available[0]!.update_available = false
    const view = await render()
    expect(view.text()).not.toContain('一键更新')
  })

  it('单次确认后逐个安装并自动关闭，正确携带摘要与旧修订', async () => {
    snapshot.installed.push(structuredClone(demoInstalled))
    snapshot.available.push(structuredClone(demoAvailable))
    const view = await render()
    await button('一键更新（2）')
    const dialog = batchDialog()
    expect(dialog.exists()).toBe(true)
    expect(dialog.text()).toContain('测试应用')
    expect(dialog.text()).toContain('v1.0.0 → v1.1.0')
    expect(dialog.text()).toContain('演示应用')
    expect(dialog.text()).toContain('v2.0.0 → v2.1.0')
    // 新增权限在弹窗中逐项展示。
    expect(dialog.text()).toContain('搜索文件与读取文件信息')
    expect(dialog.text()).toContain('添加或取消收藏')
    const lists = mocks.list.mock.calls.length
    await button('全部更新（2）')
    expect(mocks.installCatalog.mock.calls.map((call) => call[0])).toEqual(['sample', 'demo'])
    expect(mocks.installCatalog).toHaveBeenNthCalledWith(1, 'sample', available.digest, installed.revision)
    expect(mocks.installCatalog).toHaveBeenNthCalledWith(2, 'demo', demoAvailable.digest, demoInstalled.revision)
    // 列表刷新且弹窗自动关闭。
    expect(mocks.list.mock.calls.length).toBeGreaterThan(lists)
    expect(batchDialog().exists()).toBe(false)
  })

  it('单个应用失败不影响其余：保留清单与原因，完成后可关闭', async () => {
    snapshot.installed.push(structuredClone(demoInstalled))
    snapshot.available.push(structuredClone(demoAvailable))
    await render()
    await button('一键更新（2）')
    mocks.installCatalog.mockRejectedValueOnce(new Error('插件包已经变化，请重新查看安装信息'))
    await button('全部更新（2）')
    const dialog = batchDialog()
    expect(dialog.exists()).toBe(true)
    expect(dialog.text()).toContain('更新失败：插件包已经变化，请重新查看安装信息')
    expect(dialog.text()).toContain('已更新')
    // 失败后不再重复调用同一应用。
    expect(mocks.installCatalog).toHaveBeenCalledTimes(2)
    await button('完成')
    expect(batchDialog().exists()).toBe(false)
  })
})
