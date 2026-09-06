import { describe, expect, it, vi } from 'vitest'
import { deleteBatches, freezeTargets, moveSerial } from './operations'
import { entry } from '../test/fixtures'

describe('冻结的批操作', () => {
  it('路径和条目在弹窗/切目录之前冻结，删除201项分200/1串行并报告失败', async () => {
    const rows = Array.from({ length: 201 }, (_, i) => entry(i))
    const targets = freezeTargets('/original', rows)
    rows[0]!.name = 'changed'
    expect(targets[0]!.path).toBe('/original/0.jpg')
    expect(targets[0]!.entry.name).toBe('0.jpg')
    let active = 0
    const remove = vi.fn(async (paths: string[]) => {
      expect(++active).toBe(1)
      await Promise.resolve()
      active--
      return { failed: paths.length === 1 ? [{ path: paths[0]!, error: '失败' }] : [] }
    })
    const progress = vi.fn()
    const failed = await deleteBatches(targets, remove, progress)
    expect(remove.mock.calls.map(([paths]) => paths.length)).toEqual([200, 1])
    expect(failed).toEqual([{ path: '/original/200.jpg', error: '失败' }])
    expect(progress).toHaveBeenLastCalledWith({ completed: 201, total: 201, failed: 1 })
  })

  it('网络失败记录整批，继续后批，不漏报', async () => {
    const targets = freezeTargets('/', Array.from({ length: 401 }, (_, i) => entry(i)))
    const remove = vi.fn().mockRejectedValueOnce(new Error('断网')).mockResolvedValue({ failed: [] })
    const progress = vi.fn()
    expect(await deleteBatches(targets, remove, progress)).toHaveLength(200)
    expect(remove.mock.calls.map(([paths]) => paths.length)).toEqual([200, 200, 1])
    expect(progress).toHaveBeenLastCalledWith({ completed: 401, total: 401, failed: 200 })
  })

  it('移动逐项串行，始终使用原路径', async () => {
    const targets = freezeTargets('/before', [entry(1), entry(2)])
    const move = vi.fn().mockRejectedValueOnce('同名').mockResolvedValue(undefined)
    const progress = vi.fn()
    const failed = await moveSerial(targets, '/target', move, progress)
    expect(move.mock.calls).toEqual([['/before/1.jpg', '/target/1.jpg'], ['/before/2.jpg', '/target/2.jpg']])
    expect(failed).toEqual([{ path: '/before/1.jpg', error: '同名' }])
    expect(progress).toHaveBeenLastCalledWith({ completed: 2, total: 2, failed: 1 })
  })
})
