import { computed, onBeforeUnmount, onMounted, reactive, ref, shallowRef } from 'vue'
import { ElMessage } from 'element-plus'
import { api, errMsg, fmtSize, type UploadSession, type UploadSessionResponse } from '../api'
import { appConfirm } from '../dialog'
import { onSessionInvalidated } from '../session'

/** 一次入队的文件:rel 为相对子目录(文件夹上传时),普通上传为 '' */
export interface UploadFile {
  file: File
  rel?: string
}

/** 文件夹预扫描得到的轻量条目，不持有 File 句柄。 */
export interface UploadCandidate {
  name: string
  rel?: string
}

/** 登录会话内只有一个调度器，页面仅订阅队列状态。 */
export function createTransferController() {

type UploadStatus =
  | 'hashing'
  | 'waiting'
  | 'uploading'
  | 'paused'
  | 'needs_file'
  | 'cancelling'
  | 'instant'
  | 'done'
  | 'skipped'
  | 'error'

interface Item {
  id: number
  dir: string
  rel: string
  name: string
  size: number
  file?: File
  pct: number
  status: UploadStatus
  error?: string
  sha256?: string
  session?: UploadSession
  concurrency: number
  controllers: Set<AbortController>
  pauseRequested: boolean
  cancelled: boolean
  /** 每次暂停/继续/取消都会递增，令旧的异步上传轮次自动失效。 */
  generation: number
  /** undefined 表示尚未尝试生成，null 表示浏览器无法解码该视频。 */
  thumbnail?: Blob | null
  /** Telegram 共享限流冷却截止时间；只用于展示，不影响本地取消。 */
  telegramRetryUntil?: number
}

function isPauseable(item: Item) {
  return (
    !item.cancelled &&
    (item.status === 'hashing' || item.status === 'waiting' || item.status === 'uploading')
  )
}

function isPending(item: Item) {
  return !isDone(item) && item.status !== 'cancelling'
}

function isCurrent(item: Item, generation: number) {
  return !capacityClosed && !item.cancelled && item.generation === generation
}

const visible = ref(false)
const clock = ref(Date.now())
const items = shallowRef<Item[]>([])
const batchBusy = ref(false)
const preparingFileCount = ref(0)
const uploaded = new Set<(directory: string) => void>()
const cancelled = new Set<() => void>()
const notifyUploaded = (item: Item) => uploaded.forEach((listener) => listener(item.rel ? joinPath(item.dir, item.rel) : item.dir))
const onUploaded = (listener: (directory: string) => void) => { uploaded.add(listener); return () => uploaded.delete(listener) }
const onCancelled = (listener: () => void) => { cancelled.add(listener); return () => cancelled.delete(listener) }

const DISPLAY_PENDING_LIMIT = 30
const PREVIEW_LIMIT = 200
const itemById = new Map<number, Item>()
const displayedIds = ref<number[]>([])
const uploadingIds = new Set<number>()
const pendingDisplayIds = new Set<number>()
const recentCompletedIds: number[] = []
const displayRevision = ref(0)
const displayedItems = computed(() => {
  // Item 故意保持为普通对象，避免几万个任务各自创建 Vue Proxy。
  // 通过轻量版本号只刷新最多 200 个可见任务。
  void displayRevision.value
  return displayedIds.value.flatMap((id) => {
    const item = itemById.get(id)
    return item ? [item] : []
  })
})
const hiddenTaskCount = computed(() => Math.max(0, totalCount.value - displayedItems.value.length))

const statusCounts = reactive<Record<UploadStatus, number>>({
  hashing: 0,
  waiting: 0,
  uploading: 0,
  paused: 0,
  needs_file: 0,
  cancelling: 0,
  instant: 0,
  done: 0,
  skipped: 0,
  error: 0,
})

const deferredFileCount = ref(0)
const totalCount = computed(() => items.value.length + deferredFileCount.value)
const uploadedCount = computed(() => statusCounts.done)
const instantCount = computed(() => statusCounts.instant)
const successCount = computed(() => uploadedCount.value + instantCount.value)
const recentSuccessRevision = ref(0)
const recentSuccessAt: number[] = []
const recentSuccessCount = computed(() => {
  void recentSuccessRevision.value
  const cutoff = clock.value - 60_000
  return recentSuccessAt.reduce((count, timestamp) => count + (timestamp > cutoff ? 1 : 0), 0)
})
const skippedCount = computed(() => statusCounts.skipped)
const finishedExists = computed(() => successCount.value + skippedCount.value > 0)
const pendingCount = computed(
  () =>
    statusCounts.hashing +
    statusCounts.waiting +
    statusCounts.uploading +
    statusCounts.paused +
    statusCounts.needs_file +
    statusCounts.error +
    deferredFileCount.value,
)
const activeCount = computed(
  () =>
    statusCounts.hashing +
    statusCounts.waiting +
    statusCounts.uploading +
    deferredFileCount.value,
)
const pauseableCount = activeCount
const pausedCount = computed(() => statusCounts.paused)
const errorCount = computed(() => statusCounts.error)
const needsFileCount = computed(() => statusCounts.needs_file)
const cancellingCount = computed(() => statusCounts.cancelling)
const resumableCount = pausedCount
const retryableCount = errorCount

const MAX_BUFFERED_FILES = /Firefox\//i.test(navigator.userAgent) ? 100 : 600
let capacityClosed = false
const sessionController = new AbortController()
const capacityWaiters: Array<() => void> = []

function occupiesFileBuffer(status: UploadStatus) {
  return (
    status === 'hashing' ||
    status === 'waiting' ||
    status === 'uploading' ||
    status === 'paused' ||
    status === 'error'
  )
}

function bufferedFileCount() {
  return (
    statusCounts.hashing +
    statusCounts.waiting +
    statusCounts.uploading +
    statusCounts.paused +
    statusCounts.error
  )
}

function wakeCapacityWaiters() {
  for (const resolve of capacityWaiters.splice(0)) resolve()
}

function setDeferredFileCount(count: number) {
  if (!capacityClosed) deferredFileCount.value = Math.max(0, Math.floor(count))
}

function consumeDeferredFiles(count: number) {
  deferredFileCount.value = Math.max(0, deferredFileCount.value - count)
}

function clearDeferredFiles() {
  deferredFileCount.value = 0
}

/** 惰性目录遍历的背压阀：浏览器内最多保留约 600 个待处理文件句柄。 */
async function waitForCapacity(additional = 200): Promise<boolean> {
  const required = Math.max(1, additional)
  while (!capacityClosed && bufferedFileCount() + required > MAX_BUFFERED_FILES) {
    await new Promise<void>((resolve) => capacityWaiters.push(resolve))
  }
  return !capacityClosed
}

function recordSuccessfulUpload() {
  const now = Date.now()
  const cutoff = now - 60_000
  while (recentSuccessAt.length && recentSuccessAt[0]! <= cutoff) recentSuccessAt.shift()
  recentSuccessAt.push(now)
  recentSuccessRevision.value += 1
}

function isDone(item: Item) {
  return item.status === 'done' || item.status === 'instant' || item.status === 'skipped'
}

function telegramWaitSeconds(item: Item) {
  if (!item.telegramRetryUntil) return 0
  return Math.max(0, Math.ceil((item.telegramRetryUntil - clock.value) / 1000))
}

function applyTelegramRetryAfter(item: Item, seconds: number) {
  item.telegramRetryUntil = seconds > 0 ? Date.now() + seconds * 1000 : undefined
}
function displayName(item: Item) {
  if (item.session) return item.session.path.replace(/^\//, '')
  return item.rel ? `${item.rel}/${item.name}` : item.name
}

/** 卡片左侧色条:成功 / 失败 / 进行中 / 待处理 */
function tone(item: Item): 'ok' | 'err' | 'run' | 'idle' {
  if (item.status === 'error') return 'err'
  if (item.status === 'done' || item.status === 'instant') return 'ok'
  if (item.status === 'skipped') return 'idle'
  if (item.status === 'hashing' || item.status === 'uploading') return 'run'
  return 'idle'
}

let seq = 0
let busy = false
let defaultConcurrency = 1
let uploadLimit = 1
let activeUploads = 0
const uploadWaiters: Array<{
  ownerId: number
  resolve: (release: () => void) => void
}> = []
/**
 * 多分块文件是一次长时间的上游请求：它开始后独占上传通道，
 * 其他文件只保留在队列里，等该文件完成/失败/取消后再继续。
 */
let exclusiveChunkedOwner: number | null = null
let exclusiveChunkedWaiters = 0
const uploadGateWaiters: Array<() => void> = []
const uploadGateRevision = ref(0)
const chunkedExclusiveBusy = computed(() => {
  void uploadGateRevision.value
  return exclusiveChunkedOwner !== null || exclusiveChunkedWaiters > 0
})

function wakeUploadGate() {
  uploadGateRevision.value += 1
  const waiters = uploadGateWaiters.splice(0)
  for (const resolve of waiters) resolve()
}

function waitUploadGate(): Promise<void> {
  return new Promise((resolve) => uploadGateWaiters.push(resolve))
}

function isChunkedSession(session?: UploadSession) {
  return Boolean(session && session.part_count > 1)
}

function applySessionConcurrency(item: Item, response: { data: UploadSessionResponse }) {
  const chunked = isChunkedSession(response.data.session)
  // 多分块会话的 1 只是该会话的限制，不能把全局单块文件并发永久降成 1。
  if (!chunked) setUploadConcurrency(response.data.concurrency, response.data.bot_count)
  item.concurrency = chunked ? 1 : response.data.concurrency
}

/** 等待所有已有请求排空，再取得多分块文件的独占上传权。 */
async function acquireChunkedExclusive(item: Item, generation: number) {
  exclusiveChunkedWaiters += 1
  wakeUploadGate()
  try {
    while (
      isCurrent(item, generation) &&
      !item.pauseRequested &&
      (exclusiveChunkedOwner !== null || activeUploads > 0)
    ) {
      await waitUploadGate()
    }
    if (!isCurrent(item, generation) || item.pauseRequested) return null
    exclusiveChunkedOwner = item.id
    return () => {
      if (exclusiveChunkedOwner !== item.id) return
      exclusiveChunkedOwner = null
      drainUploadSlots()
      wakeUploadGate()
    }
  } finally {
    exclusiveChunkedWaiters = Math.max(0, exclusiveChunkedWaiters - 1)
    wakeUploadGate()
  }
}
/** 哈希保持串行；文件管线至少覆盖最多 20 个上传 Bot 的总并发。 */
const MAX_ACTIVE_FILES = 20

/** 多个文件及其分块共享同一额度，避免“文件并发 × 分块并发”放大请求数。 */
function drainUploadSlots() {
  while (activeUploads < uploadLimit) {
    const eligible = uploadWaiters.findIndex(({ ownerId }) =>
      exclusiveChunkedOwner === null
        ? exclusiveChunkedWaiters === 0
        : ownerId === exclusiveChunkedOwner,
    )
    if (eligible < 0) return
    const { resolve } = uploadWaiters.splice(eligible, 1)[0]!
    activeUploads += 1
    let released = false
    resolve(() => {
      if (released) return
      released = true
      activeUploads = Math.max(0, activeUploads - 1)
      drainUploadSlots()
      wakeUploadGate()
    })
  }
}

async function acquireUploadSlot(ownerId: number): Promise<() => void> {
  // 独占文件等待或占用期间，不再让其他文件开启新的请求；
  // 独占者自己仍可取得唯一的上传槽位。
  while (
    !capacityClosed && ((exclusiveChunkedOwner !== null && exclusiveChunkedOwner !== ownerId) ||
    (exclusiveChunkedOwner === null && exclusiveChunkedWaiters > 0))
  ) {
    await waitUploadGate()
  }
  if (capacityClosed) return () => {}
  return new Promise((resolve) => {
    uploadWaiters.push({ ownerId, resolve })
    drainUploadSlots()
  })
}

function setUploadConcurrency(value: number, botCount?: number) {
  if (capacityClosed) return
  const normalized = Math.max(1, Math.floor(value || 1))
  defaultConcurrency = normalized
  void botCount
  uploadLimit = normalized
  drainUploadSlots()
  wakeUploadGate()
}

/** 数组只追加、游标只前进，避免 shift/includes 在万级队列上退化为 O(n²)。 */
const queue: number[] = []
const queuedIds = new Set<number>()
let queueHead = 0
/** 本次页面会话已确保存在的目录，避免每个文件都重复调用 mkdir。 */
const ensuredDirs = new Set<string>()

function isDisplayPending(status: UploadStatus) {
  return status !== 'uploading' && status !== 'done' && status !== 'instant' && status !== 'skipped'
}

function refreshDisplayWindow() {
  const next: number[] = []
  for (const id of uploadingIds) {
    if (itemById.has(id)) next.push(id)
  }
  let pending = 0
  for (const id of pendingDisplayIds) {
    if (!itemById.has(id)) continue
    next.push(id)
    pending += 1
    if (pending >= DISPLAY_PENDING_LIMIT) break
  }
  for (const id of recentCompletedIds) if (itemById.has(id) && !next.includes(id)) next.push(id)
  displayedIds.value = next
}

let displayFrame = 0
function refreshDisplayed(item?: Item) {
  if (capacityClosed) return
  if (item && !displayedIds.value.includes(item.id)) return
  if (displayFrame) return
  displayFrame = window.requestAnimationFrame(() => {
    displayFrame = 0
    displayRevision.value += 1
  })
}

function setItemStatus(item: Item, status: UploadStatus) {
  if (capacityClosed || !itemById.has(item.id)) return
  if (item.status === status) return
  const previous = item.status
  statusCounts[previous] = Math.max(0, statusCounts[previous] - 1)
  statusCounts[status] += 1
  item.status = status
  if (status === 'done' || status === 'instant') recordSuccessfulUpload()
  if (isDone(item)) { recentCompletedIds.unshift(item.id); recentCompletedIds.splice(20) }
  if (previous === 'uploading') uploadingIds.delete(item.id)
  if (status === 'uploading') uploadingIds.add(item.id)
  if (isDisplayPending(previous) && !isDisplayPending(status)) pendingDisplayIds.delete(item.id)
  if (!isDisplayPending(previous) && isDisplayPending(status)) pendingDisplayIds.add(item.id)
  if (occupiesFileBuffer(previous) && !occupiesFileBuffer(status)) wakeCapacityWaiters()
  refreshDisplayWindow()
}

function registerItems(rawItems: Item[], prepend = true): Item[] {
  if (capacityClosed || !rawItems.length) return []
  // 队列对象不做深层响应式代理：File、Set、UploadSession 在万级规模下会造成
  // 明显的代理和依赖跟踪开销；界面只依赖上面的可见窗口刷新。
  const increments = new Map<UploadStatus, number>()
  for (const item of rawItems) {
    itemById.set(item.id, item)
    if (item.status === 'uploading') uploadingIds.add(item.id)
    if (isDisplayPending(item.status)) pendingDisplayIds.add(item.id)
    increments.set(item.status, (increments.get(item.status) ?? 0) + 1)
  }
  for (const [status, count] of increments) {
    statusCounts[status] += count
  }
  items.value = prepend ? [...rawItems, ...items.value] : [...items.value, ...rawItems]
  refreshDisplayWindow()
  return rawItems
}

function removeItems(targets: Item[]) {
  if (!targets.length) return
  const ids = new Set<number>()
  for (const item of targets) {
    if (!itemById.delete(item.id)) continue
    ids.add(item.id)
    queuedIds.delete(item.id)
    uploadingIds.delete(item.id)
    pendingDisplayIds.delete(item.id)
    statusCounts[item.status] = Math.max(0, statusCounts[item.status] - 1)
    // 从队列移除前释放大 File/Blob 引用，便于浏览器尽早回收内存。
    item.file = undefined
    item.thumbnail = undefined
    item.controllers.clear()
  }
  if (!ids.size) return
  items.value = items.value.filter((item) => !ids.has(item.id))
  refreshDisplayWindow()
}

function joinPath(dir: string, name: string) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`
}

function parentOf(path: string) {
  const index = path.lastIndexOf('/')
  return index <= 0 ? '/' : path.slice(0, index)
}

function sessionPct(session: UploadSession) {
  return session.size === 0 ? 0 : Math.round((session.uploaded_bytes / session.size) * 100)
}

function fromSession(session: UploadSession, concurrency: number): Item {
  return {
    id: ++seq,
    dir: parentOf(session.path),
    rel: '',
    name: session.name,
    size: session.size,
    pct: sessionPct(session),
    status: 'needs_file',
    error: '页面刷新后需要重新选择原文件，已完成分块不会重复上传',
    sha256: session.sha256,
    session,
    concurrency: isChunkedSession(session) ? 1 : concurrency,
    controllers: new Set(),
    pauseRequested: true,
    cancelled: false,
    generation: 0,
  }
}

// ---------- 重名冲突询问 ----------

type ConflictAction = 'overwrite' | 'skip' | 'cancel'
const conflictVisible = ref(false)
const conflictNames = ref<string[]>([])
const conflictTotal = ref(0)
let conflictResolve: ((action: ConflictAction) => void) | null = null

function askConflict(names: string[], total: number): Promise<ConflictAction> {
  if (capacityClosed) return Promise.resolve('cancel')
  // 新一轮询问不得遗留上一轮未决的 Promise。
  conflictResolve?.('cancel')
  conflictTotal.value = total
  conflictNames.value = names
  conflictVisible.value = true
  return new Promise((resolve) => (conflictResolve = resolve))
}

function resolveConflict(action: ConflictAction) {
  const resolve = conflictResolve
  conflictResolve = null
  conflictVisible.value = false
  resolve?.(action)
}

/** 点 X 或按 ESC 关闭对话框视为取消上传。 */
function onConflictClose() {
  if (conflictResolve) resolveConflict('cancel')
}

/**
 * 文件夹上传开始前一次性查重并取得用户决策。这里只处理路径字符串，
 * 不创建 File 对象；返回值会被之后所有流式批次复用。
 */
async function resolveUploadConflicts(
  dir: string,
  candidates: UploadCandidate[],
): Promise<ConflictAction> {
  if (capacityClosed) return 'cancel'
  visible.value = true
  preparingFileCount.value += candidates.length
  try {
    const relOf = (candidate: UploadCandidate) =>
      (candidate.rel ?? '').replace(/^\/+|\/+$/g, '')
    const groups = new Map<string, UploadCandidate[]>()
    for (let index = 0; index < candidates.length; index += 1) {
      const candidate = candidates[index]!
      const rel = relOf(candidate)
      const target = rel ? joinPath(dir, rel) : dir
      const group = groups.get(target)
      if (group) group.push(candidate)
      else groups.set(target, [candidate])
      if (index > 0 && index % 500 === 0) {
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
      }
    }

    const conflictPreview: string[] = []
    let conflictTotal = 0
    const grouped = [...groups.entries()]
    let groupCursor = 0
    const worker = async () => {
      while (!capacityClosed) {
        const current = grouped[groupCursor++]
        if (!current) return
        const [target, group] = current
        for (let start = 0; start < group.length && !capacityClosed; start += 500) {
          const batch = group.slice(start, start + 500)
          try {
            const response = await api.exists(
              target,
              batch.map((candidate) => candidate.name),
              sessionController.signal,
            )
            if (capacityClosed) return
            const existing = new Set(response.data.existing)
            for (const candidate of batch) {
              if (!existing.has(candidate.name)) continue
              conflictTotal += 1
              if (conflictPreview.length < PREVIEW_LIMIT) {
                const rel = relOf(candidate)
                conflictPreview.push(rel ? `${rel}/${candidate.name}` : candidate.name)
              }
            }
          } catch {
            // 预检查失败时按无冲突继续，真正上传仍由后端保证一致性。
          }
        }
      }
    }
    await Promise.all(Array.from({ length: Math.min(4, grouped.length) }, () => worker()))
    if (capacityClosed) return 'cancel'
    if (!conflictTotal) return 'overwrite'
    return await askConflict(conflictPreview, conflictTotal)
  } finally {
    preparingFileCount.value = Math.max(0, preparingFileCount.value - candidates.length)
  }
}

/** 入队前按目标目录批量查重，有同名文件时让用户选择覆盖、跳过或取消。 */
async function enqueue(
  dir: string,
  files: UploadFile[],
  preferredConflictAction?: Exclude<ConflictAction, 'cancel'>,
  fromDeferredSource = false,
): Promise<ConflictAction | undefined> {
  if (capacityClosed) return 'cancel'
  visible.value = true
  preparingFileCount.value += files.length
  try {
    return await prepareEnqueue(dir, files, preferredConflictAction, fromDeferredSource)
  } finally {
    preparingFileCount.value = Math.max(0, preparingFileCount.value - files.length)
  }
}

async function prepareEnqueue(
  dir: string,
  files: UploadFile[],
  preferredConflictAction?: Exclude<ConflictAction, 'cancel'>,
  fromDeferredSource = false,
): Promise<ConflictAction | undefined> {
  const relOf = (upload: UploadFile) => (upload.rel ?? '').replace(/^\/+|\/+$/g, '')
  const duplicates = new Set<UploadFile>()
  // 已选择“覆盖”时不再在上传过程中重复查重；“跳过”仍需静默判断每批哪些文件跳过。
  if (preferredConflictAction !== 'overwrite') {
    const groups = new Map<string, UploadFile[]>()
    for (let index = 0; index < files.length; index += 1) {
      const upload = files[index]!
      const rel = relOf(upload)
      const target = rel ? joinPath(dir, rel) : dir
      const group = groups.get(target)
      if (group) group.push(upload)
      else groups.set(target, [upload])
      if (index > 0 && index % 500 === 0) {
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
      }
    }
    const grouped = [...groups.entries()]
    let groupCursor = 0
    const duplicateWorker = async () => {
      while (!capacityClosed) {
        const current = grouped[groupCursor++]
        if (!current) return
        const [target, group] = current
        for (let start = 0; start < group.length && !capacityClosed; start += 500) {
          const batch = group.slice(start, start + 500)
          try {
            const response = await api.exists(
              target,
              batch.map((upload) => upload.file.name),
              sessionController.signal,
            )
            if (capacityClosed) return
            const existing = new Set(response.data.existing)
            for (const upload of batch) if (existing.has(upload.file.name)) duplicates.add(upload)
          } catch {
            // 查重只是交互增强，短暂失败时仍允许后续上传接口处理。
          }
        }
      }
    }
    await Promise.all(
      Array.from({ length: Math.min(4, grouped.length) }, () => duplicateWorker()),
    )
  }

  let action: ConflictAction = preferredConflictAction ?? 'overwrite'
  let selectedAction: ConflictAction | undefined
  if (duplicates.size > 0) {
    const conflictPreview: string[] = []
    let conflictCount = 0
    for (const upload of files) {
      if (!duplicates.has(upload)) continue
      conflictCount += 1
      if (conflictPreview.length < PREVIEW_LIMIT) {
        conflictPreview.push(
          relOf(upload) ? `${relOf(upload)}/${upload.file.name}` : upload.file.name,
        )
      }
    }
    if (preferredConflictAction === undefined) {
      action = await askConflict(conflictPreview, conflictCount)
      selectedAction = action
    }
    if (action === 'cancel') return action
  }

  if (capacityClosed) return 'cancel'
  const rawItems: Item[] = []
  for (let index = 0; index < files.length; index += 1) {
    const upload = files[index]!
    const item: Item = {
      id: ++seq,
      dir,
      rel: relOf(upload),
      name: upload.file.name,
      size: upload.file.size,
      file: upload.file,
      pct: 0,
      status: 'waiting',
      concurrency: defaultConcurrency,
      controllers: new Set(),
      pauseRequested: false,
      cancelled: false,
      generation: 0,
    }
    if (action === 'skip' && duplicates.has(upload)) {
      item.status = 'skipped'
      item.pct = 100
      // 跳过项不再需要持有浏览器文件句柄。
      item.file = undefined
    }
    rawItems.push(item)
    if (index > 0 && index % 500 === 0) {
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
    }
  }
  // 与任务注册放在同一个同步片段内，保证“实体任务 + 尚未读取任务”总数不跳变。
  if (fromDeferredSource) consumeDeferredFiles(rawItems.length)
  const registered = registerItems(rawItems)
  scheduleMany(registered, (item) => item.status === 'waiting')
  return selectedAction
}

function schedule(item: Item) {
  if (capacityClosed) return
  if (!queuedIds.has(item.id)) {
    queuedIds.add(item.id)
    queue.push(item.id)
  }
  void pump()
}

function scheduleMany(batch: Item[], shouldSchedule: (item: Item) => boolean = () => true) {
  let scheduled = false
  for (const item of batch) {
    if (!shouldSchedule(item)) continue
    if (queuedIds.has(item.id)) continue
    queuedIds.add(item.id)
    queue.push(item.id)
    scheduled = true
  }
  if (scheduled) void pump()
}

function removeQueued(id: number) {
  // 采用惰性删除，pump 取出 ID 时会跳过；不再移动后续数万个元素。
  queuedIds.delete(id)
}

/** 停止当前任务的本地工作；服务端会话保留，供稍后断点续传。 */
function stopItem(item: Item) {
  item.generation += 1
  item.pauseRequested = true
  removeQueued(item.id)
  for (const controller of item.controllers) controller.abort()
  item.controllers.clear()
  item.telegramRetryUntil = undefined
  wakeUploadGate()
}

function pause(item: Item) {
  if (capacityClosed) return
  if (!isPauseable(item)) return
  stopItem(item)
  if (!item.cancelled) setItemStatus(item, 'paused')
}

function resume(item: Item) {
  if (capacityClosed) return
  if (item.cancelled || isDone(item) || item.status === 'cancelling') return
  if (!item.file) {
    chooseFile(item)
    return
  }
  item.generation += 1
  item.pauseRequested = false
  item.error = undefined
  setItemStatus(item, 'waiting')
  schedule(item)
}

function retry(item: Item) {
  if (capacityClosed) return
  if (item.cancelled || isDone(item) || item.status === 'cancelling') return
  if (item.session && item.session.uploaded_parts.length === item.session.part_count) {
    item.generation += 1
    item.pauseRequested = false
    item.error = undefined
    setItemStatus(item, 'waiting')
    schedule(item)
    return
  }
  if (!item.file) {
    chooseFile(item)
    return
  }
  resume(item)
}

function pauseAll() {
  if (batchBusy.value) return
  const targets = items.value.filter(isPauseable)
  targets.forEach(pause)
  if (targets.length) ElMessage.info(`已暂停 ${targets.length} 个上传任务`)
}

function resumeAll() {
  if (batchBusy.value) return
  const targets = items.value.filter((item) => item.status === 'paused' && Boolean(item.file))
  targets.forEach(resume)
  if (targets.length) {
    ElMessage.success(`已继续 ${targets.length} 个上传任务`)
  } else if (pausedCount.value) {
    ElMessage.info('刷新页面后暂停任务需要重新选择原文件')
  }
}

function retryAll() {
  if (batchBusy.value) return
  const targets = items.value.filter((item) => item.status === 'error' && Boolean(item.file))
  targets.forEach(retry)
  if (targets.length) ElMessage.success(`已重新排队 ${targets.length} 个失败任务`)
}

async function cancelItem(item: Item, removeOnSuccess = true): Promise<boolean> {
  if (capacityClosed) return false
  if (isDone(item) || item.status === 'cancelling') return true
  item.cancelled = true
  stopItem(item)
  setItemStatus(item, 'cancelling')
  try {
    if (item.session) await api.uploads.cancel(item.session.id, sessionController.signal)
    if (removeOnSuccess) removeItems([item])
    return true
  } catch (error) {
    const status = (error as { response?: { status?: number } }).response?.status
    if (status === 404) {
      if (removeOnSuccess) removeItems([item])
      return true
    }
    item.cancelled = false
    setItemStatus(item, 'error')
    item.error = `取消失败：${errMsg(error)}`
    return false
  }
}

async function cancelAll() {
  if (batchBusy.value) return
  const targets = items.value.filter(isPending)
  const pendingTotal = pendingCount.value
  const deferredAtStart = deferredFileCount.value
  if (!pendingTotal) return
  try {
    await appConfirm({
      title: '取消全部上传',
      message: `确定取消 ${pendingTotal} 个未完成上传吗？已上传的 Telegram 分块会进入清理队列，任务将从队列移除。`,
      kind: 'danger',
      confirmText: '全部取消',
      cancelText: '保留任务',
    })
  } catch {
    return
  }

  if (capacityClosed) return
  // 同时停止仍在惰性枚举的文件夹，避免取消后又有新任务进入队列。
  cancelled.forEach((listener) => listener())
  clearDeferredFiles()

  batchBusy.value = true
  let succeeded = 0
  let failed = 0
  const removed: Item[] = []
  try {
    // 逐个请求，避免同时删除大量分块时给 SQLite/Telegram 清理队列造成突发压力。
    for (const item of targets) {
      if (capacityClosed) break
      if (await cancelItem(item, false)) {
        succeeded += 1
        removed.push(item)
      } else {
        failed += 1
      }
    }
  } finally {
    removeItems(removed)
    batchBusy.value = false
  }
  if (failed) {
    ElMessage.warning(`已取消 ${succeeded} 个任务，另有 ${failed} 个取消失败，请单独重试`)
  } else if (succeeded || deferredAtStart) {
    ElMessage.success(`已取消 ${succeeded + deferredAtStart} 个上传任务`)
  }
}

function clearFinished() {
  removeItems(items.value.filter(isDone))
}

/** 从顶栏打开上传队列；没有任务时也允许打开查看空状态。 */
function open() {
  if (!capacityClosed) visible.value = true
}

type HashWorkerMessage =
  | { type: 'progress'; id: number; percentage: number }
  | { type: 'done'; id: number; digest: string }
  | { type: 'cancelled'; id: number }
  | { type: 'error'; id: number; message: string }

interface PendingHash {
  onPct: (percentage: number) => void
  resolve: (digest: string) => void
  reject: (error: Error) => void
  stopTimer: number
}

const hashWorker = new Worker(new URL('../workers/sha256.worker.ts', import.meta.url), {
  type: 'module',
})
const pendingHashes = new Map<number, PendingHash>()
let hashSequence = 0

function finishHash(id: number, result: { digest?: string; error?: Error }) {
  const pending = pendingHashes.get(id)
  if (!pending) return
  pendingHashes.delete(id)
  window.clearInterval(pending.stopTimer)
  if (result.digest !== undefined) pending.resolve(result.digest)
  else pending.reject(result.error ?? new Error('文件校验失败'))
}

hashWorker.onmessage = (event: MessageEvent<HashWorkerMessage>) => {
  const message = event.data
  const pending = pendingHashes.get(message.id)
  if (!pending) return
  if (message.type === 'progress') pending.onPct(message.percentage)
  else if (message.type === 'done') finishHash(message.id, { digest: message.digest })
  else if (message.type === 'cancelled') finishHash(message.id, { error: new Error('任务已停止') })
  else finishHash(message.id, { error: new Error(message.message) })
}

hashWorker.onerror = () => {
  for (const id of [...pendingHashes.keys()]) {
    finishHash(id, { error: new Error('文件校验线程异常退出') })
  }
}

/** SHA-256 在独立 Worker 中流式计算，避免 WASM 占用页面主线程。 */
async function sha256File(
  file: File,
  onPct: (percentage: number) => void,
  stopped: () => boolean = () => false,
): Promise<string> {
  if (stopped()) throw new Error('任务已停止')
  const id = ++hashSequence
  return new Promise((resolve, reject) => {
    const stopTimer = window.setInterval(() => {
      if (!stopped()) return
      hashWorker.postMessage({ type: 'cancel', id })
      finishHash(id, { error: new Error('任务已停止') })
    }, 100)
    pendingHashes.set(id, { onPct, resolve, reject, stopTimer })
    hashWorker.postMessage({ type: 'hash', id, file })
  })
}

const THUMB_VIDEO_EXT = new Set(['mp4', 'webm', 'mov', 'm4v', 'mkv', 'ogv'])
const TELEGRAM_THUMB_MAX_BYTES = 200 * 1024

function isVideoFile(file: File) {
  const ext = file.name.split('.').pop()?.toLowerCase() ?? ''
  return file.type.startsWith('video/') || THUMB_VIDEO_EXT.has(ext)
}

function canvasToJpeg(canvas: HTMLCanvasElement, quality: number): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', quality))
}

/** Telegram 只接受不超过 320×320、大小小于 200KB 的 JPEG 媒体缩略图。 */
async function encodeTelegramThumbnail(canvas: HTMLCanvasElement): Promise<Blob | null> {
  for (const quality of [0.82, 0.7, 0.58, 0.46, 0.34]) {
    const image = await canvasToJpeg(canvas, quality)
    if (image && image.size < TELEGRAM_THUMB_MAX_BYTES) return image
  }
  return null
}

/** 从浏览器本地文件取一帧并压成小图；object URL 不会产生任何网络下载。 */
function createVideoThumbnail(file: File): Promise<Blob | null> {
  if (capacityClosed || !isVideoFile(file)) return Promise.resolve(null)
  return new Promise((resolve) => {
    const url = URL.createObjectURL(file)
    const video = document.createElement('video')
    let finished = false
    let capturing = false
    let timer = 0

    const finish = (image: Blob | null) => {
      if (finished) return
      finished = true
      window.clearTimeout(timer)
      sessionController.signal.removeEventListener('abort', abortThumbnail)
      video.pause()
      video.removeAttribute('src')
      video.load()
      URL.revokeObjectURL(url)
      resolve(image)
    }

    const abortThumbnail = () => finish(null)
    sessionController.signal.addEventListener('abort', abortThumbnail, { once: true })

    const capture = () => {
      if (finished || capturing || video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) return
      const width = video.videoWidth
      const height = video.videoHeight
      if (!width || !height) return finish(null)
      capturing = true
      const scale = Math.min(1, 320 / width, 320 / height)
      const canvas = document.createElement('canvas')
      canvas.width = Math.max(1, Math.round(width * scale))
      canvas.height = Math.max(1, Math.round(height * scale))
      const context = canvas.getContext('2d')
      if (!context) return finish(null)
      try {
        context.drawImage(video, 0, 0, canvas.width, canvas.height)
      } catch {
        finish(null)
        return
      }
      void encodeTelegramThumbnail(canvas).then(finish).catch(() => finish(null))
    }

    video.preload = 'auto'
    video.muted = true
    video.playsInline = true
    video.onloadedmetadata = () => {
      const duration = Number.isFinite(video.duration) ? video.duration : 0
      const target = duration > 0.2 ? Math.min(1, duration * 0.05) : 0
      if (target > 0.01) {
        try {
          video.currentTime = target
        } catch {
          capture()
        }
      } else {
        capture()
      }
    }
    video.onloadeddata = () => {
      if (video.currentTime <= 0.01) capture()
    }
    video.onseeked = capture
    video.onerror = () => finish(null)
    timer = window.setTimeout(() => finish(null), 12_000)
    video.src = url
    video.load()
  })
}

/** 目标目录 = 基目录 + 相对子路径；上传文件夹时按需递归创建。 */
async function ensureDir(item: Item): Promise<string> {
  const target = item.rel ? joinPath(item.dir, item.rel) : item.dir
  if (item.rel && !ensuredDirs.has(target)) {
    await api.mkdir(target, sessionController.signal)
    if (!capacityClosed) ensuredDirs.add(target)
  }
  return target
}

/** 完成响应丢失后核实文件是否已经原子落位，避免重复上传。 */
async function confirmCompleted(
  target: string,
  item: Item,
  sha: string,
  stopped: () => boolean = () => false,
): Promise<boolean | null> {
  const deadline = Date.now() + 5 * 60 * 1000
  while (Date.now() < deadline) {
    if (stopped()) return null
    let state: { linked: boolean; pending: boolean }
    try {
      state = (await api.instant(target, item.name, sha, item.size, sessionController.signal)).data
    } catch {
      // 核实请求也可能遇到同一段短暂网络故障，稍后再问，不能据此判定上传失败。
      await new Promise((resolve) => window.setTimeout(resolve, 1000))
      continue
    }
    if (state.linked) return true
    if (!state.pending) return false
    if (stopped()) return null
    await new Promise((resolve) => window.setTimeout(resolve, 1000))
  }
  return null
}

function partSize(session: UploadSession, index: number) {
  const start = index * session.chunk_size
  return Math.min(session.chunk_size, session.size - start)
}

async function syncSession(item: Item) {
  if (!item.session) return
  const response = await api.uploads.status(item.session.id, sessionController.signal)
  if (capacityClosed) return
  applySessionConcurrency(item, response)
  applyTelegramRetryAfter(item, response.data.retry_after)
  item.session = response.data.session
  item.pct = sessionPct(item.session)
}

async function finalizeItem(item: Item, session: UploadSession, generation: number) {
  if (!isCurrent(item, generation) || item.pauseRequested) return
  try {
    await api.uploads.complete(session.id, sessionController.signal)
  } catch (completeError) {
    const confirmed = await confirmCompleted(
      item.dir,
      item,
      item.sha256 ?? session.sha256,
      () => !isCurrent(item, generation),
    )
    if (confirmed !== true) throw completeError
  }
  if (!isCurrent(item, generation) || item.pauseRequested) return
  setItemStatus(item, 'done')
  item.pct = 100
  item.session = undefined
  item.file = undefined
  item.thumbnail = undefined
  notifyUploaded(item)
}

async function uploadMissingParts(item: Item, generation: number) {
  if (!item.session || !item.file) return
  const needsExclusive = isChunkedSession(item.session)
  const releaseExclusive = needsExclusive
    ? await acquireChunkedExclusive(item, generation)
    : null
  if (needsExclusive && !releaseExclusive) {
    if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
    return
  }
  try {
    await syncSession(item)
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
    return
  }
  const session = item.session
  const uploaded = new Set(session.uploaded_parts)
  const missing = Array.from({ length: session.part_count }, (_, index) => index).filter(
    (index) => !uploaded.has(index),
  )
  let completedBytes = session.uploaded_bytes
  const activeBytes = new Map<number, number>()
  let cursor = 0
  let stopped = false
  let failure: unknown
  let lastProgressAt = 0

  const updateProgress = (force = false) => {
    if (!item.session || item.session.size === 0) return
    const now = performance.now()
    if (!force && now - lastProgressAt < 150) return
    lastProgressAt = now
    const active = [...activeBytes.values()].reduce((sum, value) => sum + value, 0)
    item.pct = Math.min(99, Math.round(((completedBytes + active) / item.session.size) * 100))
    refreshDisplayed(item)
  }

  setItemStatus(item, 'uploading')
  const isUploadInterrupted = (error: unknown) =>
    (error as { response?: { data?: { code?: string } } }).response?.data?.code ===
    'upload_interrupted'
  const worker = async () => {
    while (!stopped && isCurrent(item, generation) && !item.pauseRequested) {
      const index = missing[cursor++]
      if (index === undefined) return
      let restartRetries = 0
      for (;;) {
        const release = await acquireUploadSlot(item.id)
        if (stopped || !isCurrent(item, generation) || item.pauseRequested) {
          release()
          return
        }
        // 取得全局槽位后才创建 Blob，万级队列不会同时持有大量切片对象。
        const start = index * session.chunk_size
        const data = item.file!.slice(start, start + partSize(session, index))
        const controller = new AbortController()
        item.controllers.add(controller)
        activeBytes.set(index, 0)
        let retryInterrupted = false
        try {
          await api.uploads.part(
            session.id,
            index,
            data,
            (loaded) => {
              if (!isCurrent(item, generation)) return
              activeBytes.set(index, Math.min(loaded, data.size))
              updateProgress()
            },
            controller.signal,
          )
          if (!isCurrent(item, generation)) return
          completedBytes += data.size
          activeBytes.delete(index)
          item.error = undefined
          updateProgress(true)
        } catch (error) {
          activeBytes.delete(index)
          if (
            isUploadInterrupted(error) &&
            restartRetries < 5 &&
            isCurrent(item, generation) &&
            !item.pauseRequested
          ) {
            restartRetries += 1
            retryInterrupted = true
          } else if (
            !stopped &&
            isCurrent(item, generation) &&
            !item.pauseRequested
          ) {
            failure = error
            stopped = true
            for (const active of item.controllers) active.abort()
          }
        } finally {
          item.controllers.delete(controller)
          release()
        }
        if (!retryInterrupted) break
        // 后端已确认共享 Bot API 完成重启且释放了分块声明，此错误才允许自动重试。
        await new Promise((resolve) => window.setTimeout(resolve, 400))
        if (stopped || !isCurrent(item, generation) || item.pauseRequested) return
      }
    }
  }

  // 多分块文件固定单并发；单块文件仍按服务端返回的总并发上传。
  const workerCount = needsExclusive
    ? 1
    : Math.max(1, Math.min(item.concurrency, missing.length || 1))
  const refreshTelegramWait = async () => {
    if (!item.session || !isCurrent(item, generation) || item.pauseRequested) return
    try {
      const response = await api.uploads.status(item.session.id, sessionController.signal)
      if (!isCurrent(item, generation)) return
      applySessionConcurrency(item, response)
      applyTelegramRetryAfter(item, response.data.retry_after)
    } catch {
      // 状态提示失败不影响正在进行的上传请求。
    }
  }
  const retryPollTimer = window.setInterval(() => void refreshTelegramWait(), 2000)
  try {
    await Promise.all(Array.from({ length: workerCount }, worker))
  } finally {
    window.clearInterval(retryPollTimer)
  }
  if (!isCurrent(item, generation)) return
  try {
    await syncSession(item)
  } catch {
    // 会话可能恰好在完成请求中消失，后面的秒传核实会给出最终状态。
  }
  if (item.pauseRequested) {
    setItemStatus(item, 'paused')
    return
  }
  if (failure) throw failure

  await finalizeItem(item, session, generation)
  } finally {
    releaseExclusive?.()
  }
}

async function processItem(item: Item) {
  const generation = item.generation
  if (!isCurrent(item, generation)) return
  if (item.pauseRequested) {
    setItemStatus(item, 'paused')
    return
  }
  if (
    item.session &&
    item.session.uploaded_parts.length === item.session.part_count
  ) {
    await finalizeItem(item, item.session, generation)
    return
  }
  if (!item.file) {
    setItemStatus(item, 'needs_file')
    return
  }
  const target = item.session ? item.dir : await ensureDir(item)
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
    return
  }

  if (!item.session) {
    setItemStatus(item, 'hashing')
    item.sha256 = await sha256File(
      item.file,
      (percentage) => {
        item.pct = percentage
        refreshDisplayed(item)
      },
      () => !isCurrent(item, generation) || item.pauseRequested,
    )
    if (item.thumbnail === undefined) item.thumbnail = await createVideoThumbnail(item.file)
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
      return
    }

    try {
      const instant = await api.instant(target, item.name, item.sha256, item.size, sessionController.signal)
      if (!isCurrent(item, generation) || item.pauseRequested) {
        if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
        return
      }
      if (instant.data.linked) {
        setItemStatus(item, 'instant')
        item.pct = 100
        item.file = undefined
        item.thumbnail = undefined
        notifyUploaded(item)
        return
      }
    } catch {
      // 秒传探测失败时继续创建普通上传会话。
    }
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
      return
    }

    const created = await api.uploads.create(target, item.name, item.size, item.sha256, sessionController.signal)
    // 退出会话后保留服务器的续传任务，不再接管或自动删除迟到的创建响应。
    if (capacityClosed) return
    applySessionConcurrency(item, created)
    applyTelegramRetryAfter(item, created.data.retry_after)
    item.session = created.data.session
    item.dir = parentOf(item.session.path)
    item.rel = ''
    item.pct = sessionPct(item.session)
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (item.cancelled) await api.uploads.cancel(item.session.id, sessionController.signal).catch(() => {})
      else if (isCurrent(item, generation)) setItemStatus(item, 'paused')
      return
    }
  }
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
    return
  }
  if (item.thumbnail && item.session && !item.session.uploaded_parts.includes(0)) {
    await api.uploads.thumbnail(item.session.id, item.thumbnail, sessionController.signal).catch(() => {
      // 缩略图失败不应阻塞文件本体上传。
    })
  }
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) setItemStatus(item, 'paused')
    return
  }
  await uploadMissingParts(item, generation)
}

async function pump() {
  if (capacityClosed || busy) return
  busy = true
  const fileWorker = async () => {
    while (queueHead < queue.length) {
      const itemId = queue[queueHead++]!
      if (!queuedIds.delete(itemId)) continue
      const item = itemById.get(itemId)
      if (!item || item.cancelled) continue
      const generation = item.generation
      try {
        await processItem(item)
      } catch (error) {
        if (isCurrent(item, generation) && !item.pauseRequested) {
          setItemStatus(item, 'error')
          item.error = errMsg(error)
        }
      }
    }
  }
  try {
    const workerCount = Math.max(1, Math.min(defaultConcurrency, MAX_ACTIVE_FILES))
    await Promise.all(Array.from({ length: workerCount }, () => fileWorker()))
  } finally {
    busy = false
    if (queueHead >= queue.length) {
      queue.length = 0
      queueHead = 0
    }
    // worker 刚退出时若又有任务入队，主动开启下一轮，避免任务滞留。
    if (queueHead < queue.length) void pump()
  }
}

function chooseFile(item: Item) {
  if (capacityClosed) return
  const input = document.createElement('input')
  input.type = 'file'
  input.onchange = () => {
    const file = input.files?.[0]
    if (file) void attachFile(item, file)
  }
  input.click()
}

async function attachFile(item: Item, file: File) {
  if (capacityClosed || !itemById.has(item.id) || !item.session) return
  if (file.size !== item.session.size) {
    setItemStatus(item, 'needs_file')
    item.error = `文件大小不一致，应为 ${item.session.size} 字节`
    return
  }
  item.file = file
  setItemStatus(item, 'hashing')
  item.error = undefined
  item.generation += 1
  const generation = item.generation
  item.pauseRequested = false
  item.cancelled = false
  try {
    const digest = await sha256File(
      file,
      (percentage) => {
        item.pct = percentage
        refreshDisplayed(item)
      },
      () => !isCurrent(item, generation) || item.pauseRequested,
    )
    if (!isCurrent(item, generation)) {
      if (item.cancelled) return
      if (item.pauseRequested) {
        item.file = undefined
        setItemStatus(item, 'needs_file')
        item.error = '校验已暂停，请重新选择原文件继续'
      }
      return
    }
    if (item.pauseRequested) {
      // 旧会话自带 sha256；校验尚未完成时不能把未验证的文件交给续传流程。
      item.file = undefined
      setItemStatus(item, 'needs_file')
      item.error = '校验已暂停，请重新选择原文件继续'
      return
    }
    if (digest !== item.session.sha256) {
      item.file = undefined
      setItemStatus(item, 'needs_file')
      item.pct = sessionPct(item.session)
      item.error = '所选文件内容与原任务不一致，请选择原文件'
      return
    }
    item.sha256 = digest
    item.thumbnail = await createVideoThumbnail(file)
    if (!isCurrent(item, generation)) return
    if (item.pauseRequested) {
      setItemStatus(item, 'paused')
      return
    }
    resume(item)
  } catch (error) {
    if (item.cancelled) return
    if (item.pauseRequested || item.generation !== generation) {
      item.file = undefined
      setItemStatus(item, 'needs_file')
      item.error = '校验已暂停，请重新选择原文件继续'
      return
    }
    item.file = undefined
    setItemStatus(item, 'needs_file')
    item.error = errMsg(error)
  }
}

let clockTimer = 0
const syncError = ref('')
const syncing = ref(false)
/** 返回前台时核对服务器进度；不盲目重试写入，也不覆盖正在上传的本地状态。 */
async function reconcile() {
  if (capacityClosed || syncing.value || document.hidden) return
  syncing.value = true
  syncError.value = ''
  try {
    const response = await api.uploads.list(sessionController.signal)
    if (capacityClosed) return
    setUploadConcurrency(response.data.concurrency, response.data.bot_count)
    const sessions = new Map(response.data.sessions.map((session) => [session.id, session]))
    for (const item of items.value) {
      if (!item.session || isPauseable(item) || item.status === 'cancelling') continue
      const session = sessions.get(item.session.id)
      if (!session) continue
      item.session = session
      item.pct = sessionPct(session)
      applyTelegramRetryAfter(item, response.data.retry_after)
      refreshDisplayed(item)
    }
  } catch { if (!capacityClosed) syncError.value = '暂时无法核实服务器进度，请检查网络后重试。' }
  finally { syncing.value = false }
}
const onForeground = () => { if (!document.hidden) void reconcile() }

onMounted(async () => {
  if (capacityClosed) return
  document.addEventListener('visibilitychange', onForeground)
  window.addEventListener('online', onForeground)
  clockTimer = window.setInterval(() => (clock.value = Date.now()), 1000)
  try {
    const response = await api.uploads.list(sessionController.signal)
    if (capacityClosed) return
    setUploadConcurrency(response.data.concurrency, response.data.bot_count)
    if (response.data.sessions.length) {
      const restored: Item[] = []
      for (const session of response.data.sessions) {
        const item = fromSession(session, response.data.concurrency)
        applyTelegramRetryAfter(item, response.data.retry_after)
        if (session.uploaded_parts.length === session.part_count) {
          item.status = 'waiting'
          item.error = undefined
          item.pauseRequested = false
        }
        restored.push(item)
      }
      const registered = registerItems(restored, false)
      scheduleMany(registered, (item) => item.status === 'waiting')
      visible.value = true
    }
  } catch {
    if (!capacityClosed) syncError.value = '无法读取可续传任务，请检查网络后核实服务器进度。'
  }
})

function dispose() {
  if (capacityClosed) return
  capacityClosed = true
  sessionController.abort()
  document.removeEventListener('visibilitychange', onForeground)
  window.removeEventListener('online', onForeground)
  window.clearInterval(clockTimer)
  if (displayFrame) window.cancelAnimationFrame(displayFrame)
  wakeCapacityWaiters()
  for (const { resolve } of uploadWaiters.splice(0)) resolve(() => {})
  exclusiveChunkedOwner = null
  exclusiveChunkedWaiters = 0
  wakeUploadGate()
  for (const item of items.value) {
    item.pauseRequested = true
    item.generation++
    item.file = undefined
    item.thumbnail = undefined
    for (const controller of item.controllers) controller.abort()
    item.controllers.clear()
  }
  for (const id of [...pendingHashes.keys()]) {
    finishHash(id, { error: new Error('页面已关闭') })
  }
  resolveConflict('cancel')
  cancelled.forEach((listener) => listener())
  uploaded.clear(); cancelled.clear()
  items.value = []; itemById.clear()
  uploadingIds.clear(); pendingDisplayIds.clear(); recentCompletedIds.length = 0
  displayedIds.value = []; recentSuccessAt.length = 0
  recentSuccessRevision.value++
  displayRevision.value++
  conflictNames.value = []; conflictTotal.value = 0
  syncError.value = ''; syncing.value = false; batchBusy.value = false
  for (const status of Object.keys(statusCounts) as UploadStatus[]) statusCounts[status] = 0
  preparingFileCount.value = 0; clearDeferredFiles()
  queue.length = 0; queuedIds.clear(); ensuredDirs.clear()
  visible.value = false
  hashWorker.terminate()
}
const stopInvalidation = onSessionInvalidated(dispose)
onBeforeUnmount(() => { stopInvalidation(); dispose() })

return { signal: sessionController.signal, reconcile, syncError, syncing, visible, batchBusy, totalCount, successCount, skippedCount, errorCount, recentSuccessCount, uploadedCount, instantCount, activeCount, pausedCount, needsFileCount, cancellingCount, pendingCount, pauseableCount, resumableCount, retryableCount, preparingFileCount, chunkedExclusiveBusy, hiddenTaskCount, DISPLAY_PENDING_LIMIT, displayedItems, finishedExists, clearFinished, pauseAll, resumeAll, retryAll, cancelAll, tone, displayName, isDone, telegramWaitSeconds, isPauseable, pause, chooseFile, resume, retry, cancelItem, conflictVisible, conflictTotal, conflictNames, onConflictClose, resolveConflict, enqueue, open, resolveUploadConflicts, waitForCapacity, setDeferredFileCount, clearDeferredFiles, onUploaded, onCancelled }
}
export type TransferController = ReturnType<typeof createTransferController>
