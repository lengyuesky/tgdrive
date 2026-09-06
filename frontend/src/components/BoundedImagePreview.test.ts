import { mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import { afterEach, describe, expect, it, vi } from 'vitest'
import BoundedImagePreview from './BoundedImagePreview.vue'
import { entry } from '../test/fixtures'

const downloadUrl = vi.hoisted(() => vi.fn((path: string) => path))
vi.mock('../api', () => ({ api: { downloadUrl } }))
let view: ReturnType<typeof mount> | undefined
afterEach(() => { view?.unmount(); downloadUrl.mockClear() })

describe('图片预览有界窗口', () => {
  it('一万条只构造三张URL，切换仅相邻窗口，边界不循环、不请求元数据', async () => {
    const entries = Array.from({ length: 10000 }, (_, i) => entry(i))
    view = mount(BoundedImagePreview, { props: { entries, count: entries.length, index: 5000, base: '/图片' }, global: { stubs: { Teleport: true } } })
    expect(view.findAll('img')).toHaveLength(3)
    expect(downloadUrl).toHaveBeenCalledTimes(3)
    expect(view.findAll('img:not(.neighbor)')).toHaveLength(1)
    expect(view.text()).toContain('仅浏览当前已加载')
    await view.get('[aria-label="下一张图片"]').trigger('click')
    expect(view.emitted('update:index')?.[0]).toEqual([5001])
    await view.setProps({ index: 5001 })
    expect(view.findAll('img').map((img) => img.attributes('src'))).toEqual(['/图片/5000.jpg', '/图片/5001.jpg', '/图片/5002.jpg'])
    await view.setProps({ index: 9999 })
    expect(view.findAll('img')).toHaveLength(2)
    expect(view.get('[aria-label="下一张图片"]').attributes('disabled')).toBeDefined()
    view.unmount(); view = undefined
    expect(document.querySelectorAll('.image-preview img')).toHaveLength(0)
  })

  it('跳过已加载非图片，支持键盘/缩放/旋转/关闭并恢复焦点', async () => {
    const button = document.createElement('button'); document.body.append(button); button.focus()
    const entries = [({ ...entry(1), ...{ name: '1.jpg' } }), ({ ...entry(2), ...{ name: 'dir', is_dir: true } }), ({ ...entry(3), ...{ name: '3.mp4' } }), ({ ...entry(4), ...{ name: '4.jpg' } })]
    view = mount(BoundedImagePreview, { props: { entries, count: 4, index: 0, base: '/' }, attachTo: document.body })
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' }))
    expect(view.emitted('update:index')?.[0]).toEqual([3])
    document.querySelector<HTMLButtonElement>('[aria-label="放大图片"]')!.click()
    document.querySelector<HTMLButtonElement>('[aria-label="旋转图片"]')!.click()
    await nextTick()
    expect(document.querySelector('img:not(.neighbor)')?.getAttribute('style')).toContain('scale(1.25) rotate(90deg)')
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    expect(view.emitted('close')).toHaveLength(1)
    view.unmount(); view = undefined
    expect(document.activeElement).toBe(button)
    expect(document.querySelectorAll('.image-preview img')).toHaveLength(0)
    button.remove()
  })
})
