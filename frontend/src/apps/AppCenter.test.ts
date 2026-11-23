import { defineComponent, h, nextTick } from 'vue'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CONTRACT_VERSION, type CatalogEntry, type CatalogResponse, type InstalledApp, type InstallPreview, type InstallSource, type PackageTarget } from './contract'

const calls = vi.hoisted(() => ({
  load: vi.fn(), catalog: vi.fn(), setRoot: vi.fn(), limits: vi.fn(), setLimits: vi.fn(), backend: vi.fn(), logs: vi.fn(), restart: vi.fn(), push: vi.fn(),
  preview: vi.fn(), install: vi.fn(),
}))
vi.mock('vue-router', async (importOriginal) => ({ ...await importOriginal<typeof import('vue-router')>(), useRouter: () => ({ push: calls.push }) }))
vi.mock('./client', async () => {
  const { shallowRef } = await import('vue')
  return {
    installedApps: shallowRef<InstalledApp[]>([]), rootSelection: shallowRef({ app_id: null as string | null, revision: 'root-1' }),
    loadInstalled: calls.load, iconUrl: () => undefined, formatBytes: (value: string) => `${value} B`, usage: vi.fn(),
    center: { catalog: calls.catalog, refresh: calls.catalog, setRoot: calls.setRoot, limits: calls.limits, setLimits: calls.setLimits, backend: calls.backend, logs: calls.logs, restart: calls.restart, preview: calls.preview, install: calls.install },
  }
})
import AppCenter from './AppCenter.vue'
import { installedApps, rootSelection } from './client'

const button = defineComponent({ inheritAttrs: false, setup(_, { attrs, slots }) { return () => h('button', attrs, slots.default?.()) } })
const select = defineComponent({ props: ['modelValue'], emits: ['change'], setup(props, { attrs, emit, slots }) { return () => h('select', { ...attrs, value: props.modelValue, onChange: (event: Event) => emit('change', (event.target as HTMLSelectElement).value) }, slots.default?.()) } })
const option = defineComponent({ props: ['label', 'value', 'disabled'], setup(props) { return () => h('option', { value: props.value, disabled: props.disabled }, props.label) } })
const panel = defineComponent({ props: ['modelValue', 'title'], setup(props, { slots }) { return () => props.modelValue ? h('section', [h('h2', props.title), slots.default?.(), slots.footer?.()]) : null } })
const basic = defineComponent({ setup(_, { slots }) { return () => h('div', slots.default?.()) } })
let wrapper: VueWrapper | undefined
const previews = new Map<string, InstallPreview>()
function app(id = 'example-backend', enabled = true): InstalledApp {
  return { manifest: { id, name: '测试服务', version: '1.0.0', description: '测试', contract: { major: 1, minor: 0 }, author: '测试', required_capabilities: [], open_with: { extensions: [], directories: false }, files: { read: true, write: true }, backend: { entry: 'backend/app', run_mode: 'resident', external_service: true } }, enabled, pinned: false, generation: 'g1', state_revision: 's1', package: { target: 'linux-arm64', sha256: 'a'.repeat(64), size: '1' }, installed_at: 1, updated_at: 1, scope: { revision: 'scope-1', scope: { mode: 'none' }, roots: [] }, backend: { state: 'ready', run_id: 'run-1', run_mode: 'resident', error_code: null } } as InstalledApp
}
function entry(id = 'example-backend', version = '1.0.1', targets: PackageTarget[] = ['linux-arm64', 'linux-amd64']): CatalogEntry {
  return { manifest: { ...app(id).manifest, version }, packages: Object.fromEntries(targets.map((target) => [target, { url: `https://github.com/test/apps/releases/download/${id}-v${version}/${id}-${version}-${target}.tgapp`, sha256: (target === 'linux-arm64' ? 'b' : 'c').repeat(64), size: '1024' }])) }
}
function ready(entries: CatalogEntry[]): CatalogResponse {
  return { status: { state: 'ready', repository: 'test/apps', last_success_at: 1, error: null }, result: { catalog: { format: CONTRACT_VERSION, repository: 'test/apps', entries }, incompatible: [] } }
}
async function render() {
  wrapper = mount(AppCenter, { global: { stubs: {
    ElButton: button, ElSelect: select, ElOption: option, ElIcon: basic, RouterLink: basic,
    ResponsivePanel: panel, ScopeEditor: basic, ElAlert: basic, ElCheckbox: basic,
  } } })
  await flushPromises(); await nextTick()
  return wrapper
}
async function click(label: string) { const target = wrapper!.findAll('button').find(button => button.text() === label); expect(target).toBeDefined(); await target!.trigger('click'); await flushPromises() }

