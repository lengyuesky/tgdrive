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

// ---------- 队列控制：暂停/续传、重试、并发槽位、停滞检测、取消、延迟计数 ----------

/** 按文件名生成独立会话，status 按 ID 回显，避免多个任务共享同一会话。 */
const serverSessions = new Map<string, UploadSession>()
let serverConcurrency = 2
function namedSession(name: string, extra: Partial<UploadSession> = {}): UploadSession {
  const value = { ...session(), id: `会话-${name}`, name, path: `/固定目录/${name}`, ...extra }
  serverSessions.set(value.id, value)
  return value
}
const sessionResponse = (value: UploadSession) => ({ data: { session: value, concurrency: serverConcurrency, bot_count: 1, retry_after: 0 } })
const namedFile = (name: string) => new File(['测试'], name, { type: 'text/plain' })
/** 永不自行完成的分块请求，只在收到中止信号时拒绝。 */
function hangingPart() {
  return (_id: string, _index: number, _data: Blob, _progress: (loaded: number) => void, signal: AbortSignal) =>
    new Promise((_resolve, reject) => {
      signal.addEventListener('abort', () => reject(new Error('已中止')))
    })
}
function useNamedSessions() {
  serverSessions.clear()
  serverConcurrency = 2
  mocks.instant.mockResolvedValue({ data: { linked: false, pending: false } })
  mocks.uploads.create.mockImplementation(async (_dir: string, name: string) => sessionResponse(namedSession(name)))
  mocks.uploads.status.mockImplementation(async (id: string) => sessionResponse(serverSessions.get(id)!))
}
async function enqueueOne(queue: TransferController, name: string) {
  await queue.enqueue('/固定目录', [{ file: namedFile(name) }], 'overwrite')
  return queue.displayedItems.value.find((item) => item.name === name)!
}

describe('暂停与续传', () => {
  beforeEach(useNamedSessions)

  it('暂停中止进行中的分块并保留会话，继续后重新上传并完成', async () => {
    mocks.uploads.part.mockImplementationOnce(hangingPart())
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '甲.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    const signal = mocks.uploads.part.mock.calls[0]![4] as AbortSignal
    expect(queue.isPauseable(item)).toBe(true)

    queue.pause(item)
    expect(signal.aborted).toBe(true)
    expect(item.status).toBe('paused')
    expect(queue.pausedCount.value).toBe(1)
    await flushPromises()
    expect(item.status).toBe('paused')
    expect(item.error).toBeUndefined()
    expect(mocks.uploads.cancel).not.toHaveBeenCalled()

    queue.resume(item)
    await vi.waitFor(() => expect(item.status).toBe('done'))
    expect(mocks.uploads.part).toHaveBeenCalledTimes(2)
    // 续传沿用已有会话，不再重复创建
    expect(mocks.uploads.create).toHaveBeenCalledOnce()
    expect(mocks.uploads.complete).toHaveBeenCalledWith('会话-甲.txt', expect.any(AbortSignal))
    expect(queue.pausedCount.value).toBe(0)
  })

  it('pauseAll / resumeAll 批量处理可暂停任务', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '乙.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    queue.pauseAll()
    expect(item.status).toBe('paused')
    await flushPromises()
    mocks.uploads.part.mockResolvedValue({ data: { ok: true } })
    queue.resumeAll()
    await vi.waitFor(() => expect(item.status).toBe('done'))
  })
})

