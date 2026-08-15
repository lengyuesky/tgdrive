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
        <span v-else-if="it.status === 'uploading'" class="state">上传中</span>
        <span v-else-if="it.status === 'skipped'" class="state">已跳过(目录里有同名文件)</span>
        <span v-if="it.error" class="upload-err">{{ it.error }}</span>
        <div class="spacer" />
        <el-button v-if="it.status === 'error'" text size="small" type="primary" @click="retry(it)">
          重试
        </el-button>
        <el-button v-if="it.status === 'waiting'" text size="small" @click="removeWaiting(it)">
          移除
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
import { computed, ref } from 'vue'
import { createSHA256 } from 'hash-wasm'
import { api, errMsg } from '../api'

interface Item {
  id: number
  dir: string
  rel: string
  file: File
  pct: number
  status: 'hashing' | 'waiting' | 'uploading' | 'instant' | 'done' | 'skipped' | 'error'
  error?: string
  /** undefined 表示尚未尝试生成，null 表示浏览器无法解码该视频。 */
  thumbnail?: Blob | null
}

const visible = ref(false)
const items = ref<Item[]>([])
const emit = defineEmits<{ uploaded: [] }>()

const totalCount = computed(() => items.value.length)
const doneCount = computed(() => items.value.filter(isDone).length)
const finishedExists = computed(() => items.value.some((i) => isDone(i) || i.status === 'error'))
function isDone(it: Item) {
  return it.status === 'done' || it.status === 'instant' || it.status === 'skipped'
}
function displayName(it: Item) {
  return it.rel ? `${it.rel}/${it.file.name}` : it.file.name
}

let seq = 0
let busy = false
const queue: Item[] = []
/** 本次会话已确保存在的目录,避免每个文件都调一次 mkdir */
const ensuredDirs = new Set<string>()

