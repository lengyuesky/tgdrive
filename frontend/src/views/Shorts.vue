<template>
  <div
    ref="stage"
    class="shorts"
    tabindex="-1"
    @wheel.prevent="onWheel"
    @touchstart.passive="onTouchStart"
    @touchend.passive="onTouchEnd"
  >
    <!-- 顶栏 -->
    <div class="sh-top">
      <el-button text class="sh-btn" :icon="ArrowLeft" @click="exit">返回</el-button>
      <span class="sh-dir" :title="dir">{{ dir === '/' ? '整库' : dir }}</span>
      <span class="sh-flex" />
      <el-button text class="sh-btn" :icon="Setting" @click="gotoSetting">
        取材文件夹设置
      </el-button>
    </div>

    <!-- 播放区 -->
    <div v-if="current" class="sh-stage">
      <video
        ref="vid"
        :key="current.path"
        :src="src"
        :poster="api.thumbnailUrl(current.path)"
        class="sh-video"
        autoplay
        playsinline
        loop
        :muted="muted"
        @click="togglePlay"
        @playing="loading = false"
        @waiting="loading = true"
        @error="onError"
      />
      <div v-if="loading" class="sh-loading"><el-icon class="spin"><Loading /></el-icon></div>
      <div v-if="failed" class="sh-failed">
        这个视频浏览器放不了,试试下载
        <el-button text class="sh-btn" @click="go(1)">看下一个</el-button>
      </div>
    </div>
    <div v-else-if="!loadingList" class="sh-empty">
      <el-empty :description="`${dir === '/' ? '库里' : dir + ' 下'}没有可播放的视频`" />
      <el-button type="primary" @click="gotoSetting">去设置取材文件夹</el-button>
    </div>

    <!-- 右侧竖排操作 -->
    <div v-if="current" class="sh-side">
      <button class="sh-act" :class="{ on: current.favorite }" title="收藏" @click="toggleFav">
        <el-icon><StarFilled v-if="current.favorite" /><Star v-else /></el-icon>
        <span>收藏</span>
      </button>
      <button class="sh-act" :class="{ on: !muted }" title="静音 / 取消静音(M)" @click="toggleMute">
        <el-icon><Mute v-if="muted" /><Microphone v-else /></el-icon>
        <span>{{ muted ? '静音中' : '有声' }}</span>
      </button>
      <button class="sh-act" title="下载" @click="download">
        <el-icon><Download /></el-icon>
        <span>下载</span>
      </button>
      <button class="sh-act" title="重新洗牌" @click="reshuffle">
        <el-icon><Refresh /></el-icon>
        <span>洗牌</span>
      </button>
    </div>

    <!-- 底栏 -->
    <div v-if="current" class="sh-bottom">
      <div class="sh-name" :title="current.path">{{ current.name }}</div>
      <div class="sh-meta">
        {{ fmtSize(current.size) }} · {{ index + 1 }} / {{ queue.length }}
        <span class="sh-hint hide-narrow">· ↑↓ 或滑动切换 · 空格暂停 · M 静音 · Esc 退出</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import {
  ArrowLeft, Download, Loading, Microphone, Mute, Refresh, Setting, Star, StarFilled,
} from '@element-plus/icons-vue'
import { api, errMsg, fmtSize, VIDEO_EXT, type SearchHit } from '../api'

const router = useRouter()
const route = useRoute()
const stage = ref<HTMLElement>()
const vid = ref<HTMLVideoElement>()

/** 取材目录,来自后台设置(「流式播放」分区);'/' 表示整库 */
const dir = ref('/')
const queue = ref<SearchHit[]>([])
const index = ref(0)
const loadingList = ref(true)
/** 当前这条正在缓冲 */
const loading = ref(true)
const failed = ref(false)
// 默认静音起播,否则浏览器会拦下自动播放;用户点开声音后记住选择。
const muted = ref(localStorage.getItem('tgdrive.shortsMuted') !== '0')
watch(muted, (v) => localStorage.setItem('tgdrive.shortsMuted', v ? '1' : '0'))

const current = computed<SearchHit | undefined>(() => queue.value[index.value])
const src = computed(() => (current.value ? api.downloadUrl(current.value.path, true) : ''))

function shuffle<T>(list: T[]): T[] {
  const out = list.slice()
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1))
    ;[out[i], out[j]] = [out[j], out[i]]
  }
  return out
}

/**
 * 立即断开当前媒体。只 pause 不保证浏览器停掉预加载,清空 src 后 load()
 * 才会主动取消仍在进行的 Range 请求(同 VideoPlayer.stop)。
 */
function stopVideo() {
  const v = vid.value
  if (!v) return
  v.pause()
  v.removeAttribute('src')
  v.load()
}

/** 读后台设置的取材目录,再按它拉取随机队列 */
async function load() {
  loadingList.value = true
  stopVideo()
  try {
    const { data: info } = await api.info()
    dir.value = info.shorts_dir || '/'
    const r = await api.search({
      under: dir.value,
      kind: 'file',
      extensions: VIDEO_EXT.join(','),
      limit: 2000,
    })
    queue.value = shuffle(r.data.results)
    index.value = 0
    loading.value = queue.value.length > 0
    failed.value = false
  } catch (e) {
    ElMessage.error(errMsg(e))
    queue.value = []
  } finally {
    loadingList.value = false
  }
  await nextTick()
  stage.value?.focus()
}

