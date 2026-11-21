import { afterEach, expect, it, vi } from 'vitest'
import { closeAllOverlays, closeTopOverlay, isTopOverlay, registerOverlay } from './overlays'

const releases: Array<() => void> = []
function register(close: () => void, element?: () => HTMLElement | null) {
  const release = registerOverlay(close, element)
  releases.push(release)
  return release
}
function button(parent: HTMLElement = document.body) {
  const element = document.createElement('button')
  parent.append(element)
  return element
}
afterEach(() => {
  for (const release of releases.splice(0).reverse()) release()
  document.body.style.overflow = ''
  document.documentElement.style.overflow = ''
  document.body.replaceChildren()
})

it('嵌套弹层只关闭顶层，全部释放后恢复原始滚动样式', () => {
  document.body.style.overflow = 'auto'
  document.documentElement.style.overflow = 'scroll'
  const events: string[] = []
  const releaseParent = register(() => events.push('父弹层'))
  const releaseChild = register(() => events.push('子弹层'))
  expect(document.body.style.overflow).toBe('hidden')
  expect(closeTopOverlay()).toBe(true)
  expect(events).toEqual(['子弹层'])
  releaseChild(); releaseChild()
  expect(document.body.style.overflow).toBe('hidden')
  closeTopOverlay(); expect(events).toEqual(['子弹层', '父弹层'])
  releaseParent()
  expect(document.body.style.overflow).toBe('auto')
  expect(document.documentElement.style.overflow).toBe('scroll')
  expect(closeTopOverlay()).toBe(false)
})

it('父弹层提前卸载不会解锁或从子弹层抢走焦点', () => {
  const trigger = button()
  trigger.focus()
  const releaseParent = register(() => {})
  const parentButton = button()
  parentButton.focus()
  const child = document.createElement('section')
  document.body.append(child)
  const childButton = button(child)
  const releaseChild = register(() => {}, () => child)
  childButton.focus()
  releaseParent()
  expect(document.body.style.overflow).toBe('hidden')
  expect(document.activeElement).toBe(childButton)
  releaseChild()
  expect(document.body.style.overflow).toBe('')
  expect(document.activeElement).toBe(parentButton)
})

it('程序化焦点不能逃出自定义弹层，关闭后恢复触发按钮', () => {
  const trigger = button()
  trigger.focus()
  const dialog = document.createElement('section')
  document.body.append(dialog)
  const first = button(dialog), second = button(dialog)
  const close = vi.fn()
  const release = register(close, () => dialog)
  expect(isTopOverlay(close)).toBe(true)
  first.focus(); second.focus()
  trigger.focus()
  expect(document.activeElement).toBe(second)
  release()
  expect(document.activeElement).toBe(trigger)
  expect(isTopOverlay(close)).toBe(false)
})

it('自定义子弹层获得焦点时阻止底层 Element 焦点锁抢回焦点', () => {
  const parent = document.createElement('section'), child = document.createElement('section')
  document.body.append(parent, child)
  const parentButton = button(parent), childButton = button(child)
  const focusOut = vi.fn()
  parent.addEventListener('focusout', focusOut)
  register(() => {})
  parentButton.focus()
  register(() => {}, () => child)
  childButton.focus()
  expect(document.activeElement).toBe(childButton)
  expect(focusOut).not.toHaveBeenCalled()
})

it('认证失效由顶向下关闭所有弹层并恢复滚动', () => {
  const events: string[] = []
  const releaseParent = register(() => { events.push('父弹层'); releaseParent() })
  const releaseChild = register(() => { events.push('子弹层'); releaseChild() })
  closeAllOverlays()
  expect(events).toEqual(['子弹层', '父弹层'])
  expect(closeTopOverlay()).toBe(false)
  expect(document.body.style.overflow).toBe('')
})
