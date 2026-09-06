import { flushPromises } from '@vue/test-utils'
import { describe, expect, it, vi } from 'vitest'
import { createDirectoryTree } from './tree'
import { entry } from '../test/fixtures'
import type { EntriesQuery, EntriesResponse } from '../api'

describe('目录树游标', () => {
  it('只请求kind=dir、每批200，不自动加载更多；展开懒载，更多行不是目标', async () => {
    const fetch = vi.fn(async (query: EntriesQuery): Promise<EntriesResponse> => ({
      path: query.path, entries: [{ ...entry(query.path === '/' ? query.cursor ? 2 : 1 : 3), is_dir: true }],
      next_cursor: query.path === '/' && !query.cursor ? 'next' : null,
      has_more: query.path === '/' && !query.cursor,
    }))
    const tree = createDirectoryTree(fetch)
    tree.start()
    await flushPromises()
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(fetch.mock.calls[0]![0]).toEqual({ path: '/', kind: 'dir', limit: 200, sort: 'name', order: 'asc', q: '', cursor: undefined })
    expect(tree.rows.value.map((row) => row.type)).toEqual(['dir', 'more'])
    tree.toggle('/1.jpg')
    await flushPromises()
    expect(fetch.mock.calls[1]![0].path).toBe('/1.jpg')
    expect(tree.rows.value.map((row) => row.path)).toEqual(['/1.jpg', '/1.jpg/3.jpg', '/'])
    await tree.loadMore('/')
    expect(tree.rows.value.map((row) => row.path)).toEqual(['/1.jpg', '/1.jpg/3.jpg', '/2.jpg'])
    tree.toggle('/1.jpg')
    expect(tree.rows.value.map((row) => row.path)).toEqual(['/1.jpg', '/2.jpg'])
    tree.dispose()
  })

  it('收起及销毁取消请求，晚到响应不会让后代重新出现', async () => {
    let finish!: (response: EntriesResponse) => void
    const fetch = vi.fn().mockResolvedValueOnce({ path: '/', entries: [{ ...entry(1), is_dir: true }], has_more: false, next_cursor: null })
      .mockImplementationOnce(() => new Promise<EntriesResponse>((resolve) => { finish = resolve }))
    const tree = createDirectoryTree(fetch)
    tree.start()
    await flushPromises()
    tree.toggle('/1.jpg')
    tree.toggle('/1.jpg')
    expect(fetch.mock.calls[1]![1].aborted).toBe(true)
    finish({ path: '/1.jpg', entries: [{ ...entry(3), is_dir: true }], has_more: false, next_cursor: null })
    await flushPromises()
    expect(tree.rows.value.map((row) => row.path)).toEqual(['/1.jpg'])
    tree.dispose()
  })
})
