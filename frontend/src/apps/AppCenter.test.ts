import { defineComponent, h, nextTick } from 'vue'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { InstalledApp } from './contract'

const calls = vi.hoisted(() => ({
  load: vi.fn(), catalog: vi.fn(), setRoot: vi.fn(), backend: vi.fn(), logs: vi.fn(), restart: vi.fn(), push: vi.fn(),
}))
vi.mock('vue-router', async (importOriginal) => ({ ...await importOriginal<typeof import('vue-router')>(), useRouter: () => ({ push: calls.push }) }))
vi.mock('./client', async () => {
  const { shallowRef } = await import('vue')
  return {
    installedApps: shallowRef<InstalledApp[]>([]), rootSelection: shallowRef({ app_id: null as string | null, revision: 'root-1' }),
    loadInstalled: calls.load, iconUrl: () => undefined, formatBytes: (value: string) => `${value} B`, usage: vi.fn(),
    center: { catalog: calls.catalog, refresh: calls.catalog, setRoot: calls.setRoot, backend: calls.backend, logs: calls.logs, restart: calls.restart },
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
function app(id = 'example-backend', enabled = true): InstalledApp {
  return { manifest: { id, name: '测试服务', version: '1.0.0', description: '测试', contract: { major: 1, minor: 0 }, author: '测试', required_capabilities: [], open_with: { extensions: [], directories: false }, files: { read: true, write: true }, backend: { entry: 'backend/app', run_mode: 'resident', external_service: true, resources: {} } }, enabled, pinned: false, generation: 'g1', state_revision: 's1', package: { target: 'linux-arm64', sha256: 'a'.repeat(64), size: '1' }, installed_at: 1, updated_at: 1, scope: { revision: 'scope-1', scope: { mode: 'none' }, roots: [] }, backend: { state: 'ready', run_id: 'run-1', run_mode: 'resident', error_code: null } } as InstalledApp
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
  installedApps.value = [app()]; rootSelection.value = { app_id: null, revision: 'root-1' }
  calls.load.mockResolvedValue(installedApps.value)
  calls.catalog.mockResolvedValue({ status: { state: 'disabled' }, result: { catalog: { entries: [] }, incompatible: [] } })
  calls.setRoot.mockImplementation(async (body) => ({ app_id: body.app_id, revision: 'root-2' }))
  calls.backend.mockResolvedValue({ status: { state: 'ready', run_id: 'run-1' }, limits: { memory_bytes: '536870912', cpu_millis: 1000, pids: 64, open_files: 1024, cache_bytes: '1073741824', database_bytes: '536870912', enforced: { memory: false, cpu: false, pids: true, open_files: true } } })
  calls.logs.mockResolvedValue({ stdout: '<script>不执行</script>', stderr: '诊断', truncated: true })
  calls.restart.mockResolvedValue({ state: 'ready', run_id: 'run-2' })
})
afterEach(() => { wrapper?.unmount(); wrapper = undefined; vi.useRealTimers() })

describe('应用中心后端管理', () => {
  it('展示真实 enforced，而不是把目标配额称为硬限制', async () => {
    const view = await render(); await click('资源限制')
    expect(calls.backend).toHaveBeenCalledWith('example-backend')
    expect(view.text()).toContain('enforced=false')
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
