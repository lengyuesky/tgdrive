import { describe, expect, it } from 'vitest'
import { boundedRange, TimelineRows } from './layout'
import { entry } from '../test/fixtures'

describe('目录虚拟行布局', () => {
  it('时间线同一天跨200边界只保留一个组头，续满最后一行', () => {
    const entries = Array.from({ length: 200 }, (_, i) => entry(i))
    const timeline = new TimelineRows(3)
    timeline.append(entries)
    const head = timeline.rows[0]!.group
    const existing = timeline.rows
    entries.push(entry(200), entry(201, 1600000000))
    timeline.append(entries)
    expect(timeline.rows).toBe(existing)
    expect(timeline.rows.filter((row) => row.group)).toHaveLength(2)
    expect(head!.count).toBe(201)
    expect(timeline.rows[67]!.count).toBe(3)
    expect(timeline.rows[timeline.rowOf(201)]!.start).toBe(201)
    expect(timeline.processed).toBe(202)
  })

  it('所有视图最大720px高、最多20列时保留全部可见行且不超过200文件', () => {
    for (const rowHeight of [56, 192, 272]) {
      for (let columns = 1; columns <= (rowHeight === 56 ? 1 : 20); columns++) {
        const visible = Math.ceil(720 / rowHeight) + 1
        const result = boundedRange(500, 500 + visible - 1, 10000, Math.ceil(720 / rowHeight), Math.floor(200 / columns))
        for (let row = 500; row < 500 + visible; row++) expect(result).toContain(row)
        expect(result.length * columns).toBeLessThanOrEqual(200)
      }
    }
  })

  it('头尾夹紧且通常前后各一视口缓冲', () => {
    expect(boundedRange(0, 9, 15, 10, 200)).toEqual(Array.from({ length: 15 }, (_, i) => i))
    expect(boundedRange(100, 109, 1000, 10, 200)).toEqual(Array.from({ length: 30 }, (_, i) => i + 90))
    expect(boundedRange(990, 999, 1000, 10, 200)).toHaveLength(20)
  })
})
