import { defineComponent } from 'vue'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { invalidateSession } from '../session'
import { createTransferController, type TransferController } from './controller'
import type { UploadSession } from '../api'

const mocks = vi.hoisted(() => ({
  exists: vi.fn(), mkdir: vi.fn(), instant: vi.fn(),
  uploads: { list: vi.fn(), create: vi.fn(), status: vi.fn(), part: vi.fn(), complete: vi.fn(), cancel: vi.fn(), thumbnail: vi.fn() },
}))
vi.mock('../api', () => ({ api: mocks, errMsg: (error: Error) => error.message, fmtSize: String }))
vi.mock('../dialog', () => ({ appConfirm: vi.fn().mockResolvedValue(undefined) }))
vi.mock('element-plus', () => ({ ElMessage: { info: vi.fn(), success: vi.fn(), warning: vi.fn() } }))

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}
function session(): UploadSession {
  return { id: '测试会话', name: '文件.txt', path: '/固定目录/文件.txt', size: 6, sha256: '测试摘要', chunk_size: 6, part_count: 1, uploaded_parts: [], uploaded_bytes: 0, created_at: 1, updated_at: 1 }
}
const response = (sessions: UploadSession[] = []) => ({ data: { sessions, concurrency: 2, bot_count: 1, retry_after: 0 } })
const uploadResponse = (value = session()) => ({ data: { session: value, concurrency: 2, bot_count: 1, retry_after: 0 } })
const file = () => new File(['测试'], '文件.txt', { type: 'text/plain' })

let automaticHash = true
const workers: TestWorker[] = []
class TestWorker {
  onmessage: ((event: MessageEvent) => void) | null = null
  onerror: (() => void) | null = null
  terminated = false
  constructor() { workers.push(this) }
  postMessage = vi.fn((message: { type: string; id: number }) => {
    if (message.type === 'hash' && automaticHash) queueMicrotask(() => {
      if (!this.terminated) this.onmessage?.({ data: { type: 'done', id: message.id, digest: '测试摘要' } } as MessageEvent)
    })
  })
  terminate = vi.fn(() => { this.terminated = true })
}
const wrappers: VueWrapper[] = []
function setup() {
  let queue!: TransferController
  wrappers.push(mount(defineComponent({ setup() { queue = createTransferController(); return () => null } })))
  return queue
}

beforeEach(() => {
  vi.resetAllMocks()
  vi.stubGlobal('Worker', TestWorker)
  automaticHash = true
  workers.length = 0
  mocks.exists.mockResolvedValue({ data: { existing: [] } })
  mocks.mkdir.mockResolvedValue({ data: { ok: true } })
  mocks.instant.mockResolvedValue({ data: { linked: true, pending: false } })
  mocks.uploads.list.mockResolvedValue(response())
  mocks.uploads.create.mockResolvedValue(uploadResponse())
  mocks.uploads.status.mockResolvedValue(uploadResponse())
  mocks.uploads.part.mockResolvedValue({ data: { ok: true } })
  mocks.uploads.complete.mockResolvedValue({ data: { ok: true } })
  mocks.uploads.cancel.mockResolvedValue({ data: { ok: true } })
})
afterEach(() => {
  for (const wrapper of wrappers.splice(0)) wrapper.unmount()
})
afterAll(() => vi.unstubAllGlobals())

