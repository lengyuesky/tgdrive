import { ref, type Ref } from 'vue'
import type { DirectoryEntry } from '../api'

/** 键盘导航所需的布局描述：行可以是分组标题（时间线），分组行不承载条目。 */
export interface NavLayout {
  count: number
  rowCount: number
  /** 每行列数为 1 时视为列表视图，左右键不移动。 */
  columns: number
  /** 一屏大约容纳的行数，用于 PageUp/PageDown。 */
  pageRows: number
  rowOf(index: number): number
  row(rowIndex: number): { start: number; count: number; group?: unknown } | undefined
}

export type NavKey = 'ArrowUp' | 'ArrowDown' | 'ArrowLeft' | 'ArrowRight' | 'Home' | 'End' | 'PageUp' | 'PageDown'
export const NAV_KEYS: ReadonlySet<string> = new Set<NavKey>(['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown'])

/** 从 from 行沿 step 方向找第一个承载条目的行；越界返回 -1。 */
function itemRow(layout: NavLayout, from: number, step: 1 | -1) {
  for (let r = from; r >= 0 && r < layout.rowCount; r += step) {
    const row = layout.row(r)
    if (row && !row.group && row.count > 0) return r
  }
  return -1
}

/** 计算按键后的新焦点下标；current < 0 表示尚无焦点。 */
export function nextIndex(current: number, key: NavKey, layout: NavLayout): number {
  const last = layout.count - 1
  if (last < 0) return -1
  if (key === 'Home') return 0
  if (key === 'End') return last
  if (current < 0 || current > last) return key === 'ArrowUp' || key === 'PageUp' ? last : 0
  if (key === 'ArrowLeft' || key === 'ArrowRight') {
    if (layout.columns <= 1) return current
    return Math.min(last, Math.max(0, current + (key === 'ArrowLeft' ? -1 : 1)))
  }
  const forward = key === 'ArrowDown' || key === 'PageDown'
  const distance = key === 'PageUp' || key === 'PageDown' ? Math.max(1, layout.pageRows) : 1
  const rowIndex = layout.rowOf(current)
  const column = current - (layout.row(rowIndex)?.start ?? current)
  // 逐个跨过条目行，分组标题行不计入步数。
  let target = rowIndex
  for (let moved = 0; moved < distance; moved++) {
    const next = itemRow(layout, target + (forward ? 1 : -1), forward ? 1 : -1)
    if (next < 0) break
    target = next
  }
  if (target === rowIndex) {
    // 已在首/末行：翻页键跳到首/尾，方向键向下到末项（最后一行不满时），向上保持不动。
    if (key === 'PageDown') return last
    if (key === 'PageUp') return 0
    return current
  }
  const row = layout.row(target)!
  return row.start + Math.min(column, row.count - 1)
}

/** 焦点与锚点之间（含两端）的条目 id。 */
export function rangeIds(entries: readonly DirectoryEntry[], anchor: number, focus: number): number[] {
  const from = Math.max(0, Math.min(anchor, focus))
  const to = Math.min(entries.length - 1, Math.max(anchor, focus))
  return entries.slice(from, to + 1).map((entry) => entry.id)
}

/** 目标是否是会自己消费按键的可编辑控件。 */
export function isEditableTarget(target: EventTarget | null) {
  const el = target as HTMLElement | null
  if (!el || !el.tagName) return false
  // 复选框不吃方向键，允许点完勾选框后继续用键盘导航（空格仍交给复选框本身）。
  if (el.tagName === 'INPUT' && (el as HTMLInputElement).type === 'checkbox') return false
  return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.isContentEditable === true
}

export interface KeyboardNavOptions {
  entries: Ref<readonly DirectoryEntry[]>
  indexOf: (id: number) => number | undefined
  layout: () => NavLayout | undefined
  reveal: (index: number) => void
  setSelection: (ids: Iterable<number>) => void
  toggle: (id: number) => void
  clearSelection: () => void
  open: (entry: DirectoryEntry) => void
  up: () => void
  remove: (entry: DirectoryEntry) => void
  rename: (entry: DirectoryEntry) => void
}

/** 文件列表键盘操作：焦点用 aria-activedescendant 表达，焦点本身留在列表容器上。 */
export function useKeyboardNav(options: KeyboardNavOptions) {
  const focusId = ref<number>()
  let anchorId: number | undefined
  const focusIndex = () => focusId.value === undefined ? -1 : options.indexOf(focusId.value) ?? -1
  const focused = () => { const i = focusIndex(); return i < 0 ? undefined : options.entries.value[i] }

  function setFocus(index: number, reveal = true) {
    const entry = options.entries.value[index]
    if (!entry) return
    focusId.value = entry.id
    if (reveal) options.reveal(index)
  }
  /** 鼠标点击同步焦点与范围锚点。 */
  function pointAt(entry: DirectoryEntry) {
    focusId.value = entry.id
    anchorId = entry.id
  }
  function reset() {
    focusId.value = undefined
    anchorId = undefined
  }

  function onKeydown(e: KeyboardEvent): boolean {
    if (e.defaultPrevented || isEditableTarget(e.target)) return false
    const mod = e.ctrlKey || e.metaKey
    const key = e.key
    if (NAV_KEYS.has(key) && !(key === 'ArrowUp' && e.altKey)) {
      const layout = options.layout()
      if (!layout) return false
      const current = focusIndex()
      const index = nextIndex(current, key as NavKey, layout)
      if (index < 0) return false
      e.preventDefault()
      setFocus(index)
      const id = options.entries.value[index]!.id
      if (e.shiftKey) {
        const anchor = anchorId === undefined ? -1 : options.indexOf(anchorId) ?? -1
        if (anchor < 0) anchorId = options.entries.value[Math.max(0, current)]?.id ?? id
        options.setSelection(rangeIds(options.entries.value, anchor < 0 ? Math.max(0, current) : anchor, index))
      } else if (!mod) {
        anchorId = id
        options.setSelection([id])
      }
      return true
    }
    if ((key === 'ArrowUp' && e.altKey) || (key === 'Backspace' && !mod && !e.altKey)) {
      e.preventDefault()
      options.up()
      return true
    }
    if (key === 'Escape') {
      options.clearSelection()
      return false // 不吞掉，让全局 Esc（关闭右键菜单）继续执行
    }
    const entry = focused()
    if (key === ' ' || key === 'Spacebar') {
      if ((e.target as HTMLElement | null)?.tagName === 'INPUT') return false
      e.preventDefault()
      if (entry) { options.toggle(entry.id); anchorId = entry.id } else setFocus(0)
      return true
    }
    if (!entry) return false
    if (key === 'Enter' && !mod && !e.altKey) { e.preventDefault(); options.open(entry); return true }
    if (key === 'Delete') { e.preventDefault(); options.remove(entry); return true }
    if (key === 'F2') { e.preventDefault(); options.rename(entry); return true }
    return false
  }
  return { focusId, onKeydown, pointAt, reset, setFocus }
}
