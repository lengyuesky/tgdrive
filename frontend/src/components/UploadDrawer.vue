<template>
  <el-drawer v-model="visible" size="min(460px, 94vw)" :close-on-click-modal="true">
    <template #header>
      <div class="head">
        <b>上传队列</b>
        <div class="spacer" />
        <el-button
          v-if="finishedExists"
          text
          size="small"
          :disabled="batchBusy"
          @click="clearFinished"
        >
          清除已完成
        </el-button>
      </div>
    </template>

    <div v-if="totalCount" class="queue-summary" aria-label="上传任务统计">
      <div class="summary-item">
        <strong class="summary-value">{{ totalCount }}</strong>
        <span class="summary-label">总任务</span>
      </div>
      <div class="summary-item summary-success">
        <strong class="summary-value">{{ successCount }}</strong>
        <span class="summary-label">成功</span>
      </div>
      <div class="summary-item summary-skipped">
        <strong class="summary-value">{{ skippedCount }}</strong>
        <span class="summary-label">跳过</span>
      </div>
      <div class="summary-item summary-error">
        <strong class="summary-value">{{ errorCount }}</strong>
        <span class="summary-label">失败</span>
      </div>
    </div>

    <div v-if="totalCount" class="queue-breakdown">
      <el-tag v-if="uploadedCount" size="small" type="success" effect="plain" round>
        普通上传 {{ uploadedCount }}
      </el-tag>
      <el-tag v-if="instantCount" size="small" type="success" effect="plain" round>
        秒传 {{ instantCount }}
      </el-tag>
      <el-tag v-if="activeCount" size="small" type="warning" effect="plain" round>
        处理中 {{ activeCount }}
      </el-tag>
      <el-tag v-if="pausedCount" size="small" type="info" effect="plain" round>
        已暂停 {{ pausedCount }}
      </el-tag>
      <el-tag v-if="needsFileCount" size="small" type="warning" effect="plain" round>
        待选原文件 {{ needsFileCount }}
      </el-tag>
      <el-tag v-if="cancellingCount" size="small" type="info" effect="plain" round>
        取消中 {{ cancellingCount }}
      </el-tag>
    </div>

    <div v-if="pendingCount" class="queue-actions">
      <div v-if="needsFileCount" class="queue-status">
        <span class="queue-hint">需逐项选择原文件后才能继续</span>
      </div>
      <div class="queue-buttons">
        <el-button
          v-if="pauseableCount"
          size="small"
          plain
          :icon="VideoPause"
          :disabled="batchBusy"
          @click="pauseAll"
        >
          全部暂停
        </el-button>
        <el-button
          v-if="resumableCount"
          size="small"
          type="primary"
          plain
          :icon="VideoPlay"
          :disabled="batchBusy"
          @click="resumeAll"
        >
          全部继续
        </el-button>
        <el-button
          v-if="retryableCount"
          size="small"
          plain
          :icon="Refresh"
          :disabled="batchBusy"
          @click="retryAll"
        >
          重试失败
        </el-button>
        <el-button
          size="small"
          type="danger"
          plain
          :icon="Close"
          :loading="batchBusy"
          :disabled="batchBusy"
          @click="cancelAll"
        >
          取消全部
        </el-button>
      </div>
    </div>

    <div v-if="items.length === 0" class="empty">暂无上传任务</div>
    <div v-for="it in items" :key="it.id" class="upload-item" :class="`tone-${tone(it)}`">
      <div class="item-top">
        <el-tag v-if="it.status === 'instant'" size="small" type="success" class="badge">秒传</el-tag>
        <el-tag v-else-if="it.status === 'skipped'" size="small" type="info" class="badge">跳过</el-tag>
        <el-tag v-else-if="it.session" size="small" type="warning" class="badge">可续传</el-tag>
        <span class="item-name" :title="displayName(it)">{{ displayName(it) }}</span>
        <span v-if="!isDone(it) && it.status !== 'error'" class="item-pct">{{ it.pct }}%</span>
      </div>

      <el-progress
        v-if="it.status !== 'skipped'"
        class="item-bar"
        :percentage="it.pct"
        :stroke-width="6"
        :show-text="false"
        :status="it.status === 'error' ? 'exception' : isDone(it) ? 'success' : undefined"
      />

      <div class="item-foot">
        <span class="state">
          <span v-if="it.status === 'hashing'">校验中(秒传检测),可暂停…</span>
          <span v-else-if="it.status === 'waiting'">排队中</span>
          <span v-else-if="it.status === 'uploading'">上传中,可随时暂停</span>
          <span v-else-if="it.status === 'paused'">已暂停,可继续上传</span>
          <span v-else-if="it.status === 'needs_file'">请选择原文件继续</span>
          <span v-else-if="it.status === 'cancelling'">正在清理已上传分块…</span>
          <span v-else-if="it.status === 'done'">上传成功</span>
          <span v-else-if="it.status === 'instant'">秒传成功</span>
          <span v-else-if="it.status === 'skipped'">已跳过(目录里有同名文件)</span>
          <span v-else-if="it.status === 'error'">上传失败</span>
        </span>
        <span v-if="it.size" class="size">{{ fmtSize(it.size) }}</span>
        <div class="spacer" />
        <div class="item-btns">
          <el-button
            v-if="isPauseable(it)"
            text
            size="small"
            :disabled="batchBusy"
            @click="pause(it)"
          >
            暂停
          </el-button>
          <el-button
            v-else-if="it.status === 'needs_file'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="chooseFile(it)"
          >
            选择文件
          </el-button>
          <el-button
            v-else-if="it.status === 'paused'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="resume(it)"
          >
            继续
          </el-button>
          <el-button
            v-else-if="it.status === 'error'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="retry(it)"
          >
            重试
          </el-button>
          <el-button
            v-if="!isDone(it) && it.status !== 'cancelling'"
            text
            size="small"
            type="danger"
            :disabled="batchBusy"
            @click="cancelItem(it)"
          >
            取消
          </el-button>
        </div>
      </div>

      <div v-if="it.error" class="upload-err" :title="it.error">{{ it.error }}</div>
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
import { ElMessage } from 'element-plus'
import { createSHA256 } from 'hash-wasm'
import { Close, Refresh, VideoPause, VideoPlay } from '@element-plus/icons-vue'
import { api, errMsg, fmtSize, type UploadSession } from '../api'
import { appConfirm } from '../dialog'

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
  return !item.cancelled && item.generation === generation
}

