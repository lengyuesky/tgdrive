<template>
  <ResponsivePanel
    v-model="visible"
    :title="name"
    :width="dialogWidth"
    top="4vh"
    destroy-on-close
  >
    <div class="preview-body" :class="{ cinema: kind === 'video' }">
      <img v-if="kind === 'image'" :src="url" class="preview-media" />
      <VideoPlayer
        v-else-if="kind === 'video'"
        ref="player"
        :key="url"
        :src="url"
        @meta="onVideoMeta"
        @ended="onEnded"
      />
      <audio v-else-if="kind === 'audio'" :src="url" controls style="width: 100%" />
      <iframe v-else-if="kind === 'pdf'" :src="url" class="preview-frame" />
      <pre v-else-if="kind === 'text'" class="preview-text">{{ text }}</pre>
      <el-empty v-else description="该类型暂不支持预览,请直接下载" />
    </div>
    <template #footer>
      <div class="preview-foot">
        <template v-if="playlist.length > 1">
          <el-button :disabled="playIndex <= 0" :icon="ArrowLeft" @click="step(-1)">
            上一个
          </el-button>
          <span class="play-pos">{{ playIndex + 1 }} / {{ playlist.length }}</span>
          <el-button :disabled="playIndex >= playlist.length - 1" @click="step(1)">
            下一个<el-icon class="el-icon--right"><ArrowRight /></el-icon>
          </el-button>
          <el-checkbox
            v-if="kind === 'video'"
            v-model="autoNext"
            class="auto-next"
            label="连播"
          />
          <span class="spacer" />
        </template>
        <el-button @click="download">下载</el-button>
        <el-button type="primary" @click="visible = false">关闭</el-button>
      </div>
    </template>
  </ResponsivePanel>
</template>

<script setup lang="ts">
import ResponsivePanel from './ResponsivePanel.vue'
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import axios from 'axios'
import { ArrowLeft, ArrowRight } from '@element-plus/icons-vue'
import { api } from '../api'
import VideoPlayer from './VideoPlayer.vue'

const visible = ref(false)
const path = ref('')
/** 同目录可预览文件列表(视频连播/切换用);单独打开时为空 */
const playlist = ref<string[]>([])
const name = computed(() => path.value.split('/').pop() ?? '')
const url = computed(() => api.downloadUrl(path.value, true))
const text = ref('')
/** 当前视频是否竖屏(9:16 等),竖屏时收窄对话框避免两侧大黑边 */
const portrait = ref(false)
// 连播必须由用户主动开启，避免仅打开预览后连续拉取整个目录的视频。
const autoNext = ref(localStorage.getItem('tgdrive.autoNext') === '1')
watch(autoNext, (v) => localStorage.setItem('tgdrive.autoNext', v ? '1' : '0'))
const player = ref<InstanceType<typeof VideoPlayer>>()

// 对话框关闭动画期间组件仍然存在，因此在 visible 变更时立即主动断流。
watch(
  visible,
  (v) => {
    if (!v) player.value?.stop()
  },
  { flush: 'sync' },
)

const playIndex = computed(() => playlist.value.indexOf(path.value))

/** 窄屏(手机)时视频直接全屏弹窗 */
const narrow = ref(false)
const mq = typeof window !== 'undefined' ? window.matchMedia('(max-width: 700px)') : null
function onMq() {
  narrow.value = mq?.matches ?? false
}
onMounted(() => {
  onMq()
  mq?.addEventListener('change', onMq)
})
onUnmounted(() => mq?.removeEventListener('change', onMq))

const EXT: Record<string, string[]> = {
  // SVG 可携带脚本，不在网盘同源下内联；仍可作为普通文件下载。
  image: ['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'avif', 'ico'],
  // mkv 在 Chrome/Edge 可播 H.264/VP9 轨(具体取决于浏览器解码器)
  video: ['mp4', 'webm', 'ogv', 'mov', 'm4v', 'mkv'],
  audio: ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus'],
  pdf: ['pdf'],
  text: [
    'txt', 'md', 'log', 'json', 'yaml', 'yml', 'toml', 'ini', 'csv', 'xml', 'html', 'css',
    'js', 'ts', 'rs', 'py', 'go', 'java', 'c', 'h', 'cpp', 'sh', 'sql', 'conf',
  ],
}

const kind = computed(() => {
  const ext = name.value.split('.').pop()?.toLowerCase() ?? ''
  for (const [k, list] of Object.entries(EXT)) if (list.includes(ext)) return k
  return 'other'
})

const dialogWidth = computed(() => {
  if (kind.value !== 'video') return '70%'
  // 竖屏视频:按 68vh 播放区高度反推出合适宽度,加工具栏余量
  return portrait.value ? 'min(520px, 94vw)' : '82%'
})

function onVideoMeta(w: number, h: number) {
  portrait.value = h > w
}

async function onEnded() {
  if (autoNext.value && playIndex.value < playlist.value.length - 1) {
    step(1)
    await nextTick()
    await player.value?.play()
  }
}

async function loadText() {
  text.value = '加载中 ...'
  try {
    // 只取前 256KB,避免大文件卡死
    const r = await axios.get(url.value, {
      headers: { Range: 'bytes=0-262143' },
      responseType: 'text',
      transformResponse: [(d) => d],
    })
    text.value = r.data
  } catch {
    text.value = '(读取失败)'
  }
}

watch([visible, path], ([v]) => {
  if (v && kind.value === 'text') loadText()
})

function step(delta: number) {
  const next = playlist.value[playIndex.value + delta]
  if (next) {
    portrait.value = false
    path.value = next
  }
}

function open(p: string, list: string[] = []) {
  path.value = p
  playlist.value = list
  portrait.value = false
  visible.value = true
}

function download() {
  window.open(api.downloadUrl(path.value), '_blank')
}

function close() { visible.value = false; player.value?.stop() }
defineExpose({ open, close })
</script>

<style scoped>
.preview-body {
  display: flex;
  justify-content: center;
  max-height: 76vh;
  overflow: auto;
}
/* 影院模式:播放器自带黑底与圆角,容器不再滚动 */
.preview-body.cinema {
  overflow: hidden;
}
.preview-media {
  max-width: 100%;
  max-height: 70vh;
}
.preview-frame {
  width: 100%;
  height: 68vh;
  border: none;
}
.preview-text {
  width: 100%;
  white-space: pre-wrap;
  word-break: break-all;
  font-size: 13px;
  background: rgba(255, 255, 255, 0.5);
  border-radius: 10px;
  padding: 12px;
  margin: 0;
}
.preview-foot {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
}
.play-pos {
  font-size: 13px;
  color: var(--text-soft);
}
.auto-next {
  margin-left: 6px;
}
.spacer {
  flex: 1;
}
</style>
