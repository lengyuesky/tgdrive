/// <reference lib="webworker" />

import { createSHA256 } from 'hash-wasm'

type HashCommand =
  | { type: 'hash'; id: number; file: File }
  | { type: 'cancel'; id: number }

const scope = self as unknown as DedicatedWorkerGlobalScope
const cancelled = new Set<number>()

scope.onmessage = (event: MessageEvent<HashCommand>) => {
  const command = event.data
  if (command.type === 'cancel') {
    cancelled.add(command.id)
    return
  }
  void hashFile(command.id, command.file)
}

async function hashFile(id: number, file: File) {
  try {
    const hasher = await createSHA256()
    hasher.init()
    const chunkSize = 8 * 1024 * 1024
    let lastReportedAt = 0
    for (let offset = 0; offset < file.size; offset += chunkSize) {
      if (cancelled.delete(id)) {
        scope.postMessage({ type: 'cancelled', id })
        return
      }
      const end = Math.min(offset + chunkSize, file.size)
      const buffer = await file.slice(offset, end).arrayBuffer()
      hasher.update(new Uint8Array(buffer))
      const now = performance.now()
      if (now - lastReportedAt >= 150 || end === file.size) {
        scope.postMessage({ type: 'progress', id, percentage: Math.round((end / file.size) * 100) })
        lastReportedAt = now
      }
    }
    if (cancelled.delete(id)) {
      scope.postMessage({ type: 'cancelled', id })
      return
    }
    if (file.size === 0) scope.postMessage({ type: 'progress', id, percentage: 100 })
    scope.postMessage({ type: 'done', id, digest: hasher.digest('hex') })
  } catch (error) {
    scope.postMessage({
      type: 'error',
      id,
      message: error instanceof Error ? error.message : String(error),
    })
  }
}

export {}