describe('失败重试', () => {
  beforeEach(useNamedSessions)

  it('分块失败后标记错误，单个重试可以完成', async () => {
    mocks.uploads.part.mockRejectedValueOnce(new Error('网络错误'))
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '丙.txt')
    await vi.waitFor(() => expect(item.status).toBe('error'))
    expect(item.error).toBe('网络错误')
    expect(queue.errorCount.value).toBe(1)
    expect(queue.retryableCount.value).toBe(1)

    queue.retry(item)
    await vi.waitFor(() => expect(item.status).toBe('done'))
    expect(item.error).toBeUndefined()
    expect(queue.retryableCount.value).toBe(0)
  })

  it('retryAll 只处理无需重新选择文件的失败项，retryableCount 与之一致', async () => {
    // 已恢复的会话：一个分块已全部上传但完成请求失败，另一个仍缺分块（无本地文件）
    const complete = namedSession('完整.txt', { uploaded_parts: [0], uploaded_bytes: 6 })
    const partial = namedSession('残缺.txt')
    mocks.uploads.list.mockResolvedValue({ data: { sessions: [complete, partial], concurrency: 2, bot_count: 1, retry_after: 0 } })
    mocks.uploads.complete.mockRejectedValueOnce(new Error('完成失败'))
    mocks.uploads.part.mockRejectedValueOnce(new Error('分块失败'))
    const queue = setup()
    await flushPromises()
    const find = (name: string) => queue.displayedItems.value.find((item) => item.name === name)!
    await vi.waitFor(() => expect(find('完整.txt').status).toBe('error'))
    const local = await enqueueOne(queue, '本地.txt')
    await vi.waitFor(() => expect(local.status).toBe('error'))
    // 取消失败让缺文件的会话也进入 error，但它不能被「全部重试」处理
    mocks.uploads.cancel.mockRejectedValueOnce(Object.assign(new Error('服务器错误'), { response: { status: 500 } }))
    await expect(queue.cancelItem(find('残缺.txt'))).resolves.toBe(false)
    const orphan = find('残缺.txt')
    expect(orphan.status).toBe('error')
    expect(orphan.file).toBeUndefined()

    expect(queue.errorCount.value).toBe(3)
    expect(queue.retryableCount.value).toBe(2)
    const click = vi.spyOn(HTMLInputElement.prototype, 'click').mockImplementation(() => {})
    queue.retryAll()
    await vi.waitFor(() => expect(find('完整.txt').status).toBe('done'))
    await vi.waitFor(() => expect(local.status).toBe('done'))
    expect(orphan.status).toBe('error')
    expect(click).not.toHaveBeenCalled()
    expect(queue.errorCount.value).toBe(1)
    expect(queue.retryableCount.value).toBe(0)
    click.mockRestore()
  })
})

describe('全局上传并发槽位', () => {
  beforeEach(useNamedSessions)

  it('多个文件共享槽位，服务端下调/上调并发后立即生效', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises() // 初始 list 并发为 2，文件管线开 2 个 worker
    serverConcurrency = 1 // 会话响应把全局槽位降为 1
    await queue.enqueue('/固定目录', [{ file: namedFile('一.txt') }, { file: namedFile('二.txt') }], 'overwrite')
    await vi.waitFor(() => expect(mocks.uploads.create).toHaveBeenCalledTimes(2))
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    await flushPromises()
    expect(mocks.uploads.part).toHaveBeenCalledOnce()

    // 前台核实时服务端并发回到 2，第二个文件取得槽位
    serverConcurrency = 2
    mocks.uploads.list.mockResolvedValue(response())
    await queue.reconcile()
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledTimes(2))
    const ids = mocks.uploads.part.mock.calls.map((call) => call[0])
    expect(new Set(ids)).toEqual(new Set(['会话-一.txt', '会话-二.txt']))
  })
})

describe('运行中调高并发补开文件管线', () => {
  beforeEach(useNamedSessions)

  it('以并发 1 开始的一批上传，并发调高后立即并行处理下一个文件', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    serverConcurrency = 1
    mocks.uploads.list.mockResolvedValue({ data: { sessions: [], concurrency: 1, bot_count: 1, retry_after: 0 } })
    const queue = setup()
    await flushPromises()
    await queue.enqueue('/固定目录', [{ file: namedFile('一.txt') }, { file: namedFile('二.txt') }], 'overwrite')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    await flushPromises()
    // 只有一个文件管线：第二个文件还没开始建会话
    expect(mocks.uploads.create).toHaveBeenCalledOnce()

    serverConcurrency = 2
    mocks.uploads.list.mockResolvedValue(response())
    await queue.reconcile()
    await vi.waitFor(() => expect(mocks.uploads.create).toHaveBeenCalledTimes(2))
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledTimes(2))
  })
})

