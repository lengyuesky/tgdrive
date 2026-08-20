<template>
  <el-drawer v-model="visible" size="400px" :close-on-click-modal="true">
    <template #header>
      <div class="head">
        <b>上传队列</b>
        <el-tag v-if="totalCount" size="small" type="info">
          {{ doneCount }} / {{ totalCount }}
        </el-tag>
        <div class="spacer" />
        <el-button v-if="finishedExists" text size="small" @click="clearFinished">
          清除已完成
        </el-button>
      </div>
    </template>

    <div v-if="items.length === 0" class="empty">暂无上传任务</div>
    <div v-for="it in items" :key="it.id" class="upload-item">
      <div class="upload-name" :title="displayName(it)">
        <el-tag v-if="it.status === 'instant'" size="small" type="success" class="badge">秒传</el-tag>
        <el-tag v-else-if="it.status === 'skipped'" size="small" type="info" class="badge">跳过</el-tag>
        <el-tag v-else-if="it.session" size="small" type="warning" class="badge">可续传</el-tag>
        {{ displayName(it) }}
      </div>
      <el-progress
        v-if="it.status !== 'skipped'"
        :percentage="it.pct"
        :status="it.status === 'error' ? 'exception' : isDone(it) ? 'success' : undefined"
      />
      <div class="upload-sub">
        <span v-if="it.status === 'hashing'" class="state">校验中(秒传检测)…</span>
        <span v-else-if="it.status === 'waiting'" class="state">排队中</span>
        <span v-else-if="it.status === 'uploading'" class="state">上传中，可随时暂停</span>
        <span v-else-if="it.status === 'paused'" class="state">已暂停，分块已保留</span>
        <span v-else-if="it.status === 'needs_file'" class="state">请选择原文件继续</span>
        <span v-else-if="it.status === 'cancelling'" class="state">正在清理已上传分块…</span>
        <span v-else-if="it.status === 'skipped'" class="state">已跳过(目录里有同名文件)</span>
        <span v-if="it.error" class="upload-err">{{ it.error }}</span>
        <div class="spacer" />
        <el-button v-if="it.status === 'uploading'" text size="small" @click="pause(it)">
          暂停
        </el-button>
        <el-button
          v-else-if="it.status === 'needs_file'"
          text
          size="small"
          type="primary"
          @click="chooseFile(it)"
        >
          选择文件
        </el-button>
        <el-button v-else-if="it.status === 'paused'" text size="small" type="primary" @click="resume(it)">
          继续
        </el-button>
        <el-button v-else-if="it.status === 'error'" text size="small" type="primary" @click="retry(it)">
          重试
        </el-button>
        <el-button
          v-if="!isDone(it) && it.status !== 'cancelling'"
          text
          size="small"
          type="danger"
          @click="cancelItem(it)"
        >
          取消任务
        </el-button>
      </div>
    </div>
  </el-drawer>

  <!-- 上传前发现同名文件时的三选一;X/ESC 关闭视为取消 -->
  <el-dialog
    v-model="conflictVisible"
    title="同名文件已存在"
    width="440px"
    append-to-body
    @close="onConflictClose"
  >
    <p class="conflict-hint">目标目录已有 {{ conflictNames.length }} 个同名文件,覆盖会替换原文件:</p>
    <ul class="conflict-list">
      <li v-for="n in conflictNames" :key="n" :title="n">{{ n }}</li>
    </ul>
    <template #footer>
      <el-button @click="resolveConflict('cancel')">取消上传</el-button>
      <el-button @click="resolveConflict('skip')">跳过这些</el-button>
      <el-button type="primary" @click="resolveConflict('overwrite')">全部覆盖</el-button>
    </template>
  </el-dialog>
</template>

<script lang="ts">
/** 一次入队的文件:rel 为相对子目录(文件夹上传时),普通上传为 '' */
export interface UploadFile {
  file: File
  rel?: string
}
</script>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { createSHA256 } from 'hash-wasm'
import { api, errMsg, type UploadSession } from '../api'

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
  /** undefined 表示尚未尝试生成，null 表示浏览器无法解码该视频。 */
  thumbnail?: Blob | null
}

const visible = ref(false)
const items = ref<Item[]>([])
const emit = defineEmits<{ uploaded: [] }>()