beforeEach(() => {
  vi.useFakeTimers(); vi.clearAllMocks()
  previews.clear()
  installedApps.value = [app()]; rootSelection.value = { app_id: null, revision: 'root-1' }
  calls.load.mockImplementation(async () => installedApps.value)
  calls.catalog.mockResolvedValue({ status: { state: 'disabled' }, result: { catalog: { entries: [] }, incompatible: [] } })
  calls.setRoot.mockImplementation(async (body) => ({ app_id: body.app_id, revision: 'root-2' }))
  calls.limits.mockResolvedValue({ memory_bytes: '2147483648', cpu_millis: 0, memory_enforced: false, cpu_enforced: false })
  calls.setLimits.mockImplementation(async (body) => ({ ...body, memory_enforced: body.memory_bytes !== '0', cpu_enforced: false }))
  calls.backend.mockResolvedValue({ status: { state: 'ready', run_id: 'run-1' }, limits: { memory_bytes: '2147483648', cpu_millis: 0, pids: 64, open_files: 1024, cache_bytes: '1073741824', database_bytes: '536870912', enforced: { memory: false, cpu: false, pids: true, open_files: true } } })
  calls.logs.mockResolvedValue({ stdout: '<script>不执行</script>', stderr: '诊断', truncated: true })
  calls.restart.mockResolvedValue({ state: 'ready', run_id: 'run-2' })
  calls.preview.mockImplementation(async (source: InstallSource) => {
    if (source.type !== 'catalog') throw new Error('测试只允许精确目录更新')
    const current = installedApps.value.find((app) => app.manifest.id === source.app_id)!
    const selected: InstallPreview = { preview_id: `preview-${source.app_id}`, expires_at: Date.now() + 600000, manifest: { ...current.manifest, version: source.version }, package: { ...current.package, target: source.target, sha256: source.sha256 }, previous_state_revision: current.state_revision, added_file_read: false, added_file_write: false, starts_backend: true, external_network: true }
    previews.set(selected.preview_id, selected)
    return selected
  })
  calls.install.mockImplementation(async ({ preview_id }: { preview_id: string }) => {
    const selected = previews.get(preview_id)!
    const current = installedApps.value.find((app) => app.manifest.id === selected.manifest.id)!
    return { ...current, manifest: selected.manifest, package: selected.package, generation: 'updated-generation', state_revision: 'updated-state' }
  })
})

