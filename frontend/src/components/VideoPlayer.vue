<template>
  <div ref="wrap" class="vp" tabindex="-1">
    <div class="vp-stage" :style="{ height: stageH }">
      <video
        ref="vid"
        :src="activeSrc || undefined"
        controls
        preload="none"
        playsinline
        :loop="loop"
        :style="videoStyle"
        @loadedmetadata="onMeta"
        @ended="emit('ended')"
        @ratechange="onRateChange"
      />
      <el-button
        v-if="!activeSrc"
        class="vp-start"
        type="primary"
        size="large"
        :icon="VideoPlay"
        @click="startPlayback"
      >
        点击播放
      </el-button>
    </div>
    <div class="vp-bar">
      <el-dropdown trigger="click" @command="setSpeed">
        <el-button text size="small" class="vp-btn">{{ speed }}×</el-button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item
              v-for="s in SPEEDS"
              :key="s"
              :command="s"
              :class="{ 'vp-speed-on': s === speed }"
              >{{ s }}×</el-dropdown-item
            >
          </el-dropdown-menu>
        </template>
      </el-dropdown>
      <el-tooltip content="旋转 90°(竖屏视频存歪了用它扶正)">
        <el-button text size="small" class="vp-btn" :icon="RefreshRight" @click="rotate" />
      </el-tooltip>
      <el-tooltip v-if="pipSupported" content="画中画(小窗悬浮,可切到别的页面)">
        <el-button text size="small" class="vp-btn" :icon="CopyDocument" @click="togglePip" />
      </el-tooltip>
      <el-tooltip content="循环播放">
        <el-button
          text
          size="small"
          class="vp-btn"
          :class="{ on: loop }"
          :icon="Refresh"
          @click="loop = !loop"
        />
      </el-tooltip>
      <el-tooltip content="全屏(含此工具栏;Esc 退出)">
        <el-button text size="small" class="vp-btn" :icon="FullScreen" @click="toggleFullscreen" />
      </el-tooltip>
      <span class="vp-hint hide-narrow">
        空格暂停 · ←→ 快进退 10s · ↑↓ 音量 · M 静音 · F 全屏 · R 旋转
      </span>
      <span class="vp-flex" />
      <span v-if="dims" class="vp-dims">{{ dims }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  CopyDocument,
  FullScreen,
  Refresh,
  RefreshRight,
  VideoPlay,
} from '@element-plus/icons-vue'

const props = defineProps<{ src: string }>()
const emit = defineEmits<{ ended: []; meta: [w: number, h: number] }>()

const wrap = ref<HTMLElement>()
const vid = ref<HTMLVideoElement>()
// 只有用户明确点击播放后才把地址交给 <video>，避免打开预览就触发 Telegram 下载。
const activeSrc = ref('')
const loop = ref(false)
const rot = ref(0)
const natural = ref<{ w: number; h: number } | null>(null)
const fit = ref<{ w: number; h: number } | null>(null)
const isFs = ref(false)

const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3]
const speed = ref(Number(localStorage.getItem('tgdrive.playSpeed')) || 1)

const pipSupported =
  typeof document !== 'undefined' && 'pictureInPictureEnabled' in document
    ? document.pictureInPictureEnabled
    : false

const dims = computed(() =>
  natural.value ? `${natural.value.w}×${natural.value.h}` : '',
)
/** 播放区高度:全屏时吃满,平时给工具栏留出空间 */
const stageH = computed(() => (isFs.value ? 'calc(100vh - 44px)' : '68vh'))

const videoStyle = computed(() => {
  // 元数据未就绪时先按容器等比缩放;就绪后用精确尺寸(旋转 90° 时
  // 布局盒宽可超出舞台宽,visual 宽高互换,不能用 max-width 钳制)
  if (!fit.value) {
    return { width: '100%', height: '100%', objectFit: 'contain' as const }
  }
  return {
    transform: `rotate(${rot.value}deg)`,
    width: `${fit.value.w}px`,
    height: `${fit.value.h}px`,
  }
})

/** 按旋转角度把视频精确装进播放区(旋转 90°/270° 时视觉宽高互换) */
function layout() {
  const stage = wrap.value?.querySelector<HTMLElement>('.vp-stage')
  const n = natural.value
  if (!stage || !n) return
  const W = stage.clientWidth
  const H = stage.clientHeight
  if (!W || !H) return
  const sideways = rot.value % 180 !== 0
  // 旋转后视觉宽 = 布局高,视觉高 = 布局宽,所以约束互换
  const scale = sideways ? Math.min(H / n.w, W / n.h) : Math.min(W / n.w, H / n.h)
  fit.value = { w: Math.floor(n.w * scale), h: Math.floor(n.h * scale) }
}

