import { computed, readonly, ref } from 'vue'

const width = ref(typeof window === 'undefined' ? 1024 : window.innerWidth)
const coarsePointer = ref(typeof window !== 'undefined' && window.matchMedia?.('(pointer: coarse)').matches === true)
const keyboard = ref(false)
const standalone = ref(false)
const online = ref(typeof navigator === 'undefined' || navigator.onLine)
export const mobile = computed(() => width.value <= 767)
export const compact = computed(() => width.value < 1024)
export const touchUI = computed(() => mobile.value || coarsePointer.value)
export const keyboardVisible = readonly(keyboard)
export const isStandalone = readonly(standalone)
export const isOnline = readonly(online)

/** 地址栏伸缩本身不算软键盘，必须同时有可编辑元素获得焦点。 */
export function keyboardIsOpen(layoutHeight: number, visualHeight: number, editable: boolean, scale = 1) {
  return editable && scale < 1.1 && layoutHeight - visualHeight > Math.max(120, layoutHeight * .18)
}

export function installViewport() {
  const root = document.documentElement
  const display = window.matchMedia('(display-mode: standalone)')
  const pointer = window.matchMedia('(pointer: coarse)')
  const visual = window.visualViewport
  let frame = 0
  function update() {
    cancelAnimationFrame(frame)
    frame = requestAnimationFrame(() => {
      width.value = window.innerWidth
      coarsePointer.value = pointer.matches
      standalone.value = display.matches || (navigator as Navigator & { standalone?: boolean }).standalone === true
      online.value = navigator.onLine
      const active = document.activeElement
      const editable = active instanceof HTMLElement && (active.matches('input:not([type=checkbox]):not([type=radio]), textarea, select') || active.isContentEditable)
      keyboard.value = keyboardIsOpen(window.innerHeight, visual?.height ?? window.innerHeight, editable, visual?.scale)
      root.classList.toggle('keyboard-open', keyboard.value)
      root.classList.toggle('is-standalone', standalone.value)
      root.style.setProperty('--app-height', `${window.innerHeight}px`)
      root.style.setProperty('--visual-height', `${visual?.height ?? window.innerHeight}px`)
      root.style.setProperty('--visual-top', `${visual?.offsetTop ?? 0}px`)
    })
  }
  const events = ['resize', 'orientationchange', 'online', 'offline', 'pageshow']
  events.forEach((event) => window.addEventListener(event, update))
  document.addEventListener('focusin', update)
  document.addEventListener('focusout', update)
  visual?.addEventListener('resize', update)
  visual?.addEventListener('scroll', update)
  display.addEventListener('change', update)
  pointer.addEventListener('change', update)
  update()
  return () => {
    cancelAnimationFrame(frame)
    events.forEach((event) => window.removeEventListener(event, update))
    document.removeEventListener('focusin', update)
    document.removeEventListener('focusout', update)
    visual?.removeEventListener('resize', update)
    visual?.removeEventListener('scroll', update)
    display.removeEventListener('change', update)
    pointer.removeEventListener('change', update)
  }
}