const visible = ref(false)
const items = ref<Item[]>([])
const batchBusy = ref(false)
const emit = defineEmits<{ uploaded: [] }>()

const totalCount = computed(() => items.value.length)
const uploadedCount = computed(() => items.value.filter((item) => item.status === 'done').length)
const instantCount = computed(() => items.value.filter((item) => item.status === 'instant').length)
const successCount = computed(() => uploadedCount.value + instantCount.value)
const skippedCount = computed(() => items.value.filter((item) => item.status === 'skipped').length)
const finishedExists = computed(() => items.value.some((item) => isDone(item)))

const pendingCount = computed(() => items.value.filter(isPending).length)
const activeCount = computed(() => items.value.filter(isPauseable).length)
const pauseableCount = computed(() => items.value.filter(isPauseable).length)
const pausedCount = computed(() => items.value.filter((item) => item.status === 'paused').length)
const errorCount = computed(() => items.value.filter((item) => item.status === 'error').length)
const needsFileCount = computed(
  () => items.value.filter((item) => item.status === 'needs_file').length,
)
const cancellingCount = computed(
  () => items.value.filter((item) => item.status === 'cancelling').length,
)
const resumableCount = computed(
  () => items.value.filter((item) => item.status === 'paused' && Boolean(item.file)).length,
)
const retryableCount = computed(
  () => items.value.filter((item) => item.status === 'error' && Boolean(item.file)).length,
)

