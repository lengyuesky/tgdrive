import { afterEach, describe, expect, it, vi } from 'vitest'
import { createMediaLoader, ORIGINAL_LIMIT, readMedia, type MediaRequest } from './loader'

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}
const tick = async () => { for (let i = 0; i < 20; i++) await Promise.resolve() }
function request(id = 1, options: Partial<MediaRequest> = {}): MediaRequest {
  return { path: `/目录/${id}.jpg`, version: '1', size: 100, thumbnailUrl: null,
    originalUrl: `/original/${id}`, failure: { blocked: false }, ...options }
}
function dependencies() {
  let id = 0
  return { fetch: vi.fn<typeof fetch>().mockImplementation(async () => new Response('data')),
    resize: vi.fn(async (_blob: Blob, _signal: AbortSignal) => new Blob(['small'])),
    createURL: vi.fn((_blob: Blob) => `blob:${++id}`), revokeURL: vi.fn() }
}
afterEach(() => vi.restoreAllMocks())

describe('共享媒体 loader 的下载、解码、回退与取消', () => {
  it('最多四个下载流水线，一个解码；解码积压不继续拉取原图', async () => {
    const deps = dependencies()
    const downloads = Array.from({ length: 8 }, () => deferred<Response>())
    let fetched = 0
    deps.fetch.mockImplementation(() => downloads[fetched++]!.promise)
    const decodes: ReturnType<typeof deferred<Blob>>[] = []
    let active = 0; let maximum = 0
    deps.resize.mockImplementation(async () => {
      active++; maximum = Math.max(maximum, active)
      const done = deferred<Blob>(); decodes.push(done)
      try { return await done.promise } finally { active-- }
    })
    const loader = createMediaLoader(deps)
    const leases = Array.from({ length: 8 }, (_, i) => loader.acquire(request(i)))
    expect(fetched).toBe(4)
    for (const download of downloads.slice(0, 4)) download.resolve(new Response('data'))
    await tick()
    expect(decodes).toHaveLength(1)
    expect(loader.stats()).toMatchObject({ running: 4, decoding: 1, decodeQueued: 3, queued: 4 })
    decodes[0]!.resolve(new Blob(['small']))
    await tick()
    expect(fetched).toBe(5)
    expect(decodes).toHaveLength(2)
    leases.forEach((lease) => lease.release())
    for (const download of downloads) download.resolve(new Response('data'))
    decodes[1]!.resolve(new Blob(['small']))
    await Promise.all(leases.map((lease) => lease.result)); await tick()
    expect(maximum).toBe(1)
    expect(loader.stats()).toMatchObject({ running: 0, decoding: 0, decodeQueued: 0, queued: 0, active: 0 })
    loader.clearCache()
  })

  it('取消排队和流读取；忽略不遵守 AbortSignal 的晚到下载，不阻塞新键', async () => {
    const deps = dependencies()
    const downloads = Array.from({ length: 5 }, () => deferred<Response>())
    let fetched = 0
    deps.fetch.mockImplementation(() => downloads[fetched++]!.promise)
    const loader = createMediaLoader(deps)
    const leases = Array.from({ length: 5 }, (_, i) => loader.acquire(request(i)))
    const signal = deps.fetch.mock.calls[0]![1]!.signal!
    leases[0]!.release(); leases[4]!.release()
    expect(signal.aborted).toBe(true)
    expect(await leases[4]!.result).toBeNull()
    for (const download of downloads) download.resolve(new Response('data'))
    await Promise.all(leases.map((lease) => lease.result))
    expect(fetched).toBe(4)
    expect(await leases[0]!.result).toBeNull()
    expect(deps.createURL).toHaveBeenCalledTimes(3)
    leases.forEach((lease) => lease.release()); loader.clearCache()
    const cancel = vi.fn()
    const controller = new AbortController()
    const pending = readMedia(new Response(new ReadableStream({ cancel })), controller.signal)
    controller.abort()
    await expect(pending).rejects.toBeDefined()
    expect(cancel).toHaveBeenCalledTimes(1)
  })

  it('图片优先小图，404或坏图才回退；视频永不下载原视频', async () => {
    const deps = dependencies()
    deps.fetch.mockImplementation(async (url) => new Response('data', { status: url === '/missing' ? 404 : 200 }))
    const loader = createMediaLoader(deps)
    const a = loader.acquire(request(1, { thumbnailUrl: '/thumb' }))
    expect(await a.result).toBeTruthy()
    expect(deps.fetch.mock.calls.map(([url]) => url)).toEqual(['/thumb'])
    const b = loader.acquire(request(2, { thumbnailUrl: '/missing' }))
    await b.result
    expect(deps.fetch.mock.calls.map(([url]) => url)).toEqual(['/thumb', '/missing', '/original/2'])
    deps.resize.mockRejectedValueOnce(new Error('坏图'))
    const c = loader.acquire(request(3, { thumbnailUrl: '/broken' }))
    await c.result
    expect(deps.fetch.mock.calls.slice(-2).map(([url]) => url)).toEqual(['/broken', '/original/3'])
    const d = loader.acquire(request(4, { originalUrl: null, thumbnailUrl: '/missing' }))
    const e = loader.acquire(request(5, { originalUrl: null }))
    expect(await d.result).toBeNull(); expect(await e.result).toBeNull()
    expect(deps.fetch.mock.calls.map(([url]) => url)).not.toContain('/original/4')
    ;[a, b, c, d, e].forEach((lease) => lease.release()); loader.clearCache()
  })

  it('元数据已超限不拉原图；流超限后重新挂载也不自动重试，取消不是失败', async () => {
    const deps = dependencies()
    const loader = createMediaLoader(deps)
    const source = request(1, { size: ORIGINAL_LIMIT + 1 })
    const first = loader.acquire(source)
    expect(await first.result).toBeNull(); first.release()
    expect(await loader.acquire(source).result).toBeNull()
    expect(deps.fetch).not.toHaveBeenCalled()
    const cancel = vi.fn()
    deps.fetch.mockResolvedValueOnce(new Response(new ReadableStream({ start(controller) {
      controller.enqueue(new Uint8Array(ORIGINAL_LIMIT)); controller.enqueue(new Uint8Array(1))
    }, cancel })))
    const unknown = request(2)
    const lease = loader.acquire(unknown)
    expect(await lease.result).toBeNull(); lease.release()
    expect(unknown.failure.blocked).toBe(true)
    expect(await loader.acquire(unknown).result).toBeNull()
    expect(deps.fetch).toHaveBeenCalledTimes(1)
    expect(deps.resize).not.toHaveBeenCalled(); expect(cancel).toHaveBeenCalled()
  })

  it('按路径和内容版本隔离，同键共享引用；旧任务取消后晚到不覆盖新任务', async () => {
    const deps = dependencies()
    const old = deferred<Response>()
    deps.fetch.mockImplementationOnce(() => old.promise)
    const loader = createMediaLoader(deps)
    const source = request()
    const a = loader.acquire(source); const b = loader.acquire(source)
    expect(deps.fetch).toHaveBeenCalledTimes(1)
    a.release(); expect(deps.fetch.mock.calls[0]![1]!.signal!.aborted).toBe(false)
    b.release(); expect(deps.fetch.mock.calls[0]![1]!.signal!.aborted).toBe(true)
    const c = loader.acquire(source)
    const url = await c.result
    old.resolve(new Response('old'))
    expect(await a.result).toBeNull()
    const d = loader.acquire(source)
    expect(await d.result).toBe(url)
    const e = loader.acquire(request(1, { version: '2' }))
    const f = loader.acquire(request(1, { path: '/other/1.jpg' }))
    expect(await e.result).not.toBe(url); expect(await f.result).not.toBe(url)
    expect(source.failure.blocked).toBe(false)
    ;[c, d, e, f].forEach((lease) => lease.release()); loader.clearCache()
  })
})

