import type { Drive, FileEntry } from '../sdk/types'
import { imageInfo } from '../reader/image'
import { coverCandidates, isVideo, parentPath } from './model'
import { RangeFile, ReadScheduler, MiB, isAbort, delay } from './io'
import { requireDirectory, withinDirectory } from './libraries'

export class Library {
  private cache = new Map<string, { files: FileEntry[]; complete: boolean }>()
  private pending = new Map<string, Promise<{ files: FileEntry[]; complete: boolean }>>()
  private generation = 0
  constructor(private drive: Drive, private root: string | null = null) {}
  clear() { this.generation++; this.cache.clear(); this.pending.clear() }
  setScope(root: string | null) { if (root !== this.root) { this.clear(); this.root = root } }
  contains(path: string) { return this.root !== null && withinDirectory(path, this.root) }
  async directory(path: string, signal: AbortSignal, full = false): Promise<{ files: FileEntry[]; complete: boolean }> {
    signal.throwIfAborted()
    requireDirectory(this.root, path)
    const generation = this.generation, cached = this.cache.get(path)
    if (cached && (!full || cached.complete)) return cached
    const key = `${path}:${full}`, pending = this.pending.get(key)
    if (pending) {
      try { return await pending }
      catch (error) {
        if (isAbort(error) && !signal.aborted) return this.directory(path, signal, full)
        throw error
      }
    }
    const task = (async () => {
      const files: FileEntry[] = []
      let cursor: string | null = null, complete = false
      do {
        const page = await this.drive.files.list({ path, limit: 200, cursor }, { signal })
        files.push(...page.entries); cursor = page.next_cursor; complete = !page.has_more
        if (!full || complete) break
        await delay(140, signal)
      } while (cursor && files.length < 10000)
      signal.throwIfAborted()
      const value = { files, complete }, current = this.cache.get(path)
      // 旧轮次不能回填刷新后的缓存，封面第一页也不能把完整选集降级为局部结果。
      if (generation === this.generation && (!current || complete || !current.complete && files.length >= current.files.length)) {
        if (!this.cache.has(path) && this.cache.size >= 4) this.cache.delete(this.cache.keys().next().value!)
        this.cache.set(path, value)
      }
      return value
    })().finally(() => { if (this.pending.get(key) === task) this.pending.delete(key) })
    this.pending.set(key, task)
    return task
  }
}
/** 按可见性加载封面；离开页面立即终止，离开视口回收对象 URL 与图片节点。 */
export class ArtLoader {
  private jobs = new Map<HTMLElement, { file: FileEntry; controller?: AbortController; url?: string; wide: boolean; running?: boolean; visible?: boolean }>()
  private observer: IntersectionObserver
  private active = 0
  constructor(private drive: Drive, private library: Library, private scheduler: ReadScheduler, private signal: AbortSignal, private thumbnailsOnly = false, private thumbnailRoots: readonly string[] = []) {
    this.observer = new IntersectionObserver(entries => {
      for (const entry of entries) {
        const target = entry.target as HTMLElement, job = this.jobs.get(target)
        if (!job) continue
        job.visible = entry.isIntersecting
        if (entry.isIntersecting) {
          if (!job.running && !target.querySelector('img')) void this.load(target)
        } else this.release(target)
      }
    }, { rootMargin: '100px' })
    signal.addEventListener('abort', () => this.clear(), { once: true })
  }
  observe(target: HTMLElement, file: FileEntry, wide = false) { this.jobs.set(target, { file, wide }); this.observer.observe(target) }
  private release(target: HTMLElement) {
    const job = this.jobs.get(target)
    job?.controller?.abort()
    if (job?.url) URL.revokeObjectURL(job.url)
    if (job) job.url = undefined
    target.querySelectorAll('img').forEach(image => { image.removeAttribute('src'); image.remove() })
  }
  private async load(target: HTMLElement) {
    const job = this.jobs.get(target)!
    job.running = true; job.controller = new AbortController()
    const signal = AbortSignal.any([this.signal, job.controller.signal])
    let entered = false
    try {
      while (this.active >= 4) await delay(50, signal)
      signal.throwIfAborted(); this.active++; entered = true
      // 收藏和历史只取已有缩略图，不为每条记录枚举不同媒体库的目录。
      const files = this.thumbnailsOnly ? [] : (await this.library.directory(parentPath(job.file.path), signal)).files
      for (const file of coverCandidates(job.file, files, job.wide)) {
        if (file.size > 8 * MiB) continue
        try {
          const source = new RangeFile(this.drive, file, signal, this.scheduler)
          const bytes = await source.read(0, file.size); source.clear()
          const info = imageInfo(bytes)
          if (info.width * info.height > 16_000_000) continue
          const url = URL.createObjectURL(new Blob([bytes], { type: info.mime }))
          job.url = url
          await this.image(target, url, signal)
          return
        } catch (error) { if (isAbort(error)) throw error; if (job.url) URL.revokeObjectURL(job.url); job.url = undefined }
      }
      signal.throwIfAborted()
      const url = await this.scheduler.run(async () => {
        // 等待视口或调度槽期间文件可能已移出范围；在真正签发缩略图前重新核对。
        const current = await this.drive.files.stat({ id: job.file.id }, { signal })
        signal.throwIfAborted()
        const allowed = this.thumbnailsOnly ? this.thumbnailRoots.some(root => withinDirectory(current.path, root)) : this.library.contains(current.path)
        if (!allowed || !isVideo(current) || current.content_version !== job.file.content_version) throw new Error('视频已移出范围或内容改变')
        return this.drive.media.url(current, 'thumbnail')
      }, signal)
      await this.image(target, url, signal)
    } catch { /* 无封面、超限或取消均保留不读取原视频的占位。 */ }
    finally {
      if (entered) this.active--
      job.running = false
      // 快速划出再划入时旧任务可能仍在取消；可见的新任务必须补上，不能永远停留占位。
      if (signal.aborted && !this.signal.aborted && job.visible && this.jobs.get(target) === job) void this.load(target)
    }
  }
  private async image(target: HTMLElement, url: string, signal: AbortSignal) {
    signal.throwIfAborted()
    const image = new Image(); image.alt = ''; image.decoding = 'async'; image.crossOrigin = 'anonymous'
    await new Promise<void>((resolve, reject) => {
      const cleanup = () => { image.onload = null; image.onerror = null; signal.removeEventListener('abort', stop) }
      const stop = () => { cleanup(); image.removeAttribute('src'); reject(new DOMException('读取已取消', 'AbortError')) }
      image.onload = () => { cleanup(); resolve() }; image.onerror = () => { cleanup(); reject(new Error('封面不可用')) }
      signal.addEventListener('abort', stop, { once: true }); image.src = url
    })
    signal.throwIfAborted(); target.append(image)
  }
  clear() { this.observer.disconnect(); for (const target of this.jobs.keys()) this.release(target); this.jobs.clear() }
}
