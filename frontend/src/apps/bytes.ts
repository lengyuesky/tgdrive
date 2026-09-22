/** 沙箱不能直接 fetch；宿主只读取自己签发的文件票据或当前包内资源。 */
export const RANGE_LIMIT = 1024 * 1024
/** 批量段读在宿主页内的并行上限；插件侧共享队列仍限制同时在途的批次数。 */
export const RANGE_PARALLEL = 4
export interface FileReference { id: number; content_version: string }
export interface RangeSpec { offset: number; length: number }

function validRangeSpec(value: unknown, prefix = '文件读取范围'): RangeSpec {
  const item = value as RangeSpec | undefined
  if (!item || !Number.isSafeInteger(item.offset) || item.offset < 0
    || !Number.isSafeInteger(item.length) || item.length < 1 || item.length > RANGE_LIMIT
    || !Number.isSafeInteger(item.offset + item.length)) throw new Error(`${prefix}无效或超过 1 MiB`)
  return { offset: item.offset, length: item.length }
}

export function rangeInput(params: Record<string, unknown>) {
  const ref = params.ref as FileReference | undefined
  const offset = params.offset as number
  const length = params.length as number
  if (!ref || !Number.isSafeInteger(ref.id) || ref.id <= 0 || typeof ref.content_version !== 'string'
    || !/^[a-f0-9]{64}$/.test(ref.content_version)) throw new Error('文件引用无效')
  const spec = validRangeSpec({ offset, length })
  return { ref: { id: ref.id, content_version: ref.content_version }, ...spec }
}

/** 批量段读：1～8 段、每段 ≤1 MiB、总量 ≤4 MiB；一次往返带回全部分块。 */
export function rangesInput(params: Record<string, unknown>) {
  const ref = params.ref as FileReference | undefined
  const list = params.ranges
  if (!ref || !Number.isSafeInteger(ref.id) || ref.id <= 0 || typeof ref.content_version !== 'string'
    || !/^[a-f0-9]{64}$/.test(ref.content_version) || !Array.isArray(list)
    || list.length < 1 || list.length > 8) throw new Error('批量读取参数无效')
  const ranges = list.map((item) => validRangeSpec(item, '批量读取范围'))
  const total = ranges.reduce((sum, item) => sum + item.length, 0)
  if (total > 4 * RANGE_LIMIT) throw new Error('批量读取总量超过 4 MiB')
  return { ref: { id: ref.id, content_version: ref.content_version }, ranges }
}
/** 有限并发映射：保持结果顺序；任一项失败即整体拒绝，调用方通过信号取消其余在途项。 */
export async function mapLimit<T, R>(items: readonly T[], limit: number, worker: (item: T, index: number) => Promise<R>): Promise<R[]> {
  const results = new Array<R>(items.length)
  let next = 0
  const run = async () => {
    while (next < items.length) { const index = next++; results[index] = await worker(items[index]!, index) }
  }
  await Promise.all(Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, run))
  return results
}
export function assetPath(path: unknown): string {
  if (typeof path !== 'string' || !path || path.length > 240 || !path.split('/').every((part) => /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(part))) throw new Error('插件资源路径无效')
  return path
}
export async function boundedBytes(response: Response, limit: number, signal: AbortSignal, range?: { offset: number; length: number }): Promise<Uint8Array> {
  const fail = async (text: string): Promise<never> => { await response.body?.cancel().catch(() => {}); throw new Error(text) }
  if (!response.ok) return fail(`文件读取失败（${response.status}），请重新打开或重试`)
  const declared = response.headers.get('Content-Length')
  const expected = Number(declared)
  if (declared !== null && (!Number.isSafeInteger(expected) || expected < 0 || expected > limit)) return fail('文件响应大小无效或超过限制')
  if (range) {
    // 范围读取的媒体响应从不压缩：长度必须与声明及 Content-Range 完全一致，否则可能是整个文件。
    const match = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(response.headers.get('Content-Range') ?? '')
    if (declared === null || response.status !== 206 || !match || Number(match[1]) !== range.offset || Number(match[2]) !== Math.min(range.offset + range.length, Number(match[3])) - 1
      || Number(match[2]) - Number(match[1]) + 1 !== expected) return fail('服务未正确返回请求范围，已停止读取以避免下载整个文件')
  }
  // 包内资源可能以 brotli/gzip 变体传输：浏览器透明解码后的长度大于 Content-Length（压缩后大小），
  // 因此只按上限流式读取；截断由浏览器与解码器报错兜底。
  const exact = range ? expected : undefined
  const reader = response.body?.getReader()
  if (!reader) throw new Error('文件响应缺少内容')
  const chunks: Uint8Array[] = []
  let written = 0
  try {
    while (true) {
      signal.throwIfAborted()
      const { done, value } = await reader.read()
      if (done) break
      if (written + value.length > (exact ?? limit)) throw new Error(exact === undefined ? '文件响应超过大小限制' : '文件响应超过声明大小')
      chunks.push(value); written += value.length
    }
    if (exact !== undefined && written !== exact) throw new Error('文件响应提前结束，请重试')
    if (chunks.length === 1) return chunks[0]!
    const result = new Uint8Array(written)
    let offset = 0
    for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.length }
    return result
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock() }
}