describe('分块上传停滞检测', () => {
  beforeEach(() => {
    useNamedSessions()
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'Date'] })
  })
  afterEach(() => vi.useRealTimers())

  it('请求体长时间无进度被中止并报告停滞', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '停滞.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    const signal = mocks.uploads.part.mock.calls[0]![4] as AbortSignal
    vi.advanceTimersByTime(115_000)
    expect(signal.aborted).toBe(false)
    vi.advanceTimersByTime(10_000)
    expect(signal.aborted).toBe(true)
    await vi.waitFor(() => expect(item.status).toBe('error'))
    expect(item.error).toContain('分块上传停滞')
  })

  it('持续有上传进度时不会误判停滞', async () => {
    let progress!: (loaded: number) => void
    mocks.uploads.part.mockImplementation((id: string, index: number, data: Blob, onProgress: (loaded: number) => void, signal: AbortSignal) => {
      progress = onProgress
      return hangingPart()(id, index, data, onProgress, signal)
    })
    const queue = setup()
    await flushPromises()
    await enqueueOne(queue, '缓慢.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    const signal = mocks.uploads.part.mock.calls[0]![4] as AbortSignal
    vi.advanceTimersByTime(100_000)
    progress(1)
    vi.advanceTimersByTime(100_000)
    expect(signal.aborted).toBe(false)
    vi.advanceTimersByTime(30_000)
    expect(signal.aborted).toBe(true)
  })
})

describe('取消', () => {
  beforeEach(useNamedSessions)

  it('取消进行中的任务会中止请求、删除服务器会话并移出队列', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '取消.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    const signal = mocks.uploads.part.mock.calls[0]![4] as AbortSignal
    await expect(queue.cancelItem(item)).resolves.toBe(true)
    expect(signal.aborted).toBe(true)
    expect(mocks.uploads.cancel).toHaveBeenCalledWith('会话-取消.txt', expect.any(AbortSignal))
    expect(queue.totalCount.value).toBe(0)
    await flushPromises()
    expect(mocks.uploads.complete).not.toHaveBeenCalled()
  })

  it('服务器会话已不存在（404）视为取消成功', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    mocks.uploads.cancel.mockRejectedValue(Object.assign(new Error('不存在'), { response: { status: 404 } }))
    const queue = setup()
    await flushPromises()
    const item = await enqueueOne(queue, '丢失.txt')
    await vi.waitFor(() => expect(mocks.uploads.part).toHaveBeenCalledOnce())
    await expect(queue.cancelItem(item)).resolves.toBe(true)
    expect(queue.totalCount.value).toBe(0)
  })

  it('cancelAll 确认后取消全部任务并清空延迟计数、通知扫描停止', async () => {
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises()
    const stopped = vi.fn()
    queue.onCancelled(stopped)
    await enqueueOne(queue, '全一.txt')
    await enqueueOne(queue, '全二.txt')
    queue.addDeferredFiles(3)
    expect(queue.pendingCount.value).toBe(5)
    await queue.cancelAll()
    expect(stopped).toHaveBeenCalledOnce()
    expect(queue.totalCount.value).toBe(0)
    expect(queue.pendingCount.value).toBe(0)
    expect(queue.batchBusy.value).toBe(false)
  })
})

describe('延迟文件计数', () => {
  it('add / release / clear 维护总数与活动数，入队时自动抵扣', async () => {
    useNamedSessions()
    mocks.uploads.part.mockImplementation(hangingPart())
    const queue = setup()
    await flushPromises()
    queue.addDeferredFiles(3)
    queue.addDeferredFiles(2.7) // 向下取整
    queue.addDeferredFiles(-4) // 负数忽略
    expect(queue.totalCount.value).toBe(5)
    expect(queue.activeCount.value).toBe(5)
    expect(queue.pendingCount.value).toBe(5)

    queue.releaseDeferredFiles(2)
    expect(queue.totalCount.value).toBe(3)
    // 从延迟来源入队：实体任务 + 剩余延迟数保持总数不变
    await queue.enqueue('/固定目录', [{ file: namedFile('延迟.txt') }], 'overwrite', true)
    expect(queue.totalCount.value).toBe(3)
    queue.releaseDeferredFiles(10) // 不会减成负数
    expect(queue.totalCount.value).toBe(1)
    queue.addDeferredFiles(4)
    queue.clearDeferredFiles()
    expect(queue.totalCount.value).toBe(1)

    invalidateSession()
    queue.addDeferredFiles(5) // 会话关闭后忽略
    expect(queue.totalCount.value).toBe(0)
  })
})
