/** 批量信封：一次往返执行多项服务端能力；白名单与上限和宿主服务端保持一致。 */
export const BATCH_LIMIT = 16
/** 可进入批量的能力：只含服务端即可完成的元数据、存储与票据操作，不含界面交互、字节读取与批量本身。 */
export const BATCH_METHODS = new Set([
  'app.ping', 'settings.get', 'settings.patch', 'files.search', 'files.list', 'files.stat', 'files.searchPage',
  'favorites.set', 'media.url', 'storage.get', 'storage.set', 'storage.list', 'storage.delete',
])
export interface BatchCall { method: string; params: Record<string, unknown> }

export function batchInput(params: Record<string, unknown>): { calls: BatchCall[] } {
  const calls = params.calls
  if (!Array.isArray(calls) || calls.length < 1 || calls.length > BATCH_LIMIT) throw new Error(`批量调用需包含 1 到 ${BATCH_LIMIT} 项能力`)
  return {
    calls: calls.map((item) => {
      const call = item as { method?: unknown; params?: unknown } | null
      if (!call || typeof call !== 'object' || typeof call.method !== 'string' || !BATCH_METHODS.has(call.method)) throw new Error('批量调用包含不支持的能力')
      const inner = call.params ?? {}
      if (!inner || typeof inner !== 'object' || Array.isArray(inner)) throw new Error('批量调用参数无效')
      return { method: call.method, params: inner as Record<string, unknown> }
    }),
  }
}
