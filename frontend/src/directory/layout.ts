import type { DirectoryEntry } from '../api'

export type DirectoryView = 'list' | 'grid' | 'photo' | 'timeline'
export interface TimelineGroup { label: string; count: number }
export interface DirectoryRow { start: number; count: number; group?: TimelineGroup }

/** 只增量组织新文件；同一天跨批次共享组头和计数，不重新排序已加载文件。 */
export class TimelineRows {
  rows: DirectoryRow[] = []
  processed = 0
  private group?: TimelineGroup
  constructor(readonly columns: number) {}
  append(entries: readonly DirectoryEntry[]) {
    for (; this.processed < entries.length; this.processed++) {
      const entry = entries[this.processed]!
      const label = new Date(entry.modified_at * 1000).toLocaleDateString('zh-CN')
      if (this.group?.label !== label) {
        this.group = { label, count: 0 }
        this.rows.push({ start: this.processed, count: 0, group: this.group })
      }
      this.group.count++
      const tail = this.rows[this.rows.length - 1]!
      if (tail.group || tail.count === this.columns) {
        this.rows.push({ start: this.processed, count: 1 })
      } else tail.count++
    }
  }
  rowOf(index: number) {
    let low = 0
    let high = this.rows.length - 1
    while (low < high) {
      const middle = Math.ceil((low + high) / 2)
      if (this.rows[middle]!.start <= index) low = middle
      else high = middle - 1
    }
    return low
  }
}

/** 一屏前后一屏缓冲；超宽屏也不能突破 200 个文件组件。 */
export function boundedRange(start: number, end: number, count: number, buffer: number, maxRows: number) {
  const visible = end - start + 1
  const spare = Math.max(0, maxRows - visible)
  const before = Math.min(buffer, Math.floor(spare / 2), start)
  const after = Math.min(buffer, spare - before)
  const from = Math.max(0, start - before)
  const to = Math.min(count - 1, end + after, from + maxRows - 1)
  return Array.from({ length: Math.max(0, to - from + 1) }, (_, i) => from + i)
}