const totalCount = computed(() => items.value.length)
const doneCount = computed(() => items.value.filter(isDone).length)
const finishedExists = computed(() => items.value.some((item) => isDone(item)))
function isDone(item: Item) {
  return item.status === 'done' || item.status === 'instant' || item.status === 'skipped'
}
function displayName(item: Item) {
  if (item.session) return item.session.path.replace(/^\//, '')
  return item.rel ? `${item.rel}/${item.name}` : item.name
}

let seq = 0
let busy = false
let defaultConcurrency = 1
/** 只保存任务 ID，消费时从响应式数组取代理，确保状态变化能触发视图更新。 */
const queue: number[] = []
/** 本次页面会话已确保存在的目录，避免每个文件都重复调用 mkdir。 */
const ensuredDirs = new Set<string>()

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
    concurrency,
    controllers: new Set(),
    pauseRequested: true,
    cancelled: false,
  }
}

// ---------- 重名冲突询问 ----------

type ConflictAction = 'overwrite' | 'skip' | 'cancel'
const conflictVisible = ref(false)
const conflictNames = ref<string[]>([])
let conflictResolve: ((action: ConflictAction) => void) | null = null

function askConflict(names: string[]): Promise<ConflictAction> {
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

/** 入队前按目标目录批量查重，有同名文件时让用户选择覆盖、跳过或取消。 */
async function enqueue(dir: string, files: UploadFile[]) {
  const relOf = (upload: UploadFile) => (upload.rel ?? '').replace(/^\/+|\/+$/g, '')
  const groups = new Map<string, UploadFile[]>()
  for (const upload of files) {
    const rel = relOf(upload)
    const target = rel ? joinPath(dir, rel) : dir
    const group = groups.get(target)
    if (group) group.push(upload)
    else groups.set(target, [upload])
  }
  const duplicates = new Set<UploadFile>()
  for (const [target, group] of groups) {
    try {
      const response = await api.exists(target, group.map((upload) => upload.file.name))
      const existing = new Set(response.data.existing)
      for (const upload of group) if (existing.has(upload.file.name)) duplicates.add(upload)
    } catch {
      // 查重只是交互增强，短暂失败时仍允许后续上传接口处理。
    }
  }

  let action: ConflictAction = 'overwrite'
  if (duplicates.size > 0) {
    action = await askConflict(
      files
        .filter((upload) => duplicates.has(upload))
        .map((upload) =>
          relOf(upload) ? `${relOf(upload)}/${upload.file.name}` : upload.file.name,
        ),
    )
    if (action === 'cancel') return
  }

  for (const upload of files) {
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
    }
    if (action === 'skip' && duplicates.has(upload)) {
      item.status = 'skipped'
      item.pct = 100
      items.value.unshift(item)
      continue
    }
    items.value.unshift(item)
    schedule(item)
  }
  visible.value = true
}

function schedule(item: Item) {
  if (!queue.includes(item.id)) queue.push(item.id)
  void pump()
}

function pause(item: Item) {
  item.pauseRequested = true
  for (const controller of item.controllers) controller.abort()
  item.controllers.clear()
  if (!item.cancelled) item.status = 'paused'
}

function resume(item: Item) {
  if (!item.file) {
    chooseFile(item)
    return
  }
  item.pauseRequested = false
  item.error = undefined
  item.status = 'waiting'
  schedule(item)
}

function retry(item: Item) {
  if (item.session && item.session.uploaded_parts.length === item.session.part_count) {
    item.pauseRequested = false
    item.error = undefined
    item.status = 'waiting'
    schedule(item)
    return
  }
  if (!item.file) {
    chooseFile(item)
    return
  }
  resume(item)
}

async function cancelItem(item: Item) {
  item.cancelled = true
  pause(item)
  item.status = 'cancelling'
  const queueIndex = queue.indexOf(item.id)
  if (queueIndex >= 0) queue.splice(queueIndex, 1)
  try {
    if (item.session) await api.uploads.cancel(item.session.id)
    items.value = items.value.filter((candidate) => candidate.id !== item.id)
  } catch (error) {
    const status = (error as { response?: { status?: number } }).response?.status
    if (status === 404) {
      items.value = items.value.filter((candidate) => candidate.id !== item.id)
      return
    }
    item.cancelled = false
    item.status = 'error'
    item.error = `取消失败：${errMsg(error)}`
  }
}

function clearFinished() {
  items.value = items.value.filter((item) => !isDone(item))
}

/** 从顶栏打开上传队列；没有任务时也允许打开查看空状态。 */
function open() {
  visible.value = true
}

