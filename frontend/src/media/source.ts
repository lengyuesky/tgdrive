import type { DirectoryEntry } from '../api'

export function contentVersion(entry: DirectoryEntry) {
  return `${entry.id}:${entry.modified_at}:${entry.size}:${entry.sha256 ?? ''}`
}

// 失败标记跟随已访问的浅元数据，不占用小图 LRU，也不会让被回收的目录常驻内存。
const failures = new WeakMap<DirectoryEntry, { key: string; blocked: boolean }>()
export function contentFailure(entry: DirectoryEntry, path: string, version: string) {
  const key = JSON.stringify([path, version, entry.has_thumbnail])
  let failure = failures.get(entry)
  if (!failure || failure.key !== key) {
    failure = { key, blocked: false }
    failures.set(entry, failure)
  }
  return failure
}
