import { computed, shallowRef, triggerRef } from 'vue'
import { createDirectoryController, type EntriesFetcher } from './controller'

export type TreeRow =
  | { type: 'dir'; path: string; name: string; depth: number; expanded: boolean }
  | { type: 'more'; path: string; depth: number; loading: boolean; error: unknown }

/** 每个展开目录独立游标；仅显式展开或点击加载更多才请求子目录。 */
export function createDirectoryTree(fetchPage: EntriesFetcher) {
  const branches = shallowRef(new Map<string, ReturnType<typeof createDirectoryController>>())
  const expanded = shallowRef<ReadonlySet<string>>(new Set(['/']))
  function branch(path: string) {
    let controller = branches.value.get(path)
    if (!controller) {
      controller = createDirectoryController(fetchPage)
      branches.value.set(path, controller)
      triggerRef(branches)
      void controller.reset({ path, kind: 'dir', q: '', sort: 'name', order: 'asc' })
    }
    return controller
  }
  function toggle(path: string) {
    const next = new Set(expanded.value)
    if (next.delete(path)) {
      // 收起时取消尚未完成的后代请求，重新展开从已知路径重建。
      for (const [key, controller] of branches.value) {
        if (key === path || key.startsWith(path + '/')) {
          controller.dispose()
          branches.value.delete(key)
          if (key !== path) next.delete(key)
        }
      }
      triggerRef(branches)
    } else {
      next.add(path)
      branch(path)
    }
    expanded.value = next
  }
  // 显式帧保证目录行在子树和“更多”之前，避免深目录递归栈溢出。
  const flatRows = computed(() => {
    const result: TreeRow[] = []
    type Frame = { path: string; depth: number } | { row: TreeRow }
    const stack: Frame[] = [{ path: '/', depth: 0 }]
    while (stack.length) {
      const frame = stack.pop()!
      if ('row' in frame) { result.push(frame.row); continue }
      const controller = branches.value.get(frame.path)
      if (!controller) continue
      if (controller.hasMore.value || controller.error.value) stack.push({ row: {
        type: 'more', path: frame.path, depth: frame.depth,
        loading: controller.loading.value, error: controller.error.value,
      } })
      for (let i = controller.entries.value.length - 1; i >= 0; i--) {
        const child = controller.entries.value[i]!
        const path = frame.path === '/' ? '/' + child.name : frame.path + '/' + child.name
        const open = expanded.value.has(path)
        if (open) stack.push({ path, depth: frame.depth + 1 })
        stack.push({ row: { type: 'dir', path, name: child.name, depth: frame.depth, expanded: open } })
      }
    }
    return result
  })
  function dispose() { for (const controller of branches.value.values()) controller.dispose() }
  return { rows: flatRows, start: () => branch('/'), toggle,
    loadMore: (path: string) => branch(path).loadMore(true), dispose }
}
