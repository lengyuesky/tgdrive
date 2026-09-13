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
  source: 'catalog', installed_at: 1, updated_at: 1,
}
const available: CatalogApp = {
  manifest: { ...manifest, version: '1.1.0', permissions: ['files.read'] }, digest: 'b'.repeat(64),
  size: 1024, source: 'github', update_available: true,
  release_url: 'https://github.com/lengyuesky/tgdrive-app/releases/tag/v1.0.0',
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
    await button('打开')
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
    const open = view.findAllComponents({ name: 'ElButton' }).find((item) => item.text() === '打开')!
    expect(open.props('disabled')).toBe(false)
    await button('打开')
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

  it('可安装卡片显示 GitHub、Release 页面与精确摘要下载链接', async () => {
    snapshot.installed = []
    const view = await render()
    await showAvailable()
    expect(view.text()).toContain('GitHub (lengyuesky/tgdrive-app)')
    const links = view.findAll('a')
    expect(links.find((item) => item.text() === 'Release 页面')!.attributes('href')).toBe(available.release_url)
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
