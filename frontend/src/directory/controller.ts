import { computed, ref, shallowRef, triggerRef } from 'vue'
import type { DirectoryEntry, EntriesQuery, EntriesResponse } from '../api'

export type DirectoryConditions = Omit<EntriesQuery, 'cursor' | 'limit'>
export type EntriesFetcher = (query: EntriesQuery, signal: AbortSignal) => Promise<EntriesResponse>

/** 元数据保持浅响应；追加只处理新批次，不复制或深代理历史数组。 */
export function createDirectoryController(fetchPage: EntriesFetcher) {
  const entries = shallowRef<DirectoryEntry[]>([])
  const batches = shallowRef<DirectoryEntry[][]>([])
  const selectedIds = shallowRef<ReadonlySet<number>>(new Set())
  const total = ref<number>()
  const loading = ref(false)
  const error = shallowRef<unknown>(null)
  const hasMore = ref(true)
  const generation = ref(0)
  const revision = ref(0)
  const byId = new Map<number, DirectoryEntry>()
  const indices = new Map<number, number>()
  let conditions: DirectoryConditions | undefined
  let cursor: string | undefined
  let request: AbortController | undefined
  let disposed = false
  const selected = computed(() => [...selectedIds.value].flatMap((id) => {
    const entry = byId.get(id)
    return entry ? [entry] : []
  }))

  async function loadMore(retry = false) {
    if (disposed || !conditions || loading.value || !hasMore.value || (error.value && !retry)) return
    const version = generation.value
    const current = new AbortController()
    request = current
    loading.value = true
    error.value = null
    try {
      const page = await fetchPage({ ...conditions, limit: 200, cursor }, current.signal)
      if (disposed || version !== generation.value || current.signal.aborted) return
      if (page.has_more && (!page.next_cursor || page.next_cursor === cursor)) {
        throw new Error('目录游标没有前进，请重试或刷新')
      }
      const batch: DirectoryEntry[] = []
      for (const entry of page.entries) {
        if (byId.has(entry.id)) continue
        indices.set(entry.id, entries.value.length)
        byId.set(entry.id, entry)
        entries.value.push(entry)
        batch.push(entry)
      }
      batches.value.push(batch)
      triggerRef(entries)
      triggerRef(batches)
      if (page.total !== undefined) total.value = page.total
      cursor = page.next_cursor ?? undefined
      hasMore.value = page.has_more
      // 动态目录可能返回全重复/空批；停住自动续载，避免滚动事件遍历剩余目录。
      if (page.has_more && batch.length === 0) error.value = new Error('本批没有新增项目，请重试或刷新')
    } catch (cause) {
      if (!disposed && version === generation.value && !current.signal.aborted) error.value = cause
    } finally {
      // 包括 finally 在内，旧请求不得解除新请求的锁或覆盖状态。
      if (!disposed && version === generation.value && request === current) {
        loading.value = false
        request = undefined
      }
    }
  }

  function reset(next: DirectoryConditions, start = true) {
    request?.abort()
    generation.value++
    conditions = { ...next }
    cursor = undefined
    request = undefined
    entries.value = []
    batches.value = []
    byId.clear()
    indices.clear()
    selectedIds.value = new Set()
    total.value = undefined
    loading.value = false
    hasMore.value = true
    error.value = null
    if (start) return loadMore()
  }

  function toggle(id: number) {
    if (!byId.has(id)) return
    const next = new Set(selectedIds.value)
    if (!next.delete(id)) next.add(id)
    selectedIds.value = next
  }
  function selectAll(on: boolean) {
    selectedIds.value = on ? new Set(byId.keys()) : new Set()
  }
  function updateFavorite(id: number, favorite: boolean) {
    const entry = byId.get(id)
    if (entry) {
      entry.favorite = favorite
      revision.value++
      triggerRef(entries)
    }
  }
  function dispose() {
    disposed = true
    request?.abort()
    generation.value++
  }
  return {
    entries, batches, selectedIds, selected, total, loading, error, hasMore, generation, revision,
    loadMore, reset, toggle, selectAll, updateFavorite, dispose,
    indexOf: (id: number) => indices.get(id),
  }
}
