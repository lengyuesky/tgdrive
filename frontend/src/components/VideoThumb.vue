<template>
  <div ref="root" class="vthumb">
    <!-- 这里只加载 Telegram 媒体消息的小图，绝不把原视频挂进列表。 -->
    <img
      v-if="active && src && !failed"
      class="vthumb-image"
      :src="src"
      loading="lazy"
      decoding="async"
      @load="ready = true"
      @error="failed = true"
    >
    <div v-if="!ready || failed" class="vthumb-fallback">
      <el-icon color="#8b5cf6"><Film /></el-icon>
    </div>
    <div v-else class="vthumb-play">
      <el-icon><VideoPlay /></el-icon>
    </div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { Film, VideoPlay } from '@element-plus/icons-vue'

const props = defineProps<{ src: string }>()

const root = ref<HTMLElement>()
const active = ref(false)
const ready = ref(false)
const failed = ref(false)
let ob: IntersectionObserver | undefined

watch(
  () => props.src,
  () => {
    ready.value = false
    failed.value = false
  },
)

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
.vthumb-image {
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