/** 取材范围在后台设置里改 */
function gotoSetting() {
  stopVideo()
  router.push({ path: '/admin', query: { s: 'stream' } })
}

/** 切到上/下一条;到头回绕,形成刷不完的体验 */
function go(delta: number) {
  if (queue.value.length < 2) return
  stopVideo()
  failed.value = false
  loading.value = true
  index.value = (index.value + delta + queue.value.length) % queue.value.length
}

function togglePlay() {
  const v = vid.value
  if (!v) return
  if (v.paused) void v.play().catch(() => {})
  else v.pause()
}

function toggleMute() {
  muted.value = !muted.value
  const v = vid.value
  if (v) {
    v.muted = muted.value
    // 从静音切到有声时浏览器可能已暂停,补一次播放
    if (!muted.value) void v.play().catch(() => {})
  }
}

async function toggleFav() {
  const hit = current.value
  if (!hit) return
  try {
    const r = await api.favorite(hit.path, !hit.favorite)
    hit.favorite = r.data.favorite
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

function download() {
  if (current.value) window.open(api.downloadUrl(current.value.path), '_blank')
}

function reshuffle() {
  if (!queue.value.length) return
  stopVideo()
  queue.value = shuffle(queue.value)
  index.value = 0
  loading.value = true
  failed.value = false
}

function onError() {
  loading.value = false
  failed.value = true
}

/** 退出回到进来时所在的目录(直接访问 /shorts 时回根目录) */
function exit() {
  stopVideo()
  const from = typeof route.query.from === 'string' ? route.query.from : '/'
  router.push('/files' + (from.startsWith('/') ? from : '/' + from))
}

// 滚轮切换需要节流,否则一次惯性滚动会连跳很多条(每条都会真去拉流)。
let wheelAt = 0
function onWheel(e: WheelEvent) {
  const now = performance.now()
  if (now - wheelAt < 400 || Math.abs(e.deltaY) < 8) return
  wheelAt = now
  go(e.deltaY > 0 ? 1 : -1)
}

let touchY = 0
function onTouchStart(e: TouchEvent) {
  touchY = e.changedTouches[0]?.clientY ?? 0
}
function onTouchEnd(e: TouchEvent) {
  const dy = (e.changedTouches[0]?.clientY ?? 0) - touchY
  if (Math.abs(dy) > 60) go(dy < 0 ? 1 : -1)
}

function onKey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement)?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA') return
  switch (e.key) {
    case 'ArrowDown':
    case 'PageDown':
      e.preventDefault()
      go(1)
      break
    case 'ArrowUp':
    case 'PageUp':
      e.preventDefault()
      go(-1)
      break
    case ' ':
      e.preventDefault()
      togglePlay()
      break
    case 'm':
    case 'M':
      toggleMute()
      break
    case 'Escape':
      exit()
      break
  }
}

onMounted(async () => {
  document.addEventListener('keydown', onKey)
  await load()
})
onUnmounted(() => {
  document.removeEventListener('keydown', onKey)
  stopVideo()
})
</script>

<style scoped>
.shorts {
  position: fixed;
  inset: 0;
  background: #000;
  color: #f1f5f9;
  outline: none;
  overflow: hidden;
  overscroll-behavior: none;
}
.sh-stage {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
}
.sh-video {
  width: 100%;
  height: 100%;
  object-fit: contain;
  background: #000;
}
.sh-top {
  position: absolute;
  z-index: 2;
  top: 0;
  left: 0;
  right: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  background: linear-gradient(rgba(0, 0, 0, 0.65), transparent);
}
.sh-dir {
  font-size: 13px;
  color: #cbd5e1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 40vw;
}
.sh-flex {
  flex: 1;
}
.sh-btn {
  color: #e2e8f0;
}
.sh-btn:hover {
  color: #fff;
}
.sh-side {
  position: absolute;
  z-index: 2;
  right: 12px;
  bottom: 110px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.sh-act {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 3px;
  width: 52px;
  padding: 0;
  border: none;
  background: transparent;
  color: #e2e8f0;
  font-size: 11px;
  cursor: pointer;
  text-shadow: 0 1px 3px rgba(0, 0, 0, 0.8);
}
.sh-act .el-icon {
  font-size: 26px;
}
.sh-act:hover {
  color: #fff;
}
.sh-act.on {
  color: var(--el-color-primary);
}
.sh-bottom {
  position: absolute;
  z-index: 2;
  left: 0;
  right: 76px;
  bottom: 0;
  padding: 20px 16px 16px;
  background: linear-gradient(transparent, rgba(0, 0, 0, 0.7));
}
.sh-name {
  font-size: 15px;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sh-meta {
  margin-top: 4px;
  font-size: 12px;
  color: #94a3b8;
}
.sh-loading,
.sh-failed {
  position: absolute;
  left: 50%;
  top: 50%;
  transform: translate(-50%, -50%);
  font-size: 14px;
  color: #cbd5e1;
}
.sh-loading .el-icon {
  font-size: 32px;
}
.spin {
  animation: sh-spin 1s linear infinite;
}
@keyframes sh-spin {
  to {
    transform: rotate(360deg);
  }
}
.sh-empty {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
}
@media (max-width: 900px) {
  .hide-narrow {
    display: none;
  }
}
</style>
