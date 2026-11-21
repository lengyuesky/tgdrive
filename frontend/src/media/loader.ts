import { resizeMedia } from './resize'

export const ORIGINAL_LIMIT = 20 * 1024 * 1024
const CACHE_LIMIT = 16 * 1024 * 1024
const CACHE_ITEMS = 128
export interface MediaRequest {
  path: string
  version: string
  thumbnailUrl: string | null
  originalUrl: string | null
  size: number
  /** 与条目元数据同寿命，虚拟行重新挂载也不自动重试失败的内容。 */
  failure: { blocked: boolean }
}
export interface MediaLease { result: Promise<string | null>; release: () => void }
interface Job {
  key: string
  request: MediaRequest
  controller: AbortController
  refs: number
  result: Promise<string | null>
  resolve: (value: string | null) => void
  settled: boolean
  url: string | null
  bytes: number
  cached: boolean
}
interface Dependencies {
  fetch: typeof fetch
  resize: typeof resizeMedia
  createURL: (blob: Blob) => string
  revokeURL: (url: string) => void
}

class TooLarge extends Error {}

/** 不依赖 Content-Length；包括无长度及谎报长度的响应都按实际读取字节硬截断。 */
export async function readMedia(response: Response, signal: AbortSignal): Promise<Blob> {
  if (!response.ok || Number(response.headers.get('content-length')) > ORIGINAL_LIMIT) {
    await response.body?.cancel().catch(() => {})
    if (response.ok) throw new TooLarge('原图超过 20MiB')
    throw new Error(`媒体请求失败 ${response.status}`)
  }
  if (!response.body) throw new Error('浏览器不支持流式媒体读取')
  const reader = response.body.getReader()
  const parts: Uint8Array<ArrayBuffer>[] = []
  let bytes = 0
  const cancel = () => { void reader.cancel().catch(() => {}) }
  signal.addEventListener('abort', cancel, { once: true })
  try {
    signal.throwIfAborted()
    while (true) {
      const { done, value } = await reader.read()
      signal.throwIfAborted()
      if (done) break
      bytes += value.byteLength
      if (bytes > ORIGINAL_LIMIT) throw new TooLarge('原图超过 20MiB')
      parts.push(value as Uint8Array<ArrayBuffer>)
    }
    return new Blob(parts, { type: response.headers.get('content-type') ?? '' })
  } finally {
    signal.removeEventListener('abort', cancel)
    await reader.cancel().catch(() => {})
    reader.releaseLock()
  }
}

export function createMediaLoader(deps: Dependencies = {
  fetch: (...args) => fetch(...args), resize: resizeMedia,
  createURL: (blob) => URL.createObjectURL(blob), revokeURL: (url) => URL.revokeObjectURL(url),
}) {
  const jobs = new Map<string, Job>()
  const queue: Job[] = []
  const cache = new Map<string, Job>()
  let cacheBytes = 0
  let running = 0
  let decoding = false
  const decodeQueue: { job: Job; run: () => void; cancel: () => void }[] = []

  function revoke(job: Job) {
    if (job.url && !job.cached && !job.refs) { deps.revokeURL(job.url); job.url = null }
  }
  function evict(job: Job) {
    cache.delete(job.key)
    cacheBytes -= job.bytes
    job.cached = false
    revoke(job)
  }
  function remember(job: Job) {
    job.cached = true
    cache.set(job.key, job)
    cacheBytes += job.bytes
    while (cache.size > CACHE_ITEMS || cacheBytes > CACHE_LIMIT) evict(cache.values().next().value!)
  }
  function nextDecode() {
    if (decoding) return
    const waiting = decodeQueue.shift()
    if (!waiting) return
    waiting.job.controller.signal.removeEventListener('abort', waiting.cancel)
    waiting.run()
  }
  function resize(blob: Blob, job: Job): Promise<Blob> {
    return new Promise((resolve, reject) => {
      const signal = job.controller.signal
      const waiting = { job, run, cancel }
      function cancel() {
        const index = decodeQueue.indexOf(waiting)
        if (index !== -1) decodeQueue.splice(index, 1)
        reject(signal.reason)
      }
      function run() {
        if (signal.aborted) { reject(signal.reason); nextDecode(); return }
        decoding = true
        void deps.resize(blob, signal).then(resolve, reject).finally(() => { decoding = false; nextDecode() })
      }
      if (signal.aborted) { reject(signal.reason); return }
      decodeQueue.push(waiting)
      signal.addEventListener('abort', cancel, { once: true })
      nextDecode()
    })
  }
  async function load(url: string, job: Job) {
    const signal = job.controller.signal
    signal.throwIfAborted()
    const response = await deps.fetch(url, { signal, credentials: 'same-origin' })
    const blob = await readMedia(response, signal)
    signal.throwIfAborted()
    return resize(blob, job)
  }
  async function run(job: Job) {
    const signal = job.controller.signal
    let blob: Blob | undefined
    try {
      if (job.request.thumbnailUrl) {
        try { blob = await load(job.request.thumbnailUrl, job) } catch { signal.throwIfAborted() }
      }
      if (!blob) {
        if (!job.request.originalUrl || job.request.size > ORIGINAL_LIMIT) throw new Error('无可用的小图或原图超过 20MiB')
        blob = await load(job.request.originalUrl, job)
      }
      signal.throwIfAborted()
      job.url = deps.createURL(blob)
      job.bytes = blob.size
      remember(job)
    } catch {
      if (!signal.aborted) job.request.failure.blocked = true
    } finally {
      job.settled = true
      job.resolve(signal.aborted ? null : job.url)
      if (!job.refs) { if (jobs.get(job.key) === job) jobs.delete(job.key); revoke(job) }
      running--
      pump()
    }
  }
  function pump() {
    while (running < 4 && queue.length) {
      const job = queue.shift()!
      // 下载槽保留至缩小结束，避免解码队列堆积大量 20MiB 原始 Blob。
      running++
      void run(job)
    }
  }
  function acquire(request: MediaRequest): MediaLease {
    const key = JSON.stringify([request.path, request.version, request.thumbnailUrl, request.originalUrl])
    let job = jobs.get(key) ?? cache.get(key)
    if (!job && request.failure.blocked) return { result: Promise.resolve(null), release: () => {} }
    if (!job) {
      let resolve!: Job['resolve']
      const result = new Promise<string | null>((done) => { resolve = done })
      job = { key, request, result, resolve, controller: new AbortController(), refs: 0, settled: false,
        url: null, bytes: 0, cached: false }
      queue.push(job)
    }
    jobs.set(key, job)
    job.refs++
    if (job.cached) { cache.delete(key); cache.set(key, job) }
    const current = job
    let released = false
    pump()
    return {
      result: current.result,
      release() {
        if (released) return
        released = true
        current.refs--
        if (current.refs) return
        if (jobs.get(key) === current) jobs.delete(key)
        if (!current.settled) {
          current.controller.abort()
          const index = queue.indexOf(current)
          if (index !== -1) { queue.splice(index, 1); current.settled = true; current.resolve(null) }
        }
        revoke(current)
      },
    }
  }
  function clearCache() { for (const job of cache.values()) evict(job) }
  return { acquire, clearCache, stats: () => ({ running, decoding: Number(decoding), queued: queue.length,
    decodeQueued: decodeQueue.length, cacheItems: cache.size, cacheBytes, active: jobs.size }) }
}

/** 主目录所有四种模式共用一个预算，不因每个组件挂载而创建独立限流器。 */
export const mediaLoader = createMediaLoader()
