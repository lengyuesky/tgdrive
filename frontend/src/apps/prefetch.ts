/** 桌面端悬停预取 launch 响应：launch 无副作用，AppHost 启动时按 60 秒内新鲜度复用。 */
import { api } from '@drive/api'
import type { AppLaunch } from './types'

const TTL = 60_000
const cache = new Map<string, { at: number; data: AppLaunch }>()
const pending = new Map<string, Promise<void>>()

function hoverable(): boolean {
  return typeof matchMedia === 'function' && matchMedia('(hover: hover) and (pointer: fine)').matches
}

/** 悬停时提前请求 launch；已有缓存或请求在途时去重，失败静默（点击时仍走正常启动）。 */
export function prefetchLaunch(id: string): void {
  if (!id || !hoverable()) return
  for (const [key, hit] of cache) if (Date.now() - hit.at >= TTL) cache.delete(key)
  if (cache.has(id) || pending.has(id)) return
  const task = api.apps.launch(id)
    .then(({ data }) => { cache.set(id, { at: Date.now(), data }) })
    .catch(() => { /* 预取失败不提示；正式打开时按原流程报错。 */ })
    .finally(() => { pending.delete(id) })
  pending.set(id, task)
}

/** 取出 60 秒内的预取结果（取后即删）；过期或缺失返回 null。 */
export function takeLaunch(id: string): AppLaunch | null {
  const hit = cache.get(id)
  cache.delete(id)
  return hit && Date.now() - hit.at < TTL ? hit.data : null
}

/** 仅测试使用：预取缓存随模块存续，测试间需显式清空。 */
export function clearPrefetched(): void {
  cache.clear()
  pending.clear()
}
