import { describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'
import { isEditableTarget, nextIndex, rangeIds, useKeyboardNav, type NavLayout } from './keyboardNav'
import { TimelineRows } from './layout'
import { entry } from '../test/fixtures'

function grid(count: number, columns: number, pageRows = 2): NavLayout {
  const rowCount = Math.ceil(count / columns)
  return {
    count, rowCount, columns, pageRows,
    rowOf: (i) => Math.floor(i / columns),
    row: (r) => r >= 0 && r < rowCount ? { start: r * columns, count: Math.min(columns, count - r * columns) } : undefined,
  }
}

describe('键盘导航纯逻辑', () => {
  it('列表视图上下单步、左右不动、首尾与翻页', () => {
    const list = grid(10, 1, 3)
    expect(nextIndex(-1, 'ArrowDown', list)).toBe(0)
    expect(nextIndex(-1, 'ArrowUp', list)).toBe(9)
    expect(nextIndex(4, 'ArrowDown', list)).toBe(5)
    expect(nextIndex(0, 'ArrowUp', list)).toBe(0)
    expect(nextIndex(9, 'ArrowDown', list)).toBe(9)
    expect(nextIndex(4, 'ArrowLeft', list)).toBe(4)
    expect(nextIndex(4, 'Home', list)).toBe(0)
    expect(nextIndex(4, 'End', list)).toBe(9)
    expect(nextIndex(4, 'PageDown', list)).toBe(7)
    expect(nextIndex(8, 'PageDown', list)).toBe(9)
    expect(nextIndex(1, 'PageUp', list)).toBe(0)
    expect(nextIndex(0, 'ArrowDown', grid(0, 1))).toBe(-1)
  })

  it('网格视图左右单项、上下按列数，末行不满时夹到末项', () => {
    const g = grid(10, 4)
    expect(nextIndex(1, 'ArrowRight', g)).toBe(2)
    expect(nextIndex(3, 'ArrowRight', g)).toBe(4)
    expect(nextIndex(0, 'ArrowLeft', g)).toBe(0)
    expect(nextIndex(9, 'ArrowRight', g)).toBe(9)
    expect(nextIndex(1, 'ArrowDown', g)).toBe(5)
    expect(nextIndex(7, 'ArrowDown', g)).toBe(9)
    expect(nextIndex(9, 'ArrowUp', g)).toBe(5)
    expect(nextIndex(2, 'ArrowUp', g)).toBe(2)
  })

  it('时间线跳过日期标题行', () => {
    const entries = [entry(0), entry(1), entry(2), entry(3, 1600000000), entry(4, 1600000000)]
    const t = new TimelineRows(2)
    t.append(entries)
    const layout: NavLayout = {
      count: entries.length, rowCount: t.rows.length, columns: 2, pageRows: 5,
      rowOf: (i) => t.rowOf(i), row: (r) => t.rows[r],
    }
    expect(nextIndex(0, 'ArrowDown', layout)).toBe(2)
    expect(nextIndex(3, 'ArrowUp', layout)).toBe(2)
    expect(nextIndex(2, 'ArrowDown', layout)).toBe(3)
  })

  it('范围选择与可编辑目标判定', () => {
    const entries = Array.from({ length: 5 }, (_, i) => entry(i))
    expect(rangeIds(entries, 3, 1)).toEqual([entries[1]!.id, entries[2]!.id, entries[3]!.id])
    const input = document.createElement('input')
    expect(isEditableTarget(input)).toBe(true)
    input.type = 'checkbox'
    expect(isEditableTarget(input)).toBe(false)
    expect(isEditableTarget(document.createElement('div'))).toBe(false)
  })
})

describe('useKeyboardNav', () => {
  function setup() {
    const entries = ref(Array.from({ length: 6 }, (_, i) => entry(i)))
    let selection = new Set<number>()
    const calls = { open: vi.fn(), up: vi.fn(), remove: vi.fn(), rename: vi.fn(), reveal: vi.fn() }
    const nav = useKeyboardNav({
      entries, indexOf: (id) => { const i = entries.value.findIndex((e) => e.id === id); return i < 0 ? undefined : i },
      layout: () => grid(6, 3), setSelection: (ids) => { selection = new Set(ids) },
      toggle: (id) => { if (!selection.delete(id)) selection.add(id) }, clearSelection: () => { selection = new Set() },
      ...calls,
    })
    const press = (key: string, init: KeyboardEventInit = {}) => nav.onKeydown(new KeyboardEvent('keydown', { key, cancelable: true, ...init }))
    return { entries, nav, press, calls, sel: () => [...selection].sort((a, b) => a - b) }
  }
  const id = (i: number) => entry(i).id

  it('方向键单选、Shift 范围、Ctrl 只移焦点、空格切换、Esc 清空', () => {
    const { nav, press, sel, calls } = setup()
    press('ArrowRight')
    expect(nav.focusId.value).toBe(id(0))
    expect(sel()).toEqual([id(0)])
    press('ArrowDown', { shiftKey: true })
    expect(sel()).toEqual([0, 1, 2, 3].map(id))
    press('ArrowRight', { ctrlKey: true })
    expect(nav.focusId.value).toBe(id(4))
    expect(sel()).toEqual([0, 1, 2, 3].map(id))
    press(' ')
    expect(sel()).toEqual([0, 1, 2, 3, 4].map(id))
    expect(calls.reveal).toHaveBeenLastCalledWith(4)
    press('Escape')
    expect(sel()).toEqual([])
  })

  it('回车打开、退格与 Alt+↑ 上级、Delete 删除、F2 重命名，输入框内不拦截', () => {
    const { nav, press, calls } = setup()
    expect(press('Enter')).toBe(false)
    nav.pointAt(entry(2))
    press('Enter')
    expect(calls.open).toHaveBeenCalledWith(expect.objectContaining({ id: id(2) }))
    press('Backspace'); press('ArrowUp', { altKey: true })
    expect(calls.up).toHaveBeenCalledTimes(2)
    press('Delete'); press('F2')
    expect(calls.remove).toHaveBeenCalledTimes(1)
    expect(calls.rename).toHaveBeenCalledTimes(1)
    const input = document.createElement('input')
    document.body.append(input)
    const event = new KeyboardEvent('keydown', { key: 'Backspace', cancelable: true, bubbles: true })
    input.addEventListener('keydown', (e) => nav.onKeydown(e))
    input.dispatchEvent(event)
    expect(calls.up).toHaveBeenCalledTimes(2)
    input.remove()
  })
})