describe('登录会话级传输生命周期', () => {
  it('401 同步终止哈希、释放文件和等待容量，不删除服务器续传任务', async () => {
    automaticHash = false
    const queue = setup()
    await flushPromises()
    await queue.enqueue('/固定目录', [{ file: file() }], 'overwrite')
    await vi.waitFor(() => expect(workers[0]!.postMessage).toHaveBeenCalled())
    const item = queue.displayedItems.value[0]!
    expect(item.file).toBeDefined()
    const capacity = queue.waitForCapacity(600)
    const cancelled = vi.fn()
    queue.onCancelled(cancelled)

    invalidateSession()
    expect(item.file).toBeUndefined()
    expect(item.thumbnail).toBeUndefined()
    expect(queue.totalCount.value).toBe(0)
    expect(queue.displayedItems.value).toEqual([])
    expect(queue.visible.value).toBe(false)
    expect(workers[0]!.terminate).toHaveBeenCalledOnce()
    expect(cancelled).toHaveBeenCalledOnce()
    await expect(capacity).resolves.toBe(false)
    await flushPromises()
    expect(mocks.instant).not.toHaveBeenCalled()
    expect(mocks.uploads.cancel).not.toHaveBeenCalled()
    await expect(queue.enqueue('/旧目录', [{ file: file() }])).resolves.toBe('cancel')
    expect(queue.totalCount.value).toBe(0)
  })

  it('失效会话的迟到恢复响应不能重新显示上传队列', async () => {
    const pending = deferred<ReturnType<typeof response>>()
    mocks.uploads.list.mockReturnValue(pending.promise)
    const queue = setup()
    const signal = mocks.uploads.list.mock.calls[0]![0] as AbortSignal
    invalidateSession()
    expect(signal.aborted).toBe(true)
    pending.resolve(response([session()]))
    await flushPromises()
    expect(queue.totalCount.value).toBe(0)
    expect(queue.visible.value).toBe(false)
    expect(mocks.uploads.complete).not.toHaveBeenCalled()
  })

  it('创建会话期间注销，迟到响应保留在服务器但不继续上传或自动删除', async () => {
    const pending = deferred<ReturnType<typeof uploadResponse>>()
    mocks.instant.mockResolvedValue({ data: { linked: false, pending: false } })
    mocks.uploads.create.mockReturnValue(pending.promise)
    const queue = setup()
    await flushPromises()
    await queue.enqueue('/固定目录', [{ file: file() }], 'overwrite')
    await vi.waitFor(() => expect(mocks.uploads.create).toHaveBeenCalledOnce())
    const signal = mocks.uploads.create.mock.calls[0]![4] as AbortSignal
    invalidateSession()
    expect(signal.aborted).toBe(true)
    pending.resolve(uploadResponse())
    await flushPromises()
    expect(queue.totalCount.value).toBe(0)
    expect(mocks.uploads.part).not.toHaveBeenCalled()
    expect(mocks.uploads.cancel).not.toHaveBeenCalled()
  })

  it('正在发送的分块会被中止，迟到成功响应不能触发完成写入', async () => {
    const pending = deferred<{ data: { ok: boolean } }>()
    mocks.instant.mockResolvedValue({ data: { linked: false, pending: false } })
    mocks.uploads.part.mockReturnValue(pending.promise)
    const queue = setup()
    await flushPromises()
    await queue.enqueue('/固定目录', [{ file: file() }], 'overwrite')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    const signal = mocks.uploads.part.mock.calls[0]![4] as AbortSignal
    const item = queue.displayedItems.value[0]!
    invalidateSession()
    expect(signal.aborted).toBe(true)
    expect(item.controllers.size).toBe(0)
    expect(item.file).toBeUndefined()
    pending.resolve({ data: { ok: true } })
    await flushPromises()
    expect(mocks.uploads.complete).not.toHaveBeenCalled()
    expect(mocks.uploads.cancel).not.toHaveBeenCalled()
    expect(queue.totalCount.value).toBe(0)
  })

  it('查重期间认证失效，不遗留冲突框或继续入队', async () => {
    const pending = deferred<{ data: { existing: string[] } }>()
    mocks.exists.mockReturnValue(pending.promise)
    const queue = setup()
    await flushPromises()
    const enqueue = queue.enqueue('/固定目录', [{ file: file() }])
    const signal = mocks.exists.mock.calls[0]![2] as AbortSignal
    invalidateSession()
    expect(signal.aborted).toBe(true)
    pending.resolve({ data: { existing: ['文件.txt'] } })
    await expect(enqueue).resolves.toBe('cancel')
    expect(queue.conflictVisible.value).toBe(false)
    expect(queue.preparingFileCount.value).toBe(0)
    expect(queue.totalCount.value).toBe(0)
  })

  it('完成通知固定使用入队目录，注销后统计也归零', async () => {
    const queue = setup()
    const uploaded = vi.fn()
    queue.onUploaded(uploaded)
    await flushPromises()
    await queue.enqueue('/固定目录', [{ file: file(), rel: '子目录' }], 'overwrite')
    await vi.waitFor(() => expect(queue.successCount.value).toBe(1))
    expect(uploaded).toHaveBeenCalledWith('/固定目录/子目录')
    expect(mocks.instant.mock.calls[0]![0]).toBe('/固定目录/子目录')
    expect(queue.recentSuccessCount.value).toBe(1)
    expect(queue.displayedItems.value[0]!.file).toBeUndefined()
    invalidateSession()
    expect(queue.successCount.value).toBe(0)
    expect(queue.recentSuccessCount.value).toBe(0)
  })

  it('新登录的控制器重新恢复服务器任务，不复用旧 File 或已中止的信号', async () => {
    const previous = setup()
    await flushPromises()
    invalidateSession()
    mocks.uploads.list.mockResolvedValue(response([session()]))
    const current = setup()
    await flushPromises()
    expect(previous.totalCount.value).toBe(0)
    expect(current.needsFileCount.value).toBe(1)
    expect(current.displayedItems.value[0]!.file).toBeUndefined()
    expect(current.displayedItems.value[0]!.error).toContain('重新选择原文件')
    expect((mocks.uploads.list.mock.calls.at(-1)![0] as AbortSignal).aborted).toBe(false)
    expect(workers[1]!.terminate).not.toHaveBeenCalled()
  })
})
