import { defineComponent, h } from 'vue'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DirectoryScope, FileNode, InstalledApp, ScopeInfo } from './contract'

const calls = vi.hoisted(() => ({ app: vi.fn(), node: vi.fn(), scope: vi.fn() }))
vi.mock('./client', () => ({ center: calls }))
import ScopeEditor from './ScopeEditor.vue'

const panel = defineComponent({ props: ['modelValue'], setup(props, { slots }) { return () => props.modelValue ? h('section', [slots.default?.(), slots.footer?.()]) : null } })
const tree = defineComponent({ setup() { return () => h('div', '文件夹列表') } })
let wrapper: VueWrapper<InstanceType<typeof ScopeEditor>> | undefined
const directory = (id: string, path: string): FileNode => ({ id, path, name: path.split('/').pop()!, parent_id: '1', kind: 'directory', size: '0', created_at: 1, modified_at: 1, revision: 'node-1', content_version: null, children_revision: 'children-1', has_thumbnail: false })
const movie = directory('2', '/电影'), series = directory('3', '/电视剧')
function app(scope: DirectoryScope = { mode: 'none' }): InstalledApp {
  return { manifest: { id: 'cinema', name: '影视', version: '1.0.0', contract: { major: 1, minor: 0 }, description: '测试', author: '测试', files: { read: true, write: true }, required_capabilities: [], open_with: { directories: false, extensions: [] } }, generation: 'g1', state_revision: 's1', enabled: true, pinned: false, backend: null, package: { target: 'web', size: '1', sha256: 'a'.repeat(64) }, installed_at: 1, updated_at: 1, scope: { revision: 'scope-1', scope, roots: scope.mode === 'selected' ? scope.directories.map(grant => ({ ...grant, node: grant.directory_id === movie.id ? movie : series })) : [] } }
}
function render() {
  wrapper = mount(ScopeEditor, { global: { plugins: [ElementPlus], stubs: { ResponsivePanel: panel, VirtualDirectoryTree: tree } } })
  return wrapper
}
async function click(label: string) {
  const button = wrapper!.findAll('button').find(button => button.text() === label)
  expect(button).toBeDefined(); await button!.trigger('click'); await flushPromises()
}
beforeEach(() => {
  vi.clearAllMocks()
  calls.node.mockImplementation(async ({ id }: { id: string }) => id === movie.id ? movie : series)
})
afterEach(() => { wrapper?.vm.close(); wrapper?.unmount(); wrapper = undefined })

describe('应用请求文件夹授权', () => {
  it('预填刚选择的文件夹，主人确认前不写入权限', async () => {
    const installed = app(), saved: ScopeInfo = { revision: 'scope-2', scope: { mode: 'selected', directories: [{ directory_id: movie.id, access: 'read_only' }] }, roots: [{ directory_id: movie.id, access: 'read_only', node: movie }] }
    calls.app.mockResolvedValue(installed); calls.scope.mockResolvedValue(saved)
    const view = render(), pending = view.vm.open(installed, '用于浏览和播放', 'read_only', movie)
    await flushPromises()
    expect(view.findAll('.grant-list li').map(row => row.text())).toEqual([expect.stringContaining('/电影')])
    expect(view.find('details').attributes('open')).toBeUndefined()
    expect(view.text()).not.toContain('不授权')
    expect(calls.scope).not.toHaveBeenCalled()
    await click('确认授权')
    expect(calls.scope).toHaveBeenCalledWith('cinema', { expected_revision: 'scope-1', scope: { mode: 'selected', directories: [{ directory_id: movie.id, access: 'read_only' }] } })
    expect(await pending).toEqual(saved)
  })
  it('新增文件夹保留已有授权，重复选择也不会降级已有读写', async () => {
    const installed = app({ mode: 'selected', directories: [{ directory_id: movie.id, access: 'read_write' }] })
    calls.app.mockResolvedValue(installed); calls.scope.mockResolvedValue(installed.scope)
    const view = render(), pending = view.vm.open(installed, '添加电视剧库', 'read_only', series)
    await flushPromises()
    expect(view.findAll('.grant-list li')).toHaveLength(2)
    await click('确认授权'); await pending
    expect(calls.scope).toHaveBeenLastCalledWith('cinema', { expected_revision: 'scope-1', scope: { mode: 'selected', directories: [{ directory_id: movie.id, access: 'read_write' }, { directory_id: series.id, access: 'read_only' }] } })
    const duplicate = view.vm.open(installed, '继续读取电影', 'read_only', movie)
    await flushPromises(); await click('确认授权'); await duplicate
    expect(calls.scope).toHaveBeenLastCalledWith('cinema', { expected_revision: 'scope-1', scope: { mode: 'selected', directories: [{ directory_id: movie.id, access: 'read_write' }] } })
  })
  it('取消请求保留原范围；没有刚选择的文件夹时直接显示选择列表', async () => {
    const installed = app(); calls.app.mockResolvedValue(installed)
    const view = render(), pending = view.vm.open(installed, '选择允许读取的文件夹')
    await flushPromises()
    expect(view.find('details').attributes('open')).toBeDefined()
    const confirm = view.findAll('button').find(button => button.text() === '确认授权')!
    expect(confirm.attributes('disabled')).toBeDefined()
    await click('取消')
    expect(await pending).toBeNull(); expect(calls.scope).not.toHaveBeenCalled(); expect(installed.scope.scope).toEqual({ mode: 'none' })
  })
  it('页面取消时，迟到的安装信息不会重新打开授权窗口', async () => {
    const installed = app(); let loaded: ((value: InstalledApp) => void) | undefined
    calls.app.mockImplementation(() => new Promise<InstalledApp>(resolve => { loaded = resolve }))
    const view = render(), pending = view.vm.open(installed, '选择允许读取的文件夹', 'read_only', movie)
    view.vm.close(); loaded?.(installed); await flushPromises()
    expect(await pending).toBeNull(); expect(view.find('section').exists()).toBe(false); expect(calls.scope).not.toHaveBeenCalled()
  })
})
