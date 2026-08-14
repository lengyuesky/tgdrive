<template>
  <div ref="root" class="vthumb">
    <!-- 进入视口才挂 video,靠 Range 请求只拉元数据与首帧,不下载整个文件 -->
    <video
      v-if="active && !failed"
      class="vthumb-video"
      :src="src + '#t=0.1'"
      preload="metadata"
      muted
      playsinline
      disablepictureinpicture
      @loadeddata="ready = true"
      @error="failed = true"
    />
    <div v-if="!ready || failed" class="vthumb-fallback">
      <el-icon color="#8b5cf6"><Film /></el-icon>
    </div>
    <div v-else class="vthumb-play">
      <el-icon><VideoPlay /></el-icon>
    </div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { Film, VideoPlay } from '@element-plus/icons-vue'

defineProps<{ src: string }>()

const root = ref<HTMLElement>()
const active = ref(false)
const ready = ref(false)
const failed = ref(false)
let ob: IntersectionObserver | undefined

onMounted(() => {
  ob = new IntersectionObserver(
    (es) => {
      if (es.some((e) => e.isIntersecting)) {
        active.value = true
        ob?.disconnect()
      }
    },
    { rootMargin: '200px' },
  )
  if (root.value) ob.observe(root.value)
})
onUnmounted(() => ob?.disconnect())
</script>

<style scoped>
.vthumb {
  position: relative;
  background: var(--tile-media-bg);
  overflow: hidden;
  /* 尺寸由宿主决定:列表里是 36px 小图,网格里由 .tile-media 撑满 */
}
.vthumb-video {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
  pointer-events: none;
}
.vthumb-fallback {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 28px; /* 宿主可通过 .vthumb-fallback 覆盖 */
}
.vthumb-play {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #fff;
  font-size: 26px;
  background: radial-gradient(circle at center, rgba(15, 20, 35, 0.35) 0, transparent 65%);
  pointer-events: none;
}
.vthumb-play .el-icon {
  filter: drop-shadow(0 1px 6px rgba(10, 14, 26, 0.6));
}
</style>