function joinPath(dir: string, name: string) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`
}

// ---------- 重名冲突询问 ----------

type ConflictAction = 'overwrite' | 'skip' | 'cancel'
const conflictVisible = ref(false)
const conflictNames = ref<string[]>([])
let conflictResolve: ((a: ConflictAction) => void) | null = null

function askConflict(names: string[]): Promise<ConflictAction> {
  conflictNames.value = names
  conflictVisible.value = true
  return new Promise((res) => (conflictResolve = res))
}

function resolveConflict(a: ConflictAction) {
  const r = conflictResolve
  conflictResolve = null
  conflictVisible.value = false
  r?.(a)
}

/** 点 X / 按 ESC 关闭对话框视为取消上传 */
function onConflictClose() {
  if (conflictResolve) resolveConflict('cancel')
}

/** 入队一批文件并打开抽屉,逐个串行处理(后端再按分块并发)。
 *  入队前按目标目录批量查重,有同名文件时让用户选覆盖/跳过/取消 */
async function enqueue(dir: string, files: UploadFile[]) {
  const relOf = (f: UploadFile) => (f.rel ?? '').replace(/^\/+|\/+$/g, '')
  // 按目标目录分组查一次重;查重失败不阻塞上传(与秒传探测同样宽容)
  const groups = new Map<string, UploadFile[]>()
  for (const f of files) {
    const rel = relOf(f)
    const target = rel ? joinPath(dir, rel) : dir
    const g = groups.get(target)
    if (g) g.push(f)
    else groups.set(target, [f])
  }
  const dupes = new Set<UploadFile>()
  for (const [target, fs] of groups) {
    try {
      const r = await api.exists(target, fs.map((f) => f.file.name))
      const hit = new Set(r.data.existing)
      for (const f of fs) if (hit.has(f.file.name)) dupes.add(f)
    } catch {
      // 查重接口不可用就当没有重名,行为退回旧版(直接覆盖)
    }
  }

  let action: ConflictAction = 'overwrite'
  if (dupes.size > 0) {
    const names = files
      .filter((f) => dupes.has(f))
      .map((f) => (relOf(f) ? `${relOf(f)}/${f.file.name}` : f.file.name))
    action = await askConflict(names)
    if (action === 'cancel') return
  }

  for (const f of files) {
    const item: Item = {
      id: ++seq,
      dir,
      rel: relOf(f),
      file: f.file,
      pct: 0,
      status: 'waiting',
    }
    if (action === 'skip' && dupes.has(f)) {
      item.status = 'skipped'
      item.pct = 100
      items.value.unshift(item)
      continue // 不进处理队列,不发任何请求
    }
    items.value.unshift(item)
    queue.push(item)
  }
  visible.value = true
  void pump()
}

function retry(it: Item) {
  const live = items.value.find((i) => i.id === it.id)
  if (!live || live.status !== 'error') return
  live.status = 'waiting'
  live.pct = 0
  live.error = undefined
  queue.push(live)
  void pump()
}

function removeWaiting(it: Item) {
  const qi = queue.findIndex((q) => q.id === it.id)
  if (qi >= 0) queue.splice(qi, 1)
  items.value = items.value.filter((i) => i.id !== it.id)
}

function clearFinished() {
  items.value = items.value.filter((i) => !isDone(i) && i.status !== 'error')
}

/** 流式 sha256(32MB 一片),大文件不占内存 */
async function sha256File(f: File, onPct: (p: number) => void): Promise<string> {
  const hasher = await createSHA256()
  hasher.init()
  const CHUNK = 32 * 1024 * 1024
  for (let off = 0; off < f.size; off += CHUNK) {
    const buf = await f.slice(off, Math.min(off + CHUNK, f.size)).arrayBuffer()
    hasher.update(new Uint8Array(buf))
    onPct(Math.round((Math.min(off + CHUNK, f.size) / f.size) * 100))
  }
  return hasher.digest('hex')
}

const THUMB_VIDEO_EXT = new Set(['mp4', 'webm', 'mov', 'm4v', 'mkv', 'ogv'])

function isVideoFile(file: File) {
  const ext = file.name.split('.').pop()?.toLowerCase() ?? ''
  return file.type.startsWith('video/') || THUMB_VIDEO_EXT.has(ext)
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
      const scale = Math.min(1, 480 / width, 480 / height)
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
      canvas.toBlob(
        (webp) => {
          if (webp?.type === 'image/webp') return finish(webp)
          canvas.toBlob((jpeg) => finish(jpeg), 'image/jpeg', 0.82)
        },
        'image/webp',
        0.78,
      )
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

/** 缩略图失败不影响文件本身上传，历史或不支持的编码会继续显示视频图标。 */
async function persistThumbnail(target: string, item: Item, sha: string) {
  if (!item.thumbnail) return
  try {
    await api.saveThumbnail(joinPath(target, item.file.name), sha, item.thumbnail)
  } catch (e) {
    console.warn('视频缩略图保存失败', e)
  }
}

/** 目标目录 = 基目录 + 相对子路径;文件夹上传时按需递归创建 */
async function ensureDir(item: Item): Promise<string> {
  const target = item.rel ? joinPath(item.dir, item.rel) : item.dir
  if (item.rel && !ensuredDirs.has(target)) {
    await api.mkdir(target) // 后端 mkdir 是递归语义(mkdir -p)
    ensuredDirs.add(target)
  }
  return target
}

/** 上传响应丢失后等待后台任务落定；仍有同名 uploading 节点时不能立即重传。 */
async function confirmCompleted(
  target: string,
  item: Item,
  sha: string,
): Promise<boolean | null> {
  const deadline = Date.now() + 5 * 60 * 1000
  while (Date.now() < deadline) {
    let state: { linked: boolean; pending: boolean }
    try {
      state = (await api.instant(target, item.file.name, sha, item.file.size)).data
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

async function pump() {
  if (busy) return
  busy = true
  while (queue.length > 0) {
    const item = queue.shift()!
    const live = items.value.find((i) => i.id === item.id)
    if (!live) continue
    try {
      const target = await ensureDir(live)

      // 秒传:先算哈希问一嘴,库里有同内容文件就零流量落位
      live.status = 'hashing'
      if (live.thumbnail === undefined) {
        live.thumbnail = await createVideoThumbnail(live.file)
      }
      let linked = false
      let pending = false
      let sha: string | undefined
      try {
        sha = await sha256File(live.file, (p) => (live.pct = p))
        const r = await api.instant(target, live.file.name, sha, live.file.size)
        linked = r.data.linked
        pending = r.data.pending
      } catch {
        // 哈希/探测失败不阻塞,老老实实走真实上传
      }
      if (!linked && pending && sha) {
        // 同一路径已有上传时先等它落定，避免多设备或手动重试并发写出重复分块。
        const confirmed = await confirmCompleted(target, live, sha)
        if (confirmed === null) throw new Error('同名文件仍在后台上传，请稍后重试')
        linked = confirmed
      }
      if (linked) {
        if (sha) await persistThumbnail(target, live, sha)
        live.status = 'instant'
        live.pct = 100
        emit('uploaded')
        continue
      }

      live.status = 'uploading'
      live.pct = 0
      let finalSha = sha
      try {
        const r = await api.upload(target, live.file, (pct) => (live.pct = pct))
        finalSha = r.data.saved[0]?.sha256 ?? finalSha
      } catch (uploadError) {
        // 响应丢失不代表服务端失败：先按已算出的哈希核实，避免用户重试后重复上传。
        if (!sha) throw uploadError
        const confirmed = await confirmCompleted(target, live, sha)
        // 核实接口自身失败时保留原始上传错误，它通常更能说明根因。
        if (confirmed !== true) throw uploadError
      }
      if (finalSha) await persistThumbnail(target, live, finalSha)
      live.status = 'done'
      live.pct = 100
      emit('uploaded')
    } catch (e) {
      live.status = 'error'
      live.error = errMsg(e)
    }
  }
  busy = false
}

defineExpose({ enqueue })
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
