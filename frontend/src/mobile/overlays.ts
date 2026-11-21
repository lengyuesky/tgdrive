import { onBeforeUnmount, watch, type Ref } from 'vue'

type Layer = {
  close: () => void
  element?: () => HTMLElement | null
  lastFocus?: HTMLElement
}
const layers: Layer[] = []
let restore: (() => void) | undefined

/** 自定义弹层在 Element 弹窗之上时，不让底层的焦点约束把焦点抢回去。 */
function protectFocusOut(event: FocusEvent) {
  const element = layers.at(-1)?.element?.()
  if (element && event.target instanceof Node && !element.contains(event.target)
    && event.relatedTarget instanceof Node && element.contains(event.relatedTarget)) {
    event.stopImmediatePropagation()
  }
}
function protectFocusIn(event: FocusEvent) {
  const layer = layers.at(-1)
  const element = layer?.element?.()
  if (!layer || !element) return // Element 弹层继续使用组件自带的焦点约束。
  if (event.target instanceof HTMLElement && element.contains(event.target)) {
    layer.lastFocus = event.target
    return
  }
  const target = layer.lastFocus?.isConnected ? layer.lastFocus
    : element.querySelector<HTMLElement>('button:not(:disabled), input:not(:disabled), [href], [tabindex="0"]') ?? element
  target.focus({ preventScroll: true })
}

/** 自定义与 Element 弹层共用引用计数，嵌套关闭不解锁底层。 */
export function registerOverlay(close: () => void, element?: () => HTMLElement | null) {
  const layer: Layer = { close, element }
  const focus = document.activeElement instanceof HTMLElement ? document.activeElement : null
  if (!layers.length) {
    const body = document.body
    const root = document.documentElement
    const bodyOverflow = body.style.overflow
    const rootOverflow = root.style.overflow
    body.style.overflow = 'hidden'
    root.style.overflow = 'hidden'
    document.addEventListener('focusout', protectFocusOut, true)
    document.addEventListener('focusin', protectFocusIn, true)
    restore = () => {
      body.style.overflow = bodyOverflow
      root.style.overflow = rootOverflow
      document.removeEventListener('focusout', protectFocusOut, true)
      document.removeEventListener('focusin', protectFocusIn, true)
    }
  }
  layers.push(layer)
  let removed = false
  return () => {
    if (removed) return
    removed = true
    const wasTop = layers.at(-1) === layer
    const index = layers.indexOf(layer)
    if (index >= 0) layers.splice(index, 1)
    if (!layers.length) { restore?.(); restore = undefined }
    // 父弹层提前卸载时不能把焦点从仍然打开的子弹层移走。
    if (wasTop && focus?.isConnected) focus.focus({ preventScroll: true })
  }
}

export function isTopOverlay(close: () => void) {
  return layers.at(-1)?.close === close
}
export function closeTopOverlay() {
  const layer = layers.at(-1)
  if (!layer) return false
  layer.close()
  return true
}
/** 认证失效时也关闭独立挂到 body 的确认框，避免旧会话弹层留在登录页。 */
export function closeAllOverlays() {
  for (const layer of [...layers].reverse()) layer.close()
}
export function useOverlay(visible: Ref<boolean>, close = () => { visible.value = false }) {
  let release: (() => void) | undefined
  watch(visible, (active) => {
    release?.(); release = undefined
    if (active) release = registerOverlay(close)
  }, { immediate: true, flush: 'sync' })
  onBeforeUnmount(() => release?.())
}
