import { onBeforeUnmount, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { errMsg } from '../api'
import { appAlert } from '../dialog'
import type { TransferController, UploadFile, UploadCandidate } from './controller'

export function createUploadIntake(queue: TransferController) {
const targetDirectory = ref('/')
const fileInput = ref<HTMLInputElement>()
const dirInput = ref<HTMLInputElement>()
type StreamConflictAction = 'overwrite' | 'skip'
interface PickerFileHandle {
  kind: 'file'
  name: string
  getFile(): Promise<File>
}
interface PickerDirectoryHandle {
  kind: 'directory'
  name: string
  values(): AsyncIterableIterator<PickerFileHandle | PickerDirectoryHandle>
}

const isFirefoxBrowser = /Firefox\//i.test(navigator.userAgent)
const DIRECTORY_BATCH_SIZE = isFirefoxBrowser ? 25 : 200
const MAX_SAFE_FIREFOX_DROP_ITEMS = 1000
let directoryScanGeneration = 0
let disposed = false
const dragging = ref(false)
let dragDepth = 0

function onDragEnter() {
  if (disposed) return
  dragDepth += 1
  dragging.value = true
}

function onDragLeave() {
  dragDepth = Math.max(0, dragDepth - 1)
  if (dragDepth === 0) dragging.value = false
}

function cancelDirectoryScan() {
  directoryScanGeneration += 1
  queue.clearDeferredFiles()
}

function beginDirectoryScan() {
  cancelDirectoryScan()
  return directoryScanGeneration
}

/**
 * 把惰性文件源按固定窗口送入上传队列。只有队列释放容量后才读取下一批 File，
 * 因此几万个文件不会同时变成前端任务和浏览器文件句柄。
 */
async function consumeUploadStream(
  target: string,
  generation: number,
  produce: (push: (file: File, rel: string) => Promise<boolean>) => Promise<void>,
  initialConflictAction: StreamConflictAction,
  fromDeferredSource = false,
) {
  let batch: UploadFile[] = []
  let conflictAction: StreamConflictAction = initialConflictAction

  const flush = async () => {
    if (disposed || generation !== directoryScanGeneration) { batch = []; return false }
    if (!batch.length) return true
    const current = batch
    batch = []
    const selected = await queue.enqueue(
      target,
      current,
      conflictAction,
      fromDeferredSource,
    )
    if (disposed || generation !== directoryScanGeneration) return false
    if (selected === 'cancel') {
      cancelDirectoryScan()
      return false
    }
    if (selected === 'overwrite' || selected === 'skip') conflictAction = selected
    return generation === directoryScanGeneration
  }

  const push = async (file: File, rel: string) => {
    if (disposed || generation !== directoryScanGeneration) return false
    if (batch.length === 0) {
      const available = await queue.waitForCapacity(DIRECTORY_BATCH_SIZE)
      if (!available || generation !== directoryScanGeneration) return false
    }
    batch.push({ file, rel })
    return batch.length < DIRECTORY_BATCH_SIZE || flush()
  }

  await produce(push)
  if (generation === directoryScanGeneration) await flush()
}

async function walkDirectoryHandle(
  handle: PickerDirectoryHandle,
  rel: string,
  push: (file: File, rel: string) => Promise<boolean>,
  generation: number,
): Promise<boolean> {
  if (disposed || generation !== directoryScanGeneration) return false
  for await (const child of handle.values()) {
    if (disposed || generation !== directoryScanGeneration) return false
    if (child.kind === 'file') {
      if (!(await push(await child.getFile(), rel))) return false
      continue
    }
    const childRel = rel ? `${rel}/${child.name}` : child.name
    if (!(await walkDirectoryHandle(child, childRel, push, generation))) return false
  }
  return true
}

async function scanDirectoryHandle(
  handle: PickerDirectoryHandle,
  rel: string,
  candidates: UploadCandidate[],
  isCurrent: () => boolean,
): Promise<boolean> {
  for await (const child of handle.values()) {
    if (!isCurrent()) return false
    if (child.kind === 'file') {
      candidates.push({ name: child.name, rel })
      continue
    }
    const childRel = rel ? `${rel}/${child.name}` : child.name
    if (!(await scanDirectoryHandle(child, childRel, candidates, isCurrent))) return false
  }
  return true
}

function fileRelativeDir(file: File) {
  const path = (file as File & { webkitRelativePath?: string }).webkitRelativePath ?? ''
  return path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : ''
}

async function candidatesFromFileList(files: FileList, withRelativePath: boolean) {
  const candidates: UploadCandidate[] = []
  const generation = directoryScanGeneration
  for (let index = 0; index < files.length; index += 1) {
    if (disposed || generation !== directoryScanGeneration) return []
    const file = files[index]!
    candidates.push({ name: file.name, rel: withRelativePath ? fileRelativeDir(file) : '' })
    if (index > 0 && index % 500 === 0) {
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
    }
  }
  return candidates
}

async function chooseConflictPolicy(
  target: string,
  candidates: UploadCandidate[],
  generation: number,
): Promise<StreamConflictAction | null> {
  if (disposed || generation !== directoryScanGeneration) return null
  const selected = await queue.resolveUploadConflicts(target, candidates)
  // 旧扫描的迟到结果不能取消用户刚开始的新扫描。
  if (disposed || generation !== directoryScanGeneration) return null
  if (!selected || selected === 'cancel') {
    cancelDirectoryScan()
    return null
  }
  return selected
}

/** Chromium 优先使用惰性目录句柄；不支持时退回 webkitdirectory。 */
async function pickDirectory() {
  if (disposed) return
  const showDirectoryPicker = (
    window as Window & { showDirectoryPicker?: () => Promise<PickerDirectoryHandle> }
  ).showDirectoryPicker
  if (!showDirectoryPicker) {
    if (isFirefoxBrowser) {
      await appAlert({
        title: 'Firefox 请拖拽文件夹上传',
        message:
          'Firefox 的文件夹选择器会在网页接管前一次性载入全部文件，几万个文件可能直接导致浏览器退出。请关闭此提示后，把文件夹从系统文件管理器拖到当前页面；拖拽入口会按批次惰性读取。',
        kind: 'warning',
        confirmText: '知道了',
      })
      return
    }
    dirInput.value?.click()
    return
  }

  try {
    // 必须在菜单点击事件内直接调用，浏览器才会认可用户手势。
    const target = targetDirectory.value
    const root = await showDirectoryPicker.call(window)
    if (disposed) return
    const generation = beginDirectoryScan()
    const candidates: UploadCandidate[] = []
    await scanDirectoryHandle(
      root,
      root.name,
      candidates,
      () => generation === directoryScanGeneration,
    )
    if (generation !== directoryScanGeneration) return
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) return
    queue.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          await walkDirectoryHandle(root, root.name, push, generation)
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) queue.clearDeferredFiles()
    }
  } catch (error) {
    if (!disposed && (error as { name?: string }).name !== 'AbortError') {
      ElMessage.error(`读取文件夹失败：${errMsg(error)}`)
    }
  }
}