describe('实际读取硬限额', () => {
  it.each([undefined, '1'])('无长度或谎报长度 %s 仍在20MiB边界停止并cancel', async (length) => {
    const cancel = vi.fn()
    const response = new Response(new ReadableStream({ start(controller) {
      controller.enqueue(new Uint8Array(ORIGINAL_LIMIT)); controller.enqueue(new Uint8Array(1))
    }, cancel }), { headers: length ? { 'Content-Length': length } : {} })
    await expect(readMedia(response, new AbortController().signal)).rejects.toThrow('20MiB')
    expect(cancel).toHaveBeenCalledTimes(1)
  })
  it('恰好20MiB允许；声明超限直接取消响应体', async () => {
    const blob = await readMedia(new Response(new Uint8Array(ORIGINAL_LIMIT)), new AbortController().signal)
    expect(blob.size).toBe(ORIGINAL_LIMIT)
    const cancel = vi.fn()
    await expect(readMedia(new Response(new ReadableStream({ cancel }), {
      headers: { 'Content-Length': String(ORIGINAL_LIMIT + 1) },
    }), new AbortController().signal)).rejects.toThrow('20MiB')
    expect(cancel).toHaveBeenCalledTimes(1)
  })
})

describe('双限额 LRU 与活动 URL 生命周期', () => {
  it('128项，命中更新LRU；驱逐活动URL不revoke，末个引用释放才回收', async () => {
    const deps = dependencies(); const loader = createMediaLoader(deps)
    const first = loader.acquire(request(0)); const firstUrl = await first.result
    for (let i = 1; i <= 128; i++) { const lease = loader.acquire(request(i)); await lease.result; lease.release() }
    expect(loader.stats().cacheItems).toBe(128)
    expect(deps.revokeURL).not.toHaveBeenCalledWith(firstUrl)
    const same = loader.acquire(request(0)); expect(await same.result).toBe(firstUrl)
    first.release(); expect(deps.revokeURL).not.toHaveBeenCalledWith(firstUrl)
    same.release(); expect(deps.revokeURL).toHaveBeenCalledWith(firstUrl)
    const recent = loader.acquire(request(1)); await recent.result; recent.release()
    const extra = loader.acquire(request(129)); await extra.result; extra.release()
    const count = deps.fetch.mock.calls.length
    const hit = loader.acquire(request(1)); await hit.result; hit.release()
    expect(deps.fetch).toHaveBeenCalledTimes(count)
    loader.clearCache()
    expect(deps.revokeURL).toHaveBeenCalledTimes(deps.createURL.mock.calls.length)
    expect(loader.stats()).toMatchObject({ active: 0, cacheItems: 0, cacheBytes: 0 })
  })
  it('16MiB先到也驱逐；clearCache不破坏活动引用且release幂等', async () => {
    const deps = dependencies()
    deps.resize.mockImplementation(async () => new Blob([new Uint8Array(1024 * 1024)]))
    const loader = createMediaLoader(deps)
    const leases = []
    for (let i = 0; i < 20; i++) { const lease = loader.acquire(request(i)); await lease.result; leases.push(lease) }
    expect(loader.stats()).toMatchObject({ cacheItems: 16, cacheBytes: 16 * 1024 * 1024, active: 20 })
    expect(deps.revokeURL).not.toHaveBeenCalled()
    loader.clearCache()
    expect(deps.revokeURL).not.toHaveBeenCalled()
    leases.forEach((lease) => { lease.release(); lease.release() })
    expect(deps.revokeURL).toHaveBeenCalledTimes(20)
  })
})