function onMeta() {
  const v = vid.value
  if (!v) return
  natural.value = { w: v.videoWidth || 16, h: v.videoHeight || 9 }
  rot.value = 0
  v.playbackRate = speed.value
  emit('meta', natural.value.w, natural.value.h)
  layout()
}

/** 用户明确播放后才装载媒体地址。 */
async function startPlayback() {
  const v = vid.value
  if (!v) return
  if (activeSrc.value !== props.src) {
    activeSrc.value = props.src
    await nextTick()
  }
  v.playbackRate = speed.value
  try {
    await v.play()
  } catch {
    // 浏览器策略或格式不支持时保留原生控件，让用户可以重试或看到错误。
  }
}

/**
 * 立即断开媒体资源。单纯 pause 只会停播放，不保证浏览器停止预加载；
 * 清空 src 后调用 load 才会主动取消仍在进行的 Range 请求。
 */
function stop() {
  const v = vid.value
  if (v) {
    v.pause()
    v.removeAttribute('src')
    v.load()
  }
  activeSrc.value = ''
  natural.value = null
  fit.value = null
}

function rotate() {
  rot.value = (rot.value + 90) % 360
  layout()
}

function setSpeed(s: number) {
  speed.value = s
  localStorage.setItem('tgdrive.playSpeed', String(s))
  if (vid.value) vid.value.playbackRate = s
}

/** 用户经原生控件改倍速时同步状态 */
function onRateChange() {
  const r = vid.value?.playbackRate
  if (r && r !== speed.value) speed.value = r
}

async function togglePip() {
  const v = vid.value
  if (!v) return
  try {
    if (document.pictureInPictureElement) await document.exitPictureInPicture()
    else await v.requestPictureInPicture()
  } catch {
    /* 浏览器拒绝(未播放过等)时忽略 */
  }
}

function toggleFullscreen() {
  if (document.fullscreenElement) document.exitFullscreen()
  else wrap.value?.requestFullscreen()
}

function onFsChange() {
  isFs.value = document.fullscreenElement === wrap.value
  requestAnimationFrame(layout)
}

function onKey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement)?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA') return
  const v = vid.value
  if (!v) return
  switch (e.key) {
    case ' ':
      e.preventDefault()
      if (v.paused) void startPlayback()
      else v.pause()
      break
    case 'ArrowLeft':
      e.preventDefault()
      v.currentTime = Math.max(0, v.currentTime - 10)
      break
    case 'ArrowRight':
      e.preventDefault()
      v.currentTime = Math.min(v.duration || Infinity, v.currentTime + 10)
      break
    case 'ArrowUp':
      e.preventDefault()
      v.volume = Math.min(1, v.volume + 0.1)
      break
    case 'ArrowDown':
      e.preventDefault()
      v.volume = Math.max(0, v.volume - 0.1)
      break
    case 'm':
    case 'M':
      v.muted = !v.muted
      break
    case 'f':
    case 'F':
      toggleFullscreen()
      break
    case 'r':
    case 'R':
      rotate()
      break
  }
}

let ro: ResizeObserver | undefined
onMounted(() => {
  document.addEventListener('keydown', onKey)
  document.addEventListener('fullscreenchange', onFsChange)
  ro = new ResizeObserver(() => layout())
  const stage = wrap.value?.querySelector('.vp-stage')
  if (stage) ro.observe(stage)
})
onUnmounted(() => {
  document.removeEventListener('keydown', onKey)
  document.removeEventListener('fullscreenchange', onFsChange)
  ro?.disconnect()
})
// 在 Vue 开始移除 DOM 前主动断流；仅依赖浏览器回收元素可能让下载继续一段时间。
onBeforeUnmount(stop)
watch(() => props.src, stop)

defineExpose({ play: startPlayback, stop })
</script>

<style scoped>
.vp {
  width: 100%;
  outline: none;
  background: #0b0e17;
  border-radius: 10px;
  overflow: hidden;
}
.vp:fullscreen {
  border-radius: 0;
}
.vp-stage {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
  background: #000;
}
.vp-start {
  position: absolute;
  z-index: 1;
  left: 50%;
  top: 50%;
  transform: translate(-50%, -50%);
}
.vp-stage video {
  outline: none;
  background: #000;
  transition: transform 0.2s;
}
.vp-bar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 5px 10px;
  min-height: 34px;
  box-sizing: border-box;
}
.vp-btn {
  color: #cbd5e1;
  font-weight: 600;
}
.vp-btn:hover {
  color: #fff;
}
.vp-btn.on {
  color: var(--el-color-primary);
}
.vp-hint {
  margin-left: 10px;
  font-size: 12px;
  color: #64748b;
}
.vp-flex {
  flex: 1;
}
.vp-dims {
  font-size: 12px;
  color: #64748b;
}
.vp-speed-on {
  color: var(--el-color-primary);
  font-weight: 700;
}
@media (max-width: 900px) {
  .hide-narrow {
    display: none;
  }
}
</style>