describe('应用中心一键更新', () => {
  it('单个更新选择最高兼容版本并沿用 ARM64，保留停用、固定和目录授权', async () => {
    const current = app('example-backend', false)
    current.pinned = true
    current.scope = { revision: 'scope-2', scope: { mode: 'all', access: 'read_only' }, roots: [] }
    installedApps.value = [current]
    calls.catalog.mockResolvedValue(ready([entry(undefined, '1.0.2'), entry(undefined, '1.0.10')]))
    const view = await render()
    expect(view.text()).toContain('可更新至 1.0.10')
    expect(view.findAll('button').filter((button) => button.text() === '安装预览')).toHaveLength(0)
    await click('更新')
    expect(calls.preview).toHaveBeenCalledWith({ type: 'catalog', app_id: current.manifest.id, version: '1.0.10', target: 'linux-arm64', sha256: 'b'.repeat(64) })
    expect(calls.install).toHaveBeenCalledTimes(1)
    expect(view.text()).toContain('更新完成')
    expect(view.text()).not.toContain('确认更新')
    expect(installedApps.value[0]).toMatchObject({ enabled: false, pinned: true, scope: current.scope, manifest: { version: '1.0.10' } })
  })
  it('全部更新先刷新目录，只更新已安装应用，失败后继续并允许单独重试', async () => {
    installedApps.value = [app('first'), app('second', false), app('no-compatible')]
    installedApps.value[0].pinned = true
    calls.catalog.mockResolvedValueOnce(ready([])).mockResolvedValue(ready([entry('first'), entry('second'), entry('no-compatible', '1.0.2', ['linux-amd64']), entry('uninstalled')]))
    calls.install.mockRejectedValueOnce(new Error('第一个包下载中断'))
    const view = await render()
    await click('一键更新')
    expect(calls.catalog).toHaveBeenCalledTimes(2)
    expect(calls.preview.mock.calls.map(([source]) => source.app_id)).toEqual(['first', 'second'])
    expect(calls.install).toHaveBeenCalledTimes(2)
    const results = view.findAll('.update-results li')
    expect(results).toHaveLength(2)
    expect(results[0].text()).toContain('第一个包下载中断')
    expect(results[1].text()).toContain('更新完成')
    expect(installedApps.value.find((app) => app.manifest.id === 'second')).toMatchObject({ enabled: false, manifest: { version: '1.0.1' } })
    await click('重试更新')
    expect(calls.install).toHaveBeenCalledTimes(3)
    expect(calls.preview.mock.calls.map(([source]) => source.app_id)).toEqual(['first', 'second', 'first'])
    expect(installedApps.value.find((app) => app.manifest.id === 'first')).toMatchObject({ pinned: true, manifest: { version: '1.0.1' } })
    expect(view.findAll('.update-results li')).toHaveLength(2)
    expect(view.findAll('.update-results li').every((result) => result.text().includes('更新完成'))).toBe(true)
  })
  it('当前架构缺少最高版本的包时选择已发布的最高同架构版本', async () => {
    calls.catalog.mockResolvedValue(ready([entry(undefined, '1.0.2'), entry(undefined, '1.0.3', ['linux-amd64'])]))
    const view = await render()
    expect(view.text()).toContain('可更新至 1.0.2')
    await click('更新')
    expect(calls.preview).toHaveBeenCalledWith(expect.objectContaining({ version: '1.0.2', target: 'linux-arm64' }))
  })
  it('新增文件写入权限时先展示预览，确认后才安装', async () => {
    calls.catalog.mockResolvedValue(ready([entry()]))
    const preview = calls.preview.getMockImplementation()!
    calls.preview.mockImplementation(async (source) => {
      const selected = await preview(source)
      selected.added_file_write = true
      return selected
    })
    const view = await render()
    await click('更新')
    expect(calls.install).not.toHaveBeenCalled()
    expect(view.text()).toContain('更新预览')
    expect(view.text()).toContain('本次新增：文件写入')
    await click('确认更新')
    expect(calls.install).toHaveBeenCalledTimes(1)
    expect(view.text()).toContain('更新完成')
  })
  it('预览绑定的旧安装修订已改变时拒绝静默套用更新', async () => {
    calls.catalog.mockResolvedValue(ready([entry()]))
    const preview = calls.preview.getMockImplementation()!
    calls.preview.mockImplementation(async (source) => ({ ...await preview(source), previous_state_revision: 'another-state' }))
    const view = await render()
    await click('更新')
    expect(calls.install).not.toHaveBeenCalled()
    expect(view.text()).toContain('应用状态已改变，请刷新后再更新')
  })
  it('目录不可用时明确显示失败，不把未知状态当成已是最新版', async () => {
    calls.catalog.mockResolvedValue({ ...ready([]), status: { state: 'unavailable', repository: 'test/apps', last_success_at: null, error: '目录读取失败' } })
    const view = await render()
    await click('一键更新')
    expect(calls.install).not.toHaveBeenCalled()
    expect(view.find('[title="应用目录暂时不可用，请稍后重试或上传应用包"]').exists()).toBe(true)
  })
})
afterEach(() => { wrapper?.unmount(); wrapper = undefined; vi.useRealTimers() })

