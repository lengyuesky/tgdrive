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

/** 目标目录 = 基目录 + 相对子路径;文件夹上传时按需递归创建 */
async function ensureDir(item: Item): Promise<string> {
  const target = item.rel ? joinPath(item.dir, item.rel) : item.dir
  if (item.rel && !ensuredDirs.has(target)) {
    await api.mkdir(target) // 后端 mkdir 是递归语义(mkdir -p)
    ensuredDirs.add(target)
  }
  return target
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
      let linked = false
      try {
        const sha = await sha256File(live.file, (p) => (live.pct = p))
        const r = await api.instant(target, live.file.name, sha, live.file.size)
        linked = r.data.linked
      } catch {
        // 哈希/探测失败不阻塞,老老实实走真实上传
      }
      if (linked) {
        live.status = 'instant'
        live.pct = 100
        emit('uploaded')
        continue
      }

      live.status = 'uploading'
      live.pct = 0
      await api.upload(target, live.file, (pct) => (live.pct = pct))
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