function isDone(item: Item) {
  return item.status === 'done' || item.status === 'instant' || item.status === 'skipped'
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
    generation: 0,
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
      generation: 0,
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

function removeQueued(id: number) {
  for (let index = queue.length - 1; index >= 0; index -= 1) {
    if (queue[index] === id) queue.splice(index, 1)
  }
}

/** 停止当前任务的本地工作；服务端会话保留，供稍后断点续传。 */
function stopItem(item: Item) {
  item.generation += 1
  item.pauseRequested = true
  removeQueued(item.id)
  for (const controller of item.controllers) controller.abort()
  item.controllers.clear()
}

function pause(item: Item) {
  if (!isPauseable(item)) return
  stopItem(item)
  if (!item.cancelled) item.status = 'paused'
}

function resume(item: Item) {
  if (item.cancelled || isDone(item) || item.status === 'cancelling') return
  if (!item.file) {
    chooseFile(item)
    return
  }
  item.generation += 1
  item.pauseRequested = false
  item.error = undefined
  item.status = 'waiting'
  schedule(item)
}

function retry(item: Item) {
  if (item.cancelled || isDone(item) || item.status === 'cancelling') return
  if (item.session && item.session.uploaded_parts.length === item.session.part_count) {
    item.generation += 1
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

async function cancelItem(item: Item): Promise<boolean> {
  if (isDone(item) || item.status === 'cancelling') return true
  item.cancelled = true
  stopItem(item)
  item.status = 'cancelling'
  try {
    if (item.session) await api.uploads.cancel(item.session.id)
    items.value = items.value.filter((candidate) => candidate.id !== item.id)
    return true
  } catch (error) {
    const status = (error as { response?: { status?: number } }).response?.status
    if (status === 404) {
      items.value = items.value.filter((candidate) => candidate.id !== item.id)
      return true
    }
    item.cancelled = false
    item.status = 'error'
    item.error = `取消失败：${errMsg(error)}`
    return false
  }
}

async function cancelAll() {
  if (batchBusy.value) return
  const targets = items.value.filter(isPending)
  if (!targets.length) return
  try {
    await appConfirm({
      title: '取消全部上传',
      message: `确定取消 ${targets.length} 个未完成上传吗？已上传的 Telegram 分块会进入清理队列，任务将从队列移除。`,
      kind: 'danger',
      confirmText: '全部取消',
      cancelText: '保留任务',
    })
  } catch {
    return
  }

  batchBusy.value = true
  let succeeded = 0
  let failed = 0
  try {
    // 逐个请求，避免同时删除大量分块时给 SQLite/Telegram 清理队列造成突发压力。
    for (const item of targets) {
      if (await cancelItem(item)) succeeded += 1
      else failed += 1
    }
  } finally {
    batchBusy.value = false
  }
  if (failed) {
    ElMessage.warning(`已取消 ${succeeded} 个任务，另有 ${failed} 个取消失败，请单独重试`)
  } else if (succeeded) {
    ElMessage.success(`已取消 ${succeeded} 个上传任务`)
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
    if (stopped()) throw new Error('任务已停止')
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
  stopped: () => boolean = () => false,
): Promise<boolean | null> {
  const deadline = Date.now() + 5 * 60 * 1000
  while (Date.now() < deadline) {
    if (stopped()) return null
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
  const response = await api.uploads.status(item.session.id)
  item.session = response.data.session
  item.concurrency = response.data.concurrency
  item.pct = sessionPct(item.session)
}

async function finalizeItem(item: Item, session: UploadSession, generation: number) {
  if (!isCurrent(item, generation) || item.pauseRequested) return
  try {
    await api.uploads.complete(session.id)
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
  item.status = 'done'
  item.pct = 100
  item.session = undefined
  emit('uploaded')
}

async function uploadMissingParts(item: Item, generation: number) {
  if (!item.session || !item.file) return
  await syncSession(item)
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
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

  const updateProgress = () => {
    if (!item.session || item.session.size === 0) return
    const active = [...activeBytes.values()].reduce((sum, value) => sum + value, 0)
    item.pct = Math.min(99, Math.round(((completedBytes + active) / item.session.size) * 100))
  }

  item.status = 'uploading'
  const worker = async () => {
    while (!stopped && isCurrent(item, generation) && !item.pauseRequested) {
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
            if (!isCurrent(item, generation)) return
            activeBytes.set(index, Math.min(loaded, data.size))
            updateProgress()
          },
          controller.signal,
        )
        if (!isCurrent(item, generation)) return
        completedBytes += data.size
        activeBytes.delete(index)
        updateProgress()
      } catch (error) {
        activeBytes.delete(index)
        if (isCurrent(item, generation) && !item.pauseRequested) {
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
  if (!isCurrent(item, generation)) return
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

  await finalizeItem(item, session, generation)
}

async function processItem(item: Item) {
  const generation = item.generation
  if (!isCurrent(item, generation)) return
  if (item.pauseRequested) {
    item.status = 'paused'
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
    item.status = 'needs_file'
    return
  }
  const target = item.session ? item.dir : await ensureDir(item)
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
    return
  }

  if (!item.session) {
    item.status = 'hashing'
    const thumbnailPromise =
      item.thumbnail === undefined ? createVideoThumbnail(item.file) : Promise.resolve(item.thumbnail)
    item.sha256 = await sha256File(
      item.file,
      (percentage) => (item.pct = percentage),
      () => !isCurrent(item, generation) || item.pauseRequested,
    )
    item.thumbnail = await thumbnailPromise
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
      return
    }

    try {
      const instant = await api.instant(target, item.name, item.sha256, item.size)
      if (!isCurrent(item, generation) || item.pauseRequested) {
        if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
        return
      }
      if (instant.data.linked) {
        item.status = 'instant'
        item.pct = 100
        emit('uploaded')
        return
      }
    } catch {
      // 秒传探测失败时继续创建普通上传会话。
    }
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
      return
    }

    const created = await api.uploads.create(target, item.name, item.size, item.sha256)
    item.session = created.data.session
    item.concurrency = created.data.concurrency
    item.dir = parentOf(item.session.path)
    item.pct = sessionPct(item.session)
    if (!isCurrent(item, generation) || item.pauseRequested) {
      if (item.cancelled) await api.uploads.cancel(item.session.id).catch(() => {})
      else if (isCurrent(item, generation)) item.status = 'paused'
      return
    }
  }
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
    return
  }
  if (item.thumbnail && item.session && !item.session.uploaded_parts.includes(0)) {
    await api.uploads.thumbnail(item.session.id, item.thumbnail).catch(() => {
      // 缩略图失败不应阻塞文件本体上传。
    })
  }
  if (!isCurrent(item, generation) || item.pauseRequested) {
    if (isCurrent(item, generation) && item.pauseRequested) item.status = 'paused'
    return
  }
  await uploadMissingParts(item, generation)
}

async function pump() {
  if (busy) return
  busy = true
  while (queue.length > 0) {
    const itemId = queue.shift()!
    const item = items.value.find((candidate) => candidate.id === itemId)
    if (!item || item.cancelled) continue
    const generation = item.generation
    try {
      await processItem(item)
    } catch (error) {
      if (isCurrent(item, generation) && !item.pauseRequested) {
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
  item.generation += 1
  const generation = item.generation
  item.pauseRequested = false
  item.cancelled = false
  try {
    const digest = await sha256File(
      file,
      (percentage) => (item.pct = percentage),
      () => !isCurrent(item, generation) || item.pauseRequested,
    )
    if (!isCurrent(item, generation)) {
      if (item.cancelled) return
      if (item.pauseRequested) {
        item.file = undefined
        item.status = 'needs_file'
        item.error = '校验已暂停，请重新选择原文件继续'
      }
      return
    }
    if (item.pauseRequested) {
      // 旧会话自带 sha256；校验尚未完成时不能把未验证的文件交给续传流程。
      item.file = undefined
      item.status = 'needs_file'
      item.error = '校验已暂停，请重新选择原文件继续'
      return
    }
    if (digest !== item.session.sha256) {
      item.file = undefined
      item.status = 'needs_file'
      item.pct = sessionPct(item.session)
      item.error = '所选文件内容与原任务不一致，请选择原文件'
      return
    }
    item.sha256 = digest
    item.thumbnail = await createVideoThumbnail(file)
    if (!isCurrent(item, generation)) return
    if (item.pauseRequested) {
      item.status = 'paused'
      return
    }
    resume(item)
  } catch (error) {
    if (item.cancelled) return
    if (item.pauseRequested || item.generation !== generation) {
      item.file = undefined
      item.status = 'needs_file'
      item.error = '校验已暂停，请重新选择原文件继续'
      return
    }
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
.head b {
  font-size: 15px;
}
:deep(.el-drawer__header) {
  margin-bottom: 14px;
}
.spacer {
  flex: 1;
}
.queue-summary {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 10px;
  margin-bottom: 12px;
}
.summary-item {
  display: flex;
  min-width: 0;
  flex-direction: column;
  align-items: center;
  padding: 12px 4px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.summary-value {
  font-size: 22px;
  line-height: 1.15;
}
.summary-label {
  margin-top: 5px;
  color: var(--text-soft);
  font-size: 12px;
  white-space: nowrap;
}
.summary-success .summary-value {
  color: var(--el-color-success);
}
.summary-skipped .summary-value {
  color: var(--el-color-info);
}
.summary-error .summary-value {
  color: var(--el-color-danger);
}
.queue-breakdown {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 14px;
}
.queue-actions {
  margin-bottom: 18px;
  padding: 12px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.queue-status,
.queue-buttons {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.queue-status + .queue-buttons {
  margin-top: 10px;
}
.queue-buttons .el-button {
  margin-left: 0;
}
.queue-hint {
  color: var(--text-soft);
  font-size: 12px;
}
.empty {
  color: var(--text-soft);
  text-align: center;
  margin-top: 32px;
}

/* ---- 任务卡片 ---- */
.upload-item {
  position: relative;
  margin-bottom: 12px;
  padding: 12px 14px 12px 16px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
  overflow: hidden;
  transition: background 0.15s, border-color 0.15s;
}
.upload-item:hover {
  background: var(--tile-bg-hover);
}
/* 左侧状态色条 */
.upload-item::before {
  content: '';
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  width: 3px;
  background: var(--el-color-info);
}
.upload-item.tone-ok::before {
  background: var(--el-color-success);
}
.upload-item.tone-err::before {
  background: var(--el-color-danger);
}
.upload-item.tone-run::before {
  background: var(--el-color-primary);
}
.item-top {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.item-name {
  flex: 1;
  min-width: 0;
  font-size: 14px;
  font-weight: 600;
  line-height: 1.5;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.item-pct {
  flex-shrink: 0;
  font-size: 13px;
  font-variant-numeric: tabular-nums;
  color: var(--text-soft);
}
.badge {
  flex-shrink: 0;
}
.item-bar {
  margin: 10px 0 8px;
}
.item-foot {
  display: flex;
  align-items: center;
  gap: 10px;
  min-height: 26px;
}
.state {
  color: var(--text-soft);
  font-size: 12px;
}
.size {
  flex-shrink: 0;
  color: var(--text-soft);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}
.item-btns {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}
.item-btns .el-button {
  margin-left: 0;
}
.upload-err {
  margin-top: 8px;
  padding: 6px 8px;
  border-radius: 8px;
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);
  font-size: 12px;
  line-height: 1.5;
  word-break: break-all;
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
