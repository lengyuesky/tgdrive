import { afterEach, expect, it, vi } from 'vitest'
import { HostImmersion } from './immersive'
afterEach(() => { document.head.querySelectorAll('meta[name="viewport"]').forEach((node) => node.remove()); document.body.style.cssText = ''; document.documentElement.style.cssText = '' })
const signal = () => new AbortController().signal
it('沉浸校验 API 和严格参数，非法输入不会检查会话或改变布局', async () => {
  const change = vi.fn(), verify = vi.fn(), painted = vi.fn(), host = new HostImmersion(change)
  await expect(host.request({ active: true }, 1, verify, signal(), painted)).rejects.toThrow('v2')
  for (const params of [{}, { active: 'true' }, { active: true, url: '/admin' }]) await expect(host.request(params, 2, verify, signal(), painted)).rejects.toThrow('布尔值')
  expect(verify).not.toHaveBeenCalled(); expect(change).not.toHaveBeenCalled()
})
it('只有有效会话能隐藏宿主，完成绘制才确认，退出恢复 viewport 和原滚动样式', async () => {
  const meta = document.createElement('meta'); meta.name = 'viewport'; meta.content = 'width=device-width, initial-scale=1'; document.head.append(meta)
  document.body.style.overflow = 'auto'
  const changed = vi.fn(), painted = vi.fn(async () => {}), host = new HostImmersion(changed)
  await expect(host.request({ active: true }, 2, async () => { throw new Error('会话过期') }, signal(), painted)).rejects.toThrow('过期')
  expect(changed).toHaveBeenLastCalledWith(false)
  await host.request({ active: true }, 2, async () => {}, signal(), painted)
  expect(changed).toHaveBeenLastCalledWith(true); expect(painted).toHaveBeenCalledOnce()
  expect(meta.content).toContain('viewport-fit=cover'); expect(document.body.style.overflowX).toBe('hidden'); expect(document.body.style.overflowY).toBe('hidden')
  host.reset()
  expect(changed).toHaveBeenLastCalledWith(false)
  expect(meta.content).toBe('width=device-width, initial-scale=1'); expect(document.body.style.overflow).toBe('auto')
  expect(document.documentElement.style.overflow).toBe('')
})
it('主屏幕已有 cover 设置时，进入和退出不重写 viewport 元数据', async () => {
  const meta = document.createElement('meta'); meta.name = 'viewport'; meta.content = 'width=device-width, initial-scale=1.0, viewport-fit=cover'; document.head.append(meta)
  const write = vi.spyOn(meta, 'content', 'set'), host = new HostImmersion(vi.fn())
  await host.request({ active: true }, 2, async () => {}, signal(), async () => {})
  host.reset()
  expect(write).not.toHaveBeenCalled()
  write.mockRestore()
})
it('背景只接受不透明颜色，并在切换主题、退出与会话失效时清理', async () => {
  const changed = vi.fn(), verify = vi.fn(async () => {}), painted = vi.fn(async () => {}), host = new HostImmersion(changed)
  for (const background of ['', '#fff', '#00000000', 'url(https://example.com)', 'red', null, 123]) {
    await expect(host.request({ active: true, background }, 2, verify, signal(), painted)).rejects.toThrow('颜色')
  }
  expect(verify).not.toHaveBeenCalled(); expect(changed).not.toHaveBeenCalled()
  await host.request({ active: true, background: '#f4ebd6' }, 2, verify, signal(), painted)
  expect(changed).toHaveBeenLastCalledWith(true, '#f4ebd6')
  await host.request({ active: true, background: '#141d1c' }, 2, verify, signal(), painted)
  expect(changed).toHaveBeenLastCalledWith(true, '#141d1c')
  await host.request({ active: false, background: '#141d1c' }, 2, verify, signal(), painted)
  expect(changed).toHaveBeenLastCalledWith(false)
  await host.request({ active: true, background: '#f4ebd6' }, 2, verify, signal(), painted)
  await expect(host.request({ active: true, background: '#141d1c' }, 2, async () => { throw new Error('会话过期') }, signal(), painted)).rejects.toThrow('过期')
  expect(changed).toHaveBeenLastCalledWith(false)
})
it('迟到的开启请求不能盖过关闭请求或已销毁的会话', async () => {
  const changed = vi.fn(), host = new HostImmersion(changed)
  let finish!: () => void
  const opening = host.request({ active: true }, 2, () => new Promise<void>((resolve) => { finish = resolve }), signal(), async () => {})
  await host.request({ active: false }, 2, async () => {}, signal(), async () => {})
  finish(); await opening
  expect(changed.mock.calls).toEqual([[false]])
  const old = host.request({ active: true }, 2, () => new Promise<void>((resolve) => { finish = resolve }), signal(), async () => {})
  host.reset(); finish(); await old
  expect(changed.mock.calls).toEqual([[false], [false]])
})
it('绘制期间取消会恢复原始单轴滚动配置与导航', async () => {
  const changed = vi.fn(), host = new HostImmersion(changed), controller = new AbortController()
  document.body.style.setProperty('overflow-x', 'clip', 'important')
  await expect(host.request({ active: true }, 2, async () => {}, controller.signal, async () => { controller.abort() })).rejects.toMatchObject({ name: 'AbortError' })
  expect(changed.mock.calls).toEqual([[true], [false]])
  expect(document.body.style.getPropertyValue('overflow-x')).toBe('clip')
  expect(document.body.style.getPropertyPriority('overflow-x')).toBe('important')
  expect(document.body.style.getPropertyValue('overflow-y')).toBe('')
  expect(document.querySelector('meta[name="viewport"]')).toBeNull()
})
it('取消的会话检查不会进入沉浸，也不会遗留临时 viewport', async () => {
  const changed = vi.fn(), host = new HostImmersion(changed), controller = new AbortController()
  controller.abort()
  await expect(host.request({ active: true }, 2, async () => {}, controller.signal, async () => {})).rejects.toMatchObject({ name: 'AbortError' })
  expect(changed).toHaveBeenLastCalledWith(false); expect(document.querySelector('meta[name="viewport"]')).toBeNull()
})
