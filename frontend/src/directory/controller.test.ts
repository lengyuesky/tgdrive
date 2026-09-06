import { describe, expect, it, vi } from 'vitest'
import type { EntriesResponse } from '../api'
import { createDirectoryController, type DirectoryConditions } from './controller'

import { entry } from '../test/fixtures'

const conditions: DirectoryConditions = { path: '/', sort: 'name', order: 'asc', kind: 'all', q: '' }
function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
function page(ids: number[], cursor: string | null = null, total?: number): EntriesResponse {
  return { path: '/', entries: ids.map((id) => entry(id)), has_more: cursor !== null, next_cursor: cursor, total }
}

describe('目录控制器', () => {
  it('首次固定200，续批保留total、浅数组原位追加、ID去重且结束不循环', async () => {
    const fetch = vi.fn().mockResolvedValueOnce(page([1, 2], 'next', 3)).mockResolvedValueOnce(page([2, 3]))
    const directory = createDirectoryController(fetch)
    await directory.reset(conditions)
    const original = directory.entries.value
    expect(fetch.mock.calls[0]![0]).toEqual({ ...conditions, limit: 200, cursor: undefined })
    directory.selectAll(true)
    await directory.loadMore()
    expect(directory.entries.value).toBe(original)
    expect(directory.entries.value.map((item) => item.id)).toEqual([1, 2, 3])
    expect(directory.batches.value.map((batch) => batch.length)).toEqual([2, 1])
    expect(directory.total.value).toBe(3)
    expect(directory.selectedIds.value).toEqual(new Set([1, 2]))
    await directory.loadMore()
    expect(fetch).toHaveBeenCalledTimes(2)
  })

  it('失败保留数据，只有显式重试才重发同一cursor，每代最多一个请求', async () => {
    const pending = deferred<EntriesResponse>()
    const fetch = vi.fn().mockResolvedValueOnce(page([1], 'next', 2)).mockReturnValueOnce(pending.promise).mockResolvedValueOnce(page([2]))
    const directory = createDirectoryController(fetch)
    await directory.reset(conditions)
    const loading = directory.loadMore()
    await directory.loadMore()
    expect(fetch).toHaveBeenCalledTimes(2)
    pending.reject(new Error('网络中断'))
    await loading
    expect(directory.entries.value).toHaveLength(1)
    await directory.loadMore()
    expect(fetch).toHaveBeenCalledTimes(2)
    await directory.loadMore(true)
    expect(fetch.mock.calls[1]![0]).toEqual(fetch.mock.calls[2]![0])
    expect(directory.entries.value).toHaveLength(2)
    expect(directory.error.value).toBeNull()
  })

  it.each(['resolve', 'reject'] as const)('旧%s及finally不能污染新代状态', async (completion) => {
    const old = deferred<EntriesResponse>()
    const current = deferred<EntriesResponse>()
    const fetch = vi.fn().mockReturnValueOnce(old.promise).mockReturnValueOnce(current.promise)
    const directory = createDirectoryController(fetch)
    const first = directory.reset(conditions)
    const second = directory.reset({ ...conditions, path: '/new', kind: 'file', sort: 'mtime', order: 'desc' })
    expect(fetch.mock.calls[0]![1].aborted).toBe(true)
    if (completion === 'resolve') old.resolve(page([1]))
    else old.reject(new Error('旧请求失败'))
    await first
    expect(directory.loading.value).toBe(true)
    expect(directory.entries.value).toEqual([])
    expect(directory.error.value).toBeNull()
    current.resolve(page([9]))
    await second
    expect(directory.entries.value.map((item) => item.id)).toEqual([9])
    expect(directory.loading.value).toBe(false)
  })

  it('过滤输入可立即换代停止旧请求但延后首批，销毁阻止晚到响应', async () => {
    const old = deferred<EntriesResponse>()
    const fetch = vi.fn().mockReturnValue(old.promise)
    const directory = createDirectoryController(fetch)
    const request = directory.reset(conditions)
    directory.reset({ ...conditions, q: '%_' }, false)
    expect(fetch.mock.calls[0]![1].aborted).toBe(true)
    expect(fetch).toHaveBeenCalledTimes(1)
    directory.dispose()
    old.resolve(page([1]))
    await request
    await directory.loadMore()
    expect(directory.entries.value).toHaveLength(0)
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it.each([{ ids: [] }, { ids: [1] }])('空或全重复续批停住自动加载，不在后台遍历', async ({ ids }) => {
    const fetch = vi.fn().mockResolvedValueOnce(page([1], 'a')).mockResolvedValueOnce(page(ids, 'b')).mockResolvedValueOnce(page([2]))
    const directory = createDirectoryController(fetch)
    await directory.reset(conditions)
    await directory.loadMore()
    for (let i = 0; i < 10; i++) await directory.loadMore()
    expect(fetch).toHaveBeenCalledTimes(2)
    expect(directory.error.value).toBeInstanceOf(Error)
    await directory.loadMore(true)
    expect(fetch.mock.calls[2]![0].cursor).toBe('b')
  })

  it('无前进游标保留旧批次并显示错误', async () => {
    const fetch = vi.fn().mockResolvedValueOnce(page([1], 'a')).mockResolvedValueOnce(page([2], 'a'))
    const directory = createDirectoryController(fetch)
    await directory.reset(conditions)
    await directory.loadMore()
    expect(directory.entries.value.map((item) => item.id)).toEqual([1])
    expect(directory.error.value).toBeInstanceOf(Error)
  })

  it('选择是一次Set替换，跨滚动保持，收藏本地更新，刷新清选择', async () => {
    const directory = createDirectoryController(vi.fn().mockResolvedValue(page([1, 2])))
    await directory.reset(conditions)
    directory.toggle(1)
    directory.toggle(999)
    expect(directory.selected.value.map((item) => item.id)).toEqual([1])
    const previous = directory.selectedIds.value
    directory.selectAll(true)
    expect(directory.selectedIds.value).not.toBe(previous)
    directory.updateFavorite(1, true)
    expect(directory.entries.value[0]!.favorite).toBe(true)
    expect(directory.revision.value).toBe(1)
    await directory.reset(conditions)
    expect(directory.selectedIds.value.size).toBe(0)
  })
})
