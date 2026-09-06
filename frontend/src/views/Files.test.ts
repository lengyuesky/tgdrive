import { flushPromises, shallowMount } from '@vue/test-utils'
import { defineComponent, nextTick, reactive } from 'vue'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { EntriesQuery, EntriesResponse } from '../api'
import { entry } from '../test/fixtures'
import Files from './Files.vue'
import DirectoryViewport from '../components/DirectoryViewport.vue'
import UploadDrawer from '../components/UploadDrawer.vue'

const mocks = vi.hoisted(() => ({ entries: vi.fn(), info: vi.fn(), route: { params: { path: [] as string[] } } }))
vi.mock('vue-router', () => ({ useRoute: () => mocks.route, useRouter: () => ({ push: vi.fn() }) }))
vi.mock('../api', () => ({
  api: { entries: mocks.entries, info: mocks.info, downloadUrl: (path: string) => path, thumbnailUrl: (path: string) => path },
  errMsg: String, fmtSize: String, fmtTime: String, isVideo: () => false, VIDEO_EXT: ['mp4'],
}))
vi.mock('../dialog', () => ({ appAlert: vi.fn(), appChoice: vi.fn(), appPrompt: vi.fn() }))

let wrapper: ReturnType<typeof shallowMount> | undefined
beforeEach(() => {
  localStorage.clear()
  mocks.route = reactive({ params: { path: [] as string[] } })
  mocks.entries.mockReset().mockImplementation(async (params: EntriesQuery) => ({ data: {
    path: params.path, entries: [entry(1)], next_cursor: 'next', has_more: true, total: 201,
  } satisfies EntriesResponse }))
  mocks.info.mockResolvedValue({ data: null })
  vi.useFakeTimers()
})
afterEach(() => { wrapper?.unmount(); vi.useRealTimers() })
async function render() {
  wrapper = shallowMount(Files, { global: { plugins: [ElementPlus], stubs: {
    ElTooltip: defineComponent({ name: 'ElTooltip', props: ['content'], template: '<div><slot /></div>' }),
  } } })
  await flushPromises()
  return wrapper
}

describe('Files远端接入与刷新语义', () => {
  it('过滤输入立即隔离旧代且250ms防抖，路径变化清查询并取消等待定时器', async () => {
    const view = await render()
    const input = view.findComponent({ name: 'ElInput' })
    const initial = view.findComponent(DirectoryViewport).props('generation')
    input.vm.$emit('update:modelValue', 'a')
    await nextTick()
    expect(view.findComponent(DirectoryViewport).props('generation')).toBeGreaterThan(initial)
    expect(view.findComponent(DirectoryViewport).props('count')).toBe(0)
    view.findComponent(DirectoryViewport).vm.$emit('nearEnd')
    await vi.advanceTimersByTimeAsync(249)
    expect(mocks.entries).toHaveBeenCalledTimes(1)
    input.vm.$emit('update:modelValue', '%_')
    await vi.advanceTimersByTimeAsync(250)
    expect(mocks.entries).toHaveBeenCalledTimes(2)
    expect(mocks.entries.mock.calls[1]![0].q).toBe('%_')
    input.vm.$emit('update:modelValue', 'old')
    mocks.route.params.path = ['new']
    await nextTick(); await flushPromises()
    expect(mocks.entries.mock.calls.at(-1)![0]).toMatchObject({ path: '/new', q: '', limit: 200 })
    const count = mocks.entries.mock.calls.length
    await vi.advanceTimersByTimeAsync(300)
    expect(mocks.entries).toHaveBeenCalledTimes(count)
  })

  it('时间线强制远端file/mtime/desc，快速切回列表建立新代而不是客户端排序', async () => {
    const view = await render()
    const timeline = view.findAllComponents({ name: 'ElTooltip' }).find((tooltip) => tooltip.props('content') === '时间线(按日期看图)')!
    timeline.findComponent({ name: 'ElButton' }).vm.$emit('click')
    await nextTick(); await flushPromises()
    expect(mocks.entries.mock.calls.at(-1)![0]).toMatchObject({ kind: 'file', sort: 'mtime', order: 'desc' })
    const generation = view.findComponent(DirectoryViewport).props('generation')
    const list = view.findAllComponents({ name: 'ElTooltip' }).find((tooltip) => tooltip.props('content') === '列表视图')!
    list.findComponent({ name: 'ElButton' }).vm.$emit('click')
    await nextTick(); await flushPromises()
    expect(mocks.entries.mock.calls.at(-1)![0]).toMatchObject({ kind: 'all', sort: 'name', order: 'asc' })
    expect(view.findComponent(DirectoryViewport).props('generation')).toBeGreaterThan(generation)
  })

  it('上传完成在向下浏览时仅提示；顶部无选择时合并刷新', async () => {
    const view = await render()
    view.findComponent(DirectoryViewport).vm.$emit('scroll', 500)
    view.findComponent(UploadDrawer).vm.$emit('uploaded')
    await vi.advanceTimersByTimeAsync(1000)
    expect(mocks.entries).toHaveBeenCalledTimes(1)
    expect(view.text()).toContain('当前浏览和选择已保留')
    view.findComponent(DirectoryViewport).vm.$emit('scroll', 0)
    view.findComponent(UploadDrawer).vm.$emit('uploaded')
    view.findComponent(UploadDrawer).vm.$emit('uploaded')
    await vi.advanceTimersByTimeAsync(1000)
    expect(mocks.entries).toHaveBeenCalledTimes(2)
  })

  it('Ctrl+A只更新已加载Set，顶部有选择也不自动刷新', async () => {
    const view = await render()
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', ctrlKey: true }))
    await nextTick()
    view.findComponent(UploadDrawer).vm.$emit('uploaded')
    await vi.advanceTimersByTimeAsync(1000)
    expect(mocks.entries).toHaveBeenCalledTimes(1)
    expect(view.text()).toContain('当前浏览和选择已保留')
  })
})