/** 流式计算 SHA-256（32MB 一片），大文件不会一次读入内存。 */
async function sha256File(
  file: File,
  onPct: (percentage: number) => void,
  stopped: () => boolean = () => false,
): Promise<string> {
  const hasher = await createSHA256()
  hasher.init()
  const chunkSize = 32 * 1024 * 1024
  for (let offset = 0; offset < file.size; offset += chunkSize) {
    if (stopped()) throw new Error('任务已取消')
    const buffer = await file.slice(offset, Math.min(offset + chunkSize, file.size)).arrayBuffer()
    hasher.update(new Uint8Array(buffer))
    onPct(Math.round((Math.min(offset + chunkSize, file.size) / file.size) * 100))
  }
  onPct(100)
  return hasher.digest('hex')
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
  if (!isVideoFile(file)) return Promise.resolve(null)
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
      video.pause()
      video.removeAttribute('src')
      video.load()
      URL.revokeObjectURL(url)
      resolve(image)
    }

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
    await api.mkdir(target)
    ensuredDirs.add(target)
  }
  return target
}

/** 完成响应丢失后核实文件是否已经原子落位，避免重复上传。 */
async function confirmCompleted(
  target: string,
  item: Item,
  sha: string,
): Promise<boolean | null> {
  const deadline = Date.now() + 5 * 60 * 1000
  while (Date.now() < deadline) {
    let state: { linked: boolean; pending: boolean }
    try {
      state = (await api.instant(target, item.name, sha, item.size)).data
    } catch {
      // 核实请求也可能遇到同一段短暂网络故障，稍后再问，不能据此判定上传失败。
      await new Promise((resolve) => window.setTimeout(resolve, 1000))
      continue
    }
    if (state.linked) return true
    if (!state.pending) return false
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
  const response = await api.uploads.status(item.session.id)
  item.session = response.data.session
  item.concurrency = response.data.concurrency
  item.pct = sessionPct(item.session)
}

async function finalizeItem(item: Item, session: UploadSession) {
  try {
    await api.uploads.complete(session.id)
  } catch (completeError) {
    const confirmed = await confirmCompleted(item.dir, item, item.sha256 ?? session.sha256)
    if (confirmed !== true) throw completeError
  }
  item.status = 'done'
  item.pct = 100
  item.session = undefined
  emit('uploaded')
}

async function uploadMissingParts(item: Item) {
  if (!item.session || !item.file) return
  await syncSession(item)
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

  const updateProgress = () => {
    if (!item.session || item.session.size === 0) return
    const active = [...activeBytes.values()].reduce((sum, value) => sum + value, 0)
    item.pct = Math.min(99, Math.round(((completedBytes + active) / item.session.size) * 100))
  }

  item.status = 'uploading'
  const worker = async () => {
    while (!stopped && !item.pauseRequested && !item.cancelled) {
      const index = missing[cursor++]
      if (index === undefined) return
      const start = index * session.chunk_size
      const data = item.file!.slice(start, start + partSize(session, index))
      const controller = new AbortController()
      item.controllers.add(controller)
      activeBytes.set(index, 0)
      try {
        await api.uploads.part(
          session.id,
          index,
          data,
          (loaded) => {
            activeBytes.set(index, Math.min(loaded, data.size))
            updateProgress()
          },
          controller.signal,
        )
        completedBytes += data.size
        activeBytes.delete(index)
        updateProgress()
      } catch (error) {
        activeBytes.delete(index)
        if (!item.pauseRequested && !item.cancelled) {
          failure = error
          stopped = true
          for (const active of item.controllers) active.abort()
        }
      } finally {
        item.controllers.delete(controller)
      }
    }
  }

  const workerCount = Math.max(1, Math.min(item.concurrency, missing.length || 1))
  await Promise.all(Array.from({ length: workerCount }, worker))
  if (item.cancelled) return
  try {
    await syncSession(item)
  } catch {
    // 会话可能恰好在完成请求中消失，后面的秒传核实会给出最终状态。
  }
  if (item.pauseRequested) {
    item.status = 'paused'
    return
  }
  if (failure) throw failure

  await finalizeItem(item, session)
}

async function processItem(item: Item) {
  if (item.cancelled) return
  if (
    item.session &&
    item.session.uploaded_parts.length === item.session.part_count
  ) {
    await finalizeItem(item, item.session)
    return
  }
  if (!item.file) {
    item.status = 'needs_file'
    return
  }
  const target = item.session ? item.dir : await ensureDir(item)

  if (!item.session) {
    item.status = 'hashing'
    const thumbnailPromise =
      item.thumbnail === undefined ? createVideoThumbnail(item.file) : Promise.resolve(item.thumbnail)
    item.sha256 = await sha256File(item.file, (percentage) => (item.pct = percentage), () => item.cancelled)
    item.thumbnail = await thumbnailPromise
    if (item.cancelled) return

    try {
      const instant = await api.instant(target, item.name, item.sha256, item.size)
      if (item.cancelled) return
      if (instant.data.linked) {
        item.status = 'instant'
        item.pct = 100
        emit('uploaded')
        return
      }
    } catch {
      // 秒传探测失败时继续创建普通上传会话。
    }
    if (item.cancelled) return

    const created = await api.uploads.create(target, item.name, item.size, item.sha256)
    item.session = created.data.session
    item.concurrency = created.data.concurrency
    item.dir = parentOf(item.session.path)
    item.pct = sessionPct(item.session)
    if (item.cancelled) {
      await api.uploads.cancel(item.session.id).catch(() => {})
      return
    }
  }
  if (item.thumbnail && item.session && !item.session.uploaded_parts.includes(0)) {
    await api.uploads.thumbnail(item.session.id, item.thumbnail).catch(() => {
      // 缩略图失败不应阻塞文件本体上传。
    })
  }
  await uploadMissingParts(item)
}

async function pump() {
  if (busy) return
  busy = true
  while (queue.length > 0) {
    const itemId = queue.shift()!
    const item = items.value.find((candidate) => candidate.id === itemId)
    if (!item || item.cancelled) continue
    try {
      await processItem(item)
    } catch (error) {
      if (!item.cancelled && !item.pauseRequested) {
        item.status = 'error'
        item.error = errMsg(error)
      }
    }
  }
  busy = false
}

function chooseFile(item: Item) {
  const input = document.createElement('input')
  input.type = 'file'
  input.onchange = () => {
    const file = input.files?.[0]
    if (file) void attachFile(item, file)
  }
  input.click()
}

async function attachFile(item: Item, file: File) {
  if (!item.session) return
  if (file.size !== item.session.size) {
    item.status = 'needs_file'
    item.error = `文件大小不一致，应为 ${item.session.size} 字节`
    return
  }
  item.file = file
  item.status = 'hashing'
  item.error = undefined
  item.cancelled = false
  try {
    const digest = await sha256File(file, (percentage) => (item.pct = percentage))
    if (digest !== item.session.sha256) {
      item.file = undefined
      item.status = 'needs_file'
      item.pct = sessionPct(item.session)
      item.error = '所选文件内容与原任务不一致，请选择原文件'
      return
    }
    item.sha256 = digest
    item.thumbnail = await createVideoThumbnail(file)
    resume(item)
  } catch (error) {
    item.file = undefined
    item.status = 'needs_file'
    item.error = errMsg(error)
  }
}

onMounted(async () => {
  try {
    const response = await api.uploads.list()
    defaultConcurrency = response.data.concurrency
    if (response.data.sessions.length) {
      for (const session of response.data.sessions) {
        const item = fromSession(session, response.data.concurrency)
        if (session.uploaded_parts.length === session.part_count) {
          item.status = 'waiting'
          item.error = undefined
          item.pauseRequested = false
        }
        items.value.push(item)
        if (item.status === 'waiting') schedule(item)
      }
      visible.value = true
    }
  } catch {
    // 文件页仍可使用；用户开始上传时会再次得到明确错误。
  }
})

onBeforeUnmount(() => {
  for (const item of items.value) {
    item.pauseRequested = true
    for (const controller of item.controllers) controller.abort()
  }
})

defineExpose({ enqueue, open })
</script>

<style scoped>
.head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
}
.spacer {
  flex: 1;
}
.empty {
  color: var(--text-soft);
  text-align: center;
  margin-top: 24px;
}
.upload-item {
  margin-bottom: 14px;
}
.upload-name {
  font-size: 13px;
  margin-bottom: 4px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  display: flex;
  align-items: center;
  gap: 6px;
}
.badge {
  flex-shrink: 0;
}
.upload-sub {
  display: flex;
  align-items: center;
  gap: 8px;
  min-height: 22px;
}
.state {
  color: var(--text-soft);
  font-size: 12px;
}
.upload-err {
  color: #f56c6c;
  font-size: 12px;
}
.conflict-hint {
  margin: 0 0 8px;
  color: var(--text-soft);
  font-size: 13px;
}
.conflict-list {
  max-height: 200px;
  overflow: auto;
  margin: 0;
  padding-left: 20px;
  font-size: 13px;
}
.conflict-list li {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