describe('应用中心后端管理', () => {
  it('目录读取成功但尚未发布应用时说明空目录状态', async () => {
    installedApps.value = []
    calls.catalog.mockResolvedValue({ status: { state: 'ready' }, result: { catalog: { entries: [] }, incompatible: [] } })
    const view = await render()
    expect(view.text()).toContain('当前目录尚未发布应用，可以上传 .tgapp 应用包安装。')
    expect(view.text()).not.toContain('目录暂时不可用')
  })
  it('重新刷新成功后清除上次操作的错误', async () => {
    calls.catalog.mockRejectedValueOnce(new Error('拒绝跨源应用中心操作'))
    const view = await render()
    expect(view.find('[title="拒绝跨源应用中心操作"]').exists()).toBe(true)
    await click('刷新目录')
    expect(view.find('[title="拒绝跨源应用中心操作"]').exists()).toBe(false)
  })
  it('展示真实 enforced，而不是把目标配额称为硬限制', async () => {
    const view = await render(); await click('资源限制')
    expect(calls.backend).toHaveBeenCalledWith('example-backend')
    expect(view.text()).toContain('内存（全部应用合计）2147483648 B · 未强制（没有可写的 cgroup）')
    expect(view.text()).toContain('CPU（全部应用合计）不限制')
    expect(view.text()).toContain('本轮已强制')
    expect(view.text()).toContain('监督式计量')
    expect(view.text()).toContain('不是实时硬隔离')
  })
  it('根地址选择与清除使用当前修订，停用项不能选择', async () => {
    installedApps.value = [app(), app('disabled-backend', false)]
    const view = await render()
    expect(view.find('option[value="disabled-backend"]').attributes('disabled')).toBeDefined()
    await view.find('select').setValue('example-backend'); await flushPromises()
    expect(calls.setRoot).toHaveBeenLastCalledWith({ expected_revision: 'root-1', app_id: 'example-backend' })
    await view.find('select').setValue(''); await flushPromises()
    expect(calls.setRoot).toHaveBeenLastCalledWith({ expected_revision: 'root-2', app_id: null })
  })
  it('应用资源限制只有一组总体设置，按 MiB 与核填写并换算保存，0 表示不限制', async () => {
    const view = await render()
    const memory = view.find('input[aria-label="应用内存上限"]'), cpu = view.find('input[aria-label="应用 CPU 上限"]')
    expect((memory.element as HTMLInputElement).value).toBe('2048')
    expect((cpu.element as HTMLInputElement).value).toBe('0')
    expect(view.find('.limit-state').text()).toBe('当前：内存 2147483648 B · 未强制（没有可写的 cgroup）；CPU 不限制')
    const save = () => view.findAll('button').find(button => button.text() === '保存限制')!
    expect(save().attributes('disabled')).toBeDefined()
    await memory.setValue('32'); await click('保存限制')
    expect(calls.setLimits).not.toHaveBeenCalled()
    expect(view.find('[title="内存上限需为 0（不限制）或 64 MiB～1 TiB"]').exists()).toBe(true)
    await memory.setValue(''); await click('保存限制')
    expect(calls.setLimits).not.toHaveBeenCalled()
    expect(view.find('[title="请填写内存与 CPU 上限，0 表示不限制"]').exists()).toBe(true)
    await memory.setValue('4096'); await cpu.setValue('1.5'); await click('保存限制')
    expect(calls.setLimits).toHaveBeenLastCalledWith({ memory_bytes: '4294967296', cpu_millis: 1500 })
    expect(view.find('.limit-state').text()).toBe('当前：内存 4294967296 B · 已强制；CPU 1.5 核 · 未强制（没有可写的 cgroup）')
    expect(save().attributes('disabled')).toBeDefined()
    await memory.setValue('0'); await cpu.setValue('0'); await click('保存限制')
    expect(calls.setLimits).toHaveBeenLastCalledWith({ memory_bytes: '0', cpu_millis: 0 })
    expect(view.find('.limit-state').text()).toBe('当前：内存 不限制；CPU 不限制')
  })
  it('日志只作为文本显示，重启后重读状态，卸载页面时停止状态定时器', async () => {
    const view = await render(); await click('后端日志')
    expect(view.text()).toContain('<script>不执行</script>')
    expect(view.find('script').exists()).toBe(false)
    await click('重启后端')
    expect(calls.restart).toHaveBeenCalledWith('example-backend')
    const before = calls.load.mock.calls.length
    await vi.advanceTimersByTimeAsync(5000)
    expect(calls.load.mock.calls.length).toBe(before + 1)
    view.unmount(); wrapper = undefined
    expect(vi.getTimerCount()).toBe(0)
  })
})
