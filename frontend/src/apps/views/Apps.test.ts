import { flushPromises, shallowMount } from '@vue/test-utils'
import ElementPlus, { ElMessage } from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppIndex, AppManifest, AppPreview, CatalogApp, InstalledApp } from '../types'
import Apps from './Apps.vue'

const mocks = vi.hoisted(() => ({
  list: vi.fn(), refreshCatalog: vi.fn(), installCatalog: vi.fn(), install: vi.fn(),
  inspect: vi.fn(), inspectCatalog: vi.fn(), state: vi.fn(), uninstall: vi.fn(), push: vi.fn(),
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
  mocks.install.mockResolvedValue({ data: installed })
  mocks.inspectCatalog.mockImplementation(async (id: string, digest: string) => {
    const app = snapshot.available.find((item) => item.manifest.id === id && item.digest === digest)!
    const existing = snapshot.installed.find((item) => item.manifest.id === id) ?? null
    return { data: structuredClone({ manifest: app.manifest, digest, existing,
      added_permissions: app.manifest.permissions.filter((permission) => !existing?.manifest.permissions.includes(permission)),
      replacements: [], replacement_revision: null,
    } satisfies AppPreview) }
  })
})
afterEach(() => { wrapper?.unmount(); ElMessage.closeAll() })

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
    expect(mocks.inspectCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest)
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest, undefined, false, null)
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
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest, installed.revision, false, null)
    expect(view.find('[role="dialog"]').exists()).toBe(true)
    expect(alertTitles()).toContain('插件包已经变化')
    expect(view.find('[data-app-id="sample"]').text()).toContain('v1.0.0')
  })
})