async function onPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  if (disposed) { input.value = ''; return }
  const files = input.files
  if (files?.length) {
    const target = targetDirectory.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, false)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) {
      input.value = ''
      return
    }
    queue.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            if (!(await push(files[index]!, ''))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) queue.clearDeferredFiles()
    }
    input.value = ''
  }
}

async function onDirPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  if (disposed) { input.value = ''; return }
  const files = input.files
  if (files?.length) {
    const target = targetDirectory.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, true)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) {
      input.value = ''
      return
    }
    queue.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            const file = files[index]!
            if (!(await push(file, fileRelativeDir(file)))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) queue.clearDeferredFiles()
    }
    input.value = ''
  }
}

async function walkEntry(
  entry: FileSystemEntry,
  rel: string,
  push: (file: File, rel: string) => Promise<boolean>,
  generation: number,
): Promise<boolean> {
  if (disposed || generation !== directoryScanGeneration) return false
  if (entry.isFile) {
    const f = await new Promise<File>((res, rej) => (entry as FileSystemFileEntry).file(res, rej))
    return push(f, rel)
  } else if (entry.isDirectory) {
    const dirRel = rel ? `${rel}/${entry.name}` : entry.name
    const reader = (entry as FileSystemDirectoryEntry).createReader()
    for (;;) {
      // readEntries 单次最多返回 100 项,要循环读到空为止
      const batch = await new Promise<FileSystemEntry[]>((res, rej) =>
        reader.readEntries(res, rej),
      )
      if (disposed || generation !== directoryScanGeneration) return false
      if (!batch.length) break
      for (const e of batch) if (!(await walkEntry(e, dirRel, push, generation))) return false
    }
  }
  return true
}

async function scanEntry(
  entry: FileSystemEntry,
  rel: string,
  candidates: UploadCandidate[],
  isCurrent: () => boolean,
): Promise<boolean> {
  if (!isCurrent()) return false
  if (entry.isFile) {
    candidates.push({ name: entry.name, rel })
    return true
  }
  if (!entry.isDirectory) return true
  const dirRel = rel ? `${rel}/${entry.name}` : entry.name
  const reader = (entry as FileSystemDirectoryEntry).createReader()
  for (;;) {
    if (!isCurrent()) return false
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
      reader.readEntries(resolve, reject),
    )
    if (!batch.length) return true
    for (const child of batch) {
      if (!(await scanEntry(child, dirRel, candidates, isCurrent))) return false
    }
  }
}

