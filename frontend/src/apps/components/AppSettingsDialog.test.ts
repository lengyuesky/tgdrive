import { flushPromises, shallowMount } from '@vue/test-utils'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppManifest, InstalledApp } from '../types'
import AppSettingsDialog from './AppSettingsDialog.vue'

const mocks = vi.hoisted(() => ({
  settings: vi.fn(), saveSettings: vi.fn(), saveScope: vi.fn(), coverStats: vi.fn(), clearCovers: vi.fn(),
  confirm: vi.fn(), success: vi.fn(), error: vi.fn(),
}))
vi.mock('@drive/api', () => ({
  api: { apps: mocks },
  errMsg: (error: Error) => error.message,
  fmtSize: (bytes: number) => `${bytes} B`,
}))
vi.mock('@drive/dialog', () => ({ appConfirm: mocks.confirm }))
vi.mock('element-plus', async (original) => {
  const actual = await original<typeof import('element-plus')>()
  return { ...actual, ElMessage: { success: mocks.success, error: mocks.error } }
})

const manifest: AppManifest = {
  id: 'books', name: '图书', version: '1.0.0', api_version: 2, min_host_version: '0.2.0',
  description: '合成清单', author: '测试作者', entry: 'index.html', permissions: ['files.read', 'media.read'], settings: [],
}
const app: InstalledApp = {
  manifest, digest: 'a'.repeat(64), revision: 'r1', enabled: true, pinned: false,
  source: 'catalog', installed_at: 1, updated_at: 1, scope: [],
}
let wrapper: ReturnType<typeof shallowMount> | undefined

beforeEach(() => {
  vi.resetAllMocks()
  mocks.settings.mockResolvedValue({ data: {} })
  mocks.coverStats.mockResolvedValue({ data: { entries: 3, bytes: 3000, limit_bytes: 268435456, limit_entries: 50000 } })
  mocks.clearCovers.mockResolvedValue({ data: { ok: true, removed: 3 } })
  mocks.confirm.mockResolvedValue(undefined)
})
afterEach(() => { wrapper?.unmount() })

async function render(target: InstalledApp) {
  wrapper = shallowMount(AppSettingsDialog, {
    props: { modelValue: true, app: target },
    global: {
      plugins: [ElementPlus], renderStubDefaultSlot: true,
      stubs: { ResponsivePanel: { template: '<section><slot /><slot name="footer" /></section>' }, DirPicker: true },
    },
  })
  await flushPromises()
  return wrapper
}

function clearButton() {
  return wrapper!.findAllComponents({ name: 'ElButton' }).find((item) => item.text().includes('清理封面缓存'))
}

describe('应用设置中的封面缓存', () => {
  it('显示占用并在确认后清理', async () => {
    await render(app)
    expect(mocks.coverStats).toHaveBeenCalledWith('books')
    expect(wrapper!.text()).toContain('已缓存 3 张 · 3000 B')
    mocks.coverStats.mockResolvedValue({ data: { entries: 0, bytes: 0, limit_bytes: 268435456, limit_entries: 50000 } })
    clearButton()!.vm.$emit('click')
    await flushPromises()
    expect(mocks.confirm).toHaveBeenCalledOnce()
    expect(mocks.clearCovers).toHaveBeenCalledWith('books')
    expect(mocks.success).toHaveBeenCalledWith('已清理 3 张封面')
    expect(wrapper!.text()).toContain('已缓存 0 张')
    expect(clearButton()!.props('disabled')).toBe(true)
  })

  it('取消确认时不清理', async () => {
    mocks.confirm.mockRejectedValue('cancel')
    await render(app)
    clearButton()!.vm.$emit('click')
    await flushPromises()
    expect(mocks.clearCovers).not.toHaveBeenCalled()
  })

  it('不读媒体且没有缓存的应用不显示封面缓存', async () => {
    mocks.coverStats.mockResolvedValue({ data: { entries: 0, bytes: 0, limit_bytes: 268435456, limit_entries: 50000 } })
    await render({ ...app, manifest: { ...manifest, permissions: [] } })
    expect(clearButton()).toBeUndefined()
    expect(wrapper!.findComponent({ name: 'ElEmpty' }).exists()).toBe(true)
  })
})
