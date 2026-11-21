/** 认证失效时同步终止本地会话工作，不等待路由加载或离开动画。 */
const invalidationListeners = new Set<() => void>()

export function onSessionInvalidated(listener: () => void) {
  invalidationListeners.add(listener)
  return () => { invalidationListeners.delete(listener) }
}

export function invalidateSession() {
  for (const listener of [...invalidationListeners]) listener()
}