async function onDrop(ev: DragEvent) {
  if (disposed) return
  dragDepth = 0
  dragging.value = false
  const items = ev.dataTransfer?.items
  if (isFirefoxBrowser && items && items.length > MAX_SAFE_FIREFOX_DROP_ITEMS) {
    await appAlert({
      title: 'Firefox 无法安全处理此次拖拽',
      message: `Firefox 已把文件夹展开为 ${items.length} 个顶层文件项；网页无法在不复制这些文件句柄的情况下继续，强行处理会再次导致浏览器退出。请改用 Chrome/Edge 上传这个超大目录，或把目录拆分后再用 Firefox。`,
      kind: 'warning',
      confirmText: '知道了',
    })
    return
  }
  // 优先走 entry 递归(支持拖入整个文件夹);await 前必须同步取完 entry
  const firstItem = items?.[0] as
    | (DataTransferItem & { getAsEntry?: () => FileSystemEntry | null })
    | undefined
  const firstEntryGetter = firstItem?.webkitGetAsEntry ?? firstItem?.getAsEntry
  if (items?.length && typeof firstEntryGetter === 'function') {
    const roots: FileSystemEntry[] = []
    for (const it of items) {
      const extended = it as DataTransferItem & { getAsEntry?: () => FileSystemEntry | null }
      const getEntry = extended.webkitGetAsEntry ?? extended.getAsEntry
      const e = getEntry?.call(extended)
      if (e) roots.push(e)
    }
    if (roots.length) {
      const target = targetDirectory.value
      const generation = beginDirectoryScan()
      try {
        const candidates: UploadCandidate[] = []
        for (const root of roots) {
          if (
            !(await scanEntry(
              root,
              '',
              candidates,
              () => generation === directoryScanGeneration,
            ))
          ) {
            return
          }
        }
        if (generation !== directoryScanGeneration) return
        const conflictAction = await chooseConflictPolicy(target, candidates, generation)
        if (!conflictAction) return
        queue.setDeferredFileCount(candidates.length)
        await consumeUploadStream(
          target,
          generation,
          async (push) => {
            for (const root of roots) if (!(await walkEntry(root, '', push, generation))) return
          },
          conflictAction,
          true,
        )
      } catch {
        /* 个别条目读取失败时保留已经进入队列的任务。 */
      } finally {
        if (generation === directoryScanGeneration) queue.clearDeferredFiles()
      }
      return
    }
  }
  const files = ev.dataTransfer?.files
  if (files?.length) {
    const target = targetDirectory.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, false)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) return
    queue.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            if (!(await push(files[index]!, ''))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) queue.clearDeferredFiles()
    }
  }
}

function pasteStamp() {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`
}

function onPaste(e: ClipboardEvent) {
  if (disposed) return
  if (e.target instanceof HTMLElement && (e.target.isContentEditable || e.target.matches('input, textarea'))) return
  const files = Array.from(e.clipboardData?.files ?? [])
  if (!files.length) return // 纯文本粘贴(输入框等)不拦截
  const stamp = pasteStamp()
  const ups: UploadFile[] = files.map((f, i) => {
    const ext = f.name.split('.').pop() || 'png'
    const name = `粘贴_${stamp}${files.length > 1 ? `_${i + 1}` : ''}.${ext}`
    return { file: new File([f], name, { type: f.type }) }
  })
  queue.enqueue(targetDirectory.value, ups)
}

const stopCancel = queue.onCancelled(cancelDirectoryScan)
function dispose() {
  if (disposed) return
  disposed = true
  cancelDirectoryScan()
  stopCancel()
  queue.signal.removeEventListener('abort', dispose)
  if (fileInput.value) fileInput.value.value = ''
  if (dirInput.value) dirInput.value.value = ''
  dragging.value = false
  dragDepth = 0
}
queue.signal.addEventListener('abort', dispose, { once: true })
if (queue.signal.aborted) dispose()
onBeforeUnmount(dispose)
function pickFiles(target: string) { if (!disposed) { targetDirectory.value = target; fileInput.value?.click() } }
const supportsDirectory = typeof window !== 'undefined' && ('showDirectoryPicker' in window || 'webkitdirectory' in document.createElement('input'))
return { fileInput, dirInput, targetDirectory, pickFiles, supportsDirectory, onPick, onDirPick, pickDirectory, onDrop, onPaste, onDragEnter, onDragLeave, dragging, isFirefoxBrowser }
}
export type UploadIntake = ReturnType<typeof createUploadIntake>
