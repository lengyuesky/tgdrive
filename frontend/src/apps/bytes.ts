/** 沙箱不能直接 fetch；宿主只读取自己签发的文件票据或当前包内资源。 */
export const RANGE_LIMIT = 1024 * 1024
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
export function assetPath(path: unknown): string {
  if (typeof path !== 'string' || !path || path.length > 240 || !path.split('/').every((part) => /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(part))) throw new Error('插件资源路径无效')
  return path
}
export async function boundedBytes(response: Response, limit: number, signal: AbortSignal, range?: { offset: number; length: number }): Promise<Uint8Array> {
  const fail = async (text: string): Promise<never> => { await response.body?.cancel().catch(() => {}); throw new Error(text) }
  if (!response.ok) return fail(`文件读取失败（${response.status}），请重新打开或重试`)
  let expected = Number(response.headers.get('Content-Length'))
  if (!response.headers.has('Content-Length') || !Number.isSafeInteger(expected) || expected < 0 || expected > limit) return fail('文件响应大小无效或超过限制')
  if (range) {
    const match = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(response.headers.get('Content-Range') ?? '')
    if (response.status !== 206 || !match || Number(match[1]) !== range.offset || Number(match[2]) !== Math.min(range.offset + range.length, Number(match[3])) - 1
      || Number(match[2]) - Number(match[1]) + 1 !== expected) return fail('服务未正确返回请求范围，已停止读取以避免下载整个文件')
  }
  const reader = response.body?.getReader()
  if (!reader) throw new Error('文件响应缺少内容')
  const result = new Uint8Array(expected)
  let written = 0
  try {
    while (true) {
      signal.throwIfAborted()
      const { done, value } = await reader.read()
      if (done) break
      if (written + value.length > expected) throw new Error('文件响应超过声明大小')
      result.set(value, written); written += value.length
    }
    if (written !== expected) throw new Error('文件响应提前结束，请重试')
    return result
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock() }
}
