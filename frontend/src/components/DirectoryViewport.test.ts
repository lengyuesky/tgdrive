import { flushPromises, mount } from '@vue/test-utils'
import { defineComponent, h, nextTick, ref } from 'vue'
import { afterEach, describe, expect, it } from 'vitest'
import DirectoryViewport from './DirectoryViewport.vue'
import VirtualViewport from './VirtualViewport.vue'
import { entry } from '../test/fixtures'
import { resizeViewport } from '../test/setup'
import type { DirectoryView } from '../directory/layout'

const wrappers: { unmount: () => void }[] = []
afterEach(() => { wrappers.splice(0).forEach((wrapper) => wrapper.unmount()); resizeViewport(1000) })
async function settle() { await nextTick(); await flushPromises(); await nextTick() }
function directory(mode: DirectoryView, size = 10000) {
  const entries = Array.from({ length: size }, (_, i) => entry(i))
  const wrapper = mount(DirectoryViewport, {
    props: { entries, count: entries.length, mode, generation: 1, revision: 0, indexOf: (id) => id },
    slots: { default: ({ entry: row, mediaKey, generation }) => h('span', { 'data-media-key': mediaKey, 'data-generation': generation }, row.name), header: () => h('div', '表头') },
    attachTo: document.body,
  })
  wrappers.push(wrapper)
  return { wrapper, entries }
}

describe('四视图虚拟挂载', () => {
  it.each(['list', 'grid', 'photo', 'timeline'] as const)('%s只挂载可见缓冲，不随10000条元数据增长', async (mode) => {
    const { wrapper } = directory(mode)
    await settle()
    let nodes = wrapper.findAll('[data-entry-id]')
    expect(nodes.length).toBeGreaterThan(0)
    expect(nodes.length).toBeLessThanOrEqual(200)
    const element = wrapper.find('[data-testid="virtual-viewport"]').element as HTMLElement
    element.scrollTo({ top: 20000 })
    await settle()
    nodes = wrapper.findAll('[data-entry-id]')
    expect(nodes.length).toBeGreaterThan(0)
    expect(nodes.length).toBeLessThanOrEqual(200)
    expect(Number(nodes[0]!.attributes('data-entry-id'))).toBeGreaterThan(0)
    expect(wrapper.find('[data-media-key]').attributes('data-generation')).toBe('1')
  })

  it.each(['grid', 'photo', 'timeline'] as const)('极宽高窗口%s限制20列/720px，全部可见行有内容', async (mode) => {
    resizeViewport(10000, 10000)
    const { wrapper } = directory(mode)
    await settle()
    const element = wrapper.find('[data-testid="virtual-viewport"]').element as HTMLElement
    element.scrollTo({ top: 20000 })
    await settle()
    const nodes = wrapper.findAll('[data-entry-id]')
    expect(nodes.length).toBeLessThanOrEqual(200)
    const rows = wrapper.findAll('.virtual-row')
    const positions = rows.map((row) => ({
      start: Number((row.attributes('style') ?? '').match(/translateY\((\d+)px\)/)?.[1]),
      size: Number((row.attributes('style') ?? '').match(/height: (\d+)px/)?.[1]),
    }))
    expect(Math.min(...positions.map((item) => item.start))).toBeLessThanOrEqual(20000)
    expect(Math.max(...positions.map((item) => item.start + item.size))).toBeGreaterThanOrEqual(20720)
  })

  it('追加同数组的200/201条也更新timeline行和跨批次组头', async () => {
    const { wrapper, entries } = directory('timeline', 200)
    await settle()
    expect(wrapper.text()).toContain('已加载 200 项')
    entries.push(entry(200))
    await wrapper.setProps({ count: 201 })
    await settle()
    expect(wrapper.text()).toContain('已加载 201 项')
    expect(wrapper.findAll('.timeline-heading')).toHaveLength(1)
  })

  it('resize以稳定ID重定位，换目录代次和快速切时间线清零滚动', async () => {
    const { wrapper, entries } = directory('grid')
    await settle()
    const element = wrapper.find('[data-testid="virtual-viewport"]').element as HTMLElement
    element.scrollTo({ top: 10000 })
    await settle()
    const before = wrapper.findAll('.virtual-row').find((row) => {
      const start = Number(row.attributes('style')?.match(/translateY\((\d+)px\)/)?.[1])
      return start <= 10000 && start + 192 > 10000
    })!
    const anchor = Number(before.find('[data-entry-id]').attributes('data-entry-id'))
    resizeViewport(600)
    await settle()
    expect(wrapper.find(`[data-entry-id="${anchor}"]`).exists()).toBe(true)
    await wrapper.setProps({ mode: 'timeline', generation: 2, entries: entries.slice(0, 200), count: 200 })
    await wrapper.setProps({ mode: 'list', generation: 3, entries: entries.slice(0, 200), count: 200 })
    await settle()
    expect(element.scrollTop).toBe(0)
    expect(wrapper.find('[data-entry-id="0"]').exists()).toBe(true)
    expect(wrapper.findAll('.timeline-heading')).toHaveLength(0)
  })

  it('Set选择滚出卸载再返回仍选中，新批次不自动选中', async () => {
    const selected = ref(new Set([0]))
    const entries = Array.from({ length: 1000 }, (_, i) => entry(i))
    const wrapper = mount(defineComponent({ setup: () => () => h(DirectoryViewport, {
      entries, count: entries.length, mode: 'list', generation: 1, revision: 0, indexOf: (id: number) => id,
    }, { default: ({ entry: row }: { entry: { id: number } }) => h('input', { type: 'checkbox', checked: selected.value.has(row.id) }) }) }), { attachTo: document.body })
    wrappers.push(wrapper)
    await settle()
    const element = wrapper.find('[data-testid="virtual-viewport"]').element as HTMLElement
    expect((wrapper.find('[data-entry-id="0"] input').element as HTMLInputElement).checked).toBe(true)
    element.scrollTo({ top: 20000 }); await settle()
    expect(wrapper.find('[data-entry-id="0"]').exists()).toBe(false)
    element.scrollTo({ top: 0 }); await settle()
    expect((wrapper.find('[data-entry-id="0"] input').element as HTMLInputElement).checked).toBe(true)
    expect((wrapper.find('[data-entry-id="1"] input').element as HTMLInputElement).checked).toBe(false)
  })

  it('sticky表头计入scrollMargin和两视口续载距离', async () => {
    const wrapper = mount(VirtualViewport, {
      props: { count: 100, estimateSize: () => 56, itemKey: (i) => i, rowHeight: 56 },
      slots: { header: () => h('div', '表头'), default: ({ index }) => h('div', String(index)) },
      attachTo: document.body,
    })
    wrappers.push(wrapper)
    await settle()
    const element = wrapper.element as HTMLElement
    element.scrollTo({ top: 3820 }); await settle()
    expect(wrapper.emitted('nearEnd')).toBeUndefined()
    element.scrollTo({ top: 3840 }); await settle()
    expect(wrapper.emitted('nearEnd')).toHaveLength(1)
    expect(wrapper.vm.firstVisibleIndex()).toBeGreaterThan(0)
    wrapper.vm.scrollToIndex(80); await settle()
    // 行顶留在sticky表头下方，不被36px表头覆盖。
    expect(element.scrollTop).toBe(80 * 56)
    expect(wrapper.vm.firstVisibleIndex()).toBe(80)
  })
})