describe('服务端安装预览与旧应用替换', () => {
  const replacements: AppPreview['replacements'] = [
    { id: 'books', name: '图书', installed: true, has_data: true },
    { id: 'comics', name: '漫画', installed: false, has_data: true },
  ]
  function readerPreview(existing: InstalledApp | null = null): AppPreview {
    return {
      manifest: { ...manifest, id: 'reader', name: '阅读馆', version: '1.0.0', min_host_version: '0.3.3' },
      digest: 'f'.repeat(64), existing, added_permissions: [], replacements: structuredClone(replacements),
      replacement_revision: 'opaque-server-preview-token',
    }
  }
  async function importFile(file: File) {
    const input = wrapper!.find<HTMLInputElement>('input[type="file"]')
    Object.defineProperty(input.element, 'files', { value: [file], configurable: true })
    await input.trigger('change')
    await flushPromises()
  }

  it('目录先请求服务端预览，等待时不可安装；列明已安装和卸载残留数据，显式确认才替换', async () => {
    snapshot.installed = []
    const info = readerPreview()
    // 服务端返回值优先于目录中的名称、摘要与本地安装列表。
    let finish!: (value: { data: AppPreview }) => void
    mocks.inspectCatalog.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }))
    const view = await render()
    await showAvailable()
    await view.find('.tile-launch').trigger('click')
    expect(mocks.inspectCatalog).toHaveBeenCalledExactlyOnceWith('sample', available.digest)
    expect(view.find('[role="dialog"]').exists()).toBe(false)
    await view.find('.tile-launch').trigger('click')
    expect(mocks.inspectCatalog).toHaveBeenCalledTimes(1)
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    finish({ data: info })
    await flushPromises()
    const dialog = view.find('[role="dialog"]')
    expect(dialog.text()).toContain('阅读馆')
    expect(dialog.text()).toContain('图书（books）：已安装，将卸载')
    expect(dialog.text()).toContain('漫画（comics）：已卸载，仍有残留数据')
    for (const text of ['阅读进度', '书签', '偏好', '书库配置', '恢复快照', '不会迁移', '网盘原文件和网盘收藏继续保留']) {
      expect(dialog.text()).toContain(text)
    }
    expect(dialog.text()).not.toContain('同意并安装')
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    await button('安装并替换旧应用')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('reader', info.digest, undefined, true, info.replacement_revision)
    expect(view.find('[role="dialog"]').exists()).toBe(false)
    expect(mocks.list).toHaveBeenCalledTimes(2)
  })

  it('取消不安装或清理；再次预览失败不会重用旧替换信息', async () => {
    snapshot.installed = []
    mocks.inspectCatalog.mockResolvedValueOnce({ data: readerPreview() })
    const view = await render()
    await showAvailable()
    await button('安装')
    await button('取消')
    expect(view.find('[role="dialog"]').exists()).toBe(false)
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    expect(mocks.uninstall).not.toHaveBeenCalled()
    const message = vi.spyOn(ElMessage, 'error')
    mocks.inspectCatalog.mockRejectedValueOnce(new Error('目录已变化，请刷新后重试'))
    await button('安装')
    expect(message).toHaveBeenCalledWith('目录已变化，请刷新后重试')
    expect(view.find('[role="dialog"]').exists()).toBe(false)
    expect(mocks.installCatalog).not.toHaveBeenCalled()
  })

  it('已安装但没有私有数据的旧应用也必须明确替换', async () => {
    snapshot.installed = []
    const info = readerPreview()
    info.replacements = [{ id: 'books', name: '图书', installed: true, has_data: false }]
    mocks.inspectCatalog.mockResolvedValueOnce({ data: info })
    const view = await render()
    await showAvailable()
    await button('安装')
    expect(view.find('[role="dialog"]').text()).toContain('无私有数据')
    await button('安装并替换旧应用')
    expect(mocks.installCatalog).toHaveBeenLastCalledWith('reader', info.digest, undefined, true, info.replacement_revision)
  })

  it('无替换项的目录预览也透传原指纹，刷新列表不重建指纹或自动授权新出现的旧应用', async () => {
    snapshot.installed = []
    const info = readerPreview()
    info.replacements = []
    info.replacement_revision = 'server-empty-replacement-scope'
    mocks.inspectCatalog.mockResolvedValueOnce({ data: info })
    await render()
    await showAvailable()
    await button('安装')
    snapshot.installed.push({ ...structuredClone(installed), manifest: { ...manifest, id: 'books', name: '图书' } })
    await button('刷新')
    mocks.installCatalog.mockRejectedValueOnce(new Error('替换范围已变化，请重新预览'))
    await button('同意并安装')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('reader', info.digest, undefined, false, 'server-empty-replacement-scope')
    expect(alertTitles()).toContain('替换范围已变化，请重新预览')
    expect(mocks.inspectCatalog).toHaveBeenCalledTimes(1)
    expect(mocks.uninstall).not.toHaveBeenCalled()
  })

  it.each([false, true])('本地包复用检查响应，替换=%s 时仅确认对应按钮才安装原文件', async (replaceLegacy) => {
    const info = readerPreview()
    if (!replaceLegacy) info.replacements = []
    mocks.inspect.mockResolvedValueOnce({ data: info })
    const view = await render()
    const file = new File(['local-package'], 'reader.tgapp', { type: 'application/octet-stream' })
    await importFile(file)
    expect(mocks.inspect).toHaveBeenCalledExactlyOnceWith(file)
    expect(mocks.inspectCatalog).not.toHaveBeenCalled()
    expect(mocks.install).not.toHaveBeenCalled()
    expect(view.find('[role="dialog"]').text()).toContain('本地导入')
    expect(view.find<HTMLInputElement>('input[type="file"]').element.value).toBe('')
    await button(replaceLegacy ? '安装并替换旧应用' : '同意并安装')
    expect(mocks.install).toHaveBeenCalledExactlyOnceWith(file, info.digest, undefined, replaceLegacy, info.replacement_revision)
    expect(mocks.installCatalog).not.toHaveBeenCalled()
  })

  it('本地预览失败不显示确认框，也不发送安装请求', async () => {
    mocks.inspect.mockRejectedValueOnce(new Error('安装包无效'))
    const message = vi.spyOn(ElMessage, 'error')
    const view = await render()
    await importFile(new File(['broken'], 'reader.tgapp'))
    expect(message).toHaveBeenCalledWith('安装包无效')
    expect(view.find('[role="dialog"]').exists()).toBe(false)
    expect(mocks.install).not.toHaveBeenCalled()
  })

  it('替换失败保留预览和错误，安装过程中重复点击不会再提交', async () => {
    snapshot.installed = []
    mocks.inspectCatalog.mockResolvedValueOnce({ data: readerPreview() })
    let reject!: (error: Error) => void
    mocks.installCatalog.mockImplementationOnce(() => new Promise((_, fail) => { reject = fail }))
    const view = await render()
    await showAvailable()
    await button('安装')
    await button('安装并替换旧应用')
    await button('安装并替换旧应用')
    expect(mocks.installCatalog).toHaveBeenCalledTimes(1)
    reject(new Error('旧应用状态已变化，请重新预览'))
    await flushPromises()
    expect(alertTitles()).toContain('旧应用状态已变化')
    expect(view.find('[role="dialog"]').exists()).toBe(true)
    expect(mocks.uninstall).not.toHaveBeenCalled()
    expect(mocks.list).toHaveBeenCalledTimes(1)
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
    expect(mocks.inspectCatalog.mock.calls).toEqual([['sample', available.digest], ['demo', demoAvailable.digest]])
    expect(mocks.installCatalog).toHaveBeenNthCalledWith(1, 'sample', available.digest, installed.revision, false, null)
    expect(mocks.installCatalog).toHaveBeenNthCalledWith(2, 'demo', demoAvailable.digest, demoInstalled.revision, false, null)
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

  it('批量更新发现替换时跳过该项，其他应用正常更新且始终不授权清理', async () => {
    snapshot.installed.push(structuredClone(demoInstalled))
    snapshot.available.push(structuredClone(demoAvailable))
    mocks.inspectCatalog.mockResolvedValueOnce({ data: {
      manifest: available.manifest, digest: available.digest, existing: installed, added_permissions: [],
      replacements: [{ id: 'books', name: '图书', installed: false, has_data: true }],
    } satisfies AppPreview })
    await render()
    await button('一键更新（2）')
    expect(batchDialog().text()).toContain('请单独打开安装预览并确认清理范围')
    expect(mocks.installCatalog).not.toHaveBeenCalled()
    await button('全部更新（2）')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('demo', demoAvailable.digest, demoInstalled.revision, false, null)
    expect(mocks.uninstall).not.toHaveBeenCalled()
    expect(batchDialog().text()).toContain('已更新')
  })

  it('批量预览失败的条目不安装，并使用服务端返回的当前修订确认其余项目', async () => {
    snapshot.installed.push(structuredClone(demoInstalled))
    snapshot.available.push(structuredClone(demoAvailable))
    mocks.inspectCatalog.mockRejectedValueOnce(new Error('包摘要已失效'))
    mocks.inspectCatalog.mockResolvedValueOnce({ data: {
      manifest: demoAvailable.manifest, digest: demoAvailable.digest,
      existing: { ...demoInstalled, revision: 'server-revision' }, added_permissions: ['media.read'], replacements: [],
    } satisfies AppPreview })
    await render()
    await button('一键更新（2）')
    expect(batchDialog().text()).toContain('包摘要已失效')
    expect(batchDialog().text()).toContain('读取文件内容')
    await button('全部更新（2）')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('demo', demoAvailable.digest, 'server-revision', false, undefined)
  })

  it('阅读馆无残留的批量预览也回传指纹，范围变化后保留拒绝结果且不自动授权替换', async () => {
    const reader = { ...manifest, id: 'reader', name: '阅读馆' }
    snapshot.installed = [{ ...structuredClone(installed), manifest: reader }]
    snapshot.available = [{ ...structuredClone(available), manifest: { ...reader, version: '1.1.0' } }]
    mocks.inspectCatalog.mockResolvedValueOnce({ data: {
      manifest: snapshot.available[0]!.manifest, digest: available.digest, existing: snapshot.installed[0]!,
      added_permissions: [], replacements: [], replacement_revision: 'server-empty-batch-scope',
    } satisfies AppPreview })
    await render()
    await button('一键更新（1）')
    snapshot.installed.push({ ...structuredClone(installed), manifest: { ...manifest, id: 'comics', name: '漫画' } })
    await button('刷新')
    mocks.installCatalog.mockRejectedValueOnce(new Error('旧应用安装修订已变化，请重新预览'))
    await button('全部更新（1）')
    expect(mocks.installCatalog).toHaveBeenCalledExactlyOnceWith('reader', available.digest, installed.revision, false, 'server-empty-batch-scope')
    expect(batchDialog().text()).toContain('旧应用安装修订已变化，请重新预览')
    expect(mocks.inspectCatalog).toHaveBeenCalledTimes(1)
    expect(mocks.uninstall).not.toHaveBeenCalled()
  })
})
