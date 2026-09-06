<template>
  <Teleport to="body">
    <div class="image-preview" role="dialog" aria-modal="true" aria-label="图片预览" @click.self="$emit('close')">
      <header>
        <span>{{ current?.name }} · 仅浏览当前已加载的图片</span>
        <button ref="closeButton" aria-label="关闭图片预览" @click="$emit('close')">关闭</button>
      </header>
      <div class="preview-stage" @click.self="$emit('close')">
        <img v-for="item in windowItems" :key="item.key" :src="item.url" :alt="item.entry.name"
          :class="{ neighbor: item.index !== index }" :aria-hidden="item.index !== index"
          :style="item.index === index ? { transform: `scale(${scale}) rotate(${rotation}deg)` } : undefined"
          @error="failed.add(item.key)" />
        <p v-if="currentItem && failed.has(currentItem.key)" class="preview-error">图片加载失败，请下载后查看。</p>
      </div>
      <footer>
        <button aria-label="上一张图片" :disabled="previous < 0" @click="step(previous)">上一张</button>
        <button aria-label="缩小图片" @click="scale = Math.max(0.25, scale - 0.25)">缩小</button>
        <button aria-label="还原图片" @click="scale = 1; rotation = 0">还原</button>
        <button aria-label="放大图片" @click="scale = Math.min(4, scale + 0.25)">放大</button>
        <button aria-label="旋转图片" @click="rotation += 90">旋转</button>
        <button aria-label="下一张图片" :disabled="next < 0" @click="step(next)">下一张</button>
      </footer>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { api, type Entry } from '../api'
const props = defineProps<{ entries: readonly Entry[]; count: number; index: number; base: string }>()
const emit = defineEmits<{ close: []; 'update:index': [index: number] }>()
const isImage = (entry: Entry) => !entry.is_dir && /\.(jpe?g|png|gif|webp|bmp|avif|ico)$/i.test(entry.name)
function neighbor(direction: number) {
  for (let i = props.index + direction; i >= 0 && i < props.count; i += direction) {
    if (isImage(props.entries[i]!)) return i
  }
  return -1
}
const previous = computed(() => neighbor(-1))
const next = computed(() => neighbor(1))
const current = computed(() => props.entries[props.index])
// 只构造当前和两侧各一张的 URL，不为所有已加载图片建立 urlList 或 Image 实例。
const windowItems = computed(() => [previous.value, props.index, next.value].filter((i) => i >= 0 && i < props.count).map((index) => {
  const entry = props.entries[index]!
  const path = props.base === '/' ? `/${entry.name}` : `${props.base}/${entry.name}`
  return { index, entry, key: JSON.stringify([path, entry.modified_at, entry.size, entry.sha256]), url: api.downloadUrl(path, true) }
}))
const currentItem = computed(() => windowItems.value.find((item) => item.index === props.index))
const failed = reactive(new Set<string>())
const scale = ref(1)
const rotation = ref(0)
const closeButton = ref<HTMLButtonElement>()
const oldFocus = document.activeElement as HTMLElement | null
function step(index: number) { if (index >= 0) emit('update:index', index) }
watch(() => props.index, () => { scale.value = 1; rotation.value = 0; failed.clear() })
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') emit('close')
  else if (event.key === 'ArrowLeft') step(previous.value)
  else if (event.key === 'ArrowRight') step(next.value)
  else if (event.key === 'Tab') {
    const buttons = [...closeButton.value!.closest('.image-preview')!.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')]
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement)
    buttons[(index + (event.shiftKey ? -1 : 1) + buttons.length) % buttons.length]?.focus()
  } else return
  event.preventDefault()
  event.stopImmediatePropagation()
}
onMounted(() => { closeButton.value?.focus(); document.addEventListener('keydown', keydown, true) })
onUnmounted(() => { document.removeEventListener('keydown', keydown, true); oldFocus?.focus() })
</script>

<style scoped>
.image-preview { position: fixed; inset: 0; z-index: 3000; background: rgba(0, 0, 0, .92); color: #fff; display: flex; flex-direction: column; }
.image-preview header, .image-preview footer { display: flex; align-items: center; justify-content: center; gap: 12px; padding: 12px; z-index: 1; background: #151515; flex-wrap: wrap; }
.image-preview header span { flex: 1; overflow-wrap: anywhere; }
.image-preview button { color: #fff; background: #333; border: 1px solid #888; border-radius: 6px; padding: 8px 12px; cursor: pointer; }
.image-preview button:disabled { opacity: .4; cursor: default; }
.preview-stage { min-height: 0; flex: 1; display: flex; align-items: center; justify-content: center; overflow: auto; position: relative; }
.preview-stage img { max-width: 100%; max-height: 100%; object-fit: contain; }
.preview-stage img.neighbor { display: none; }
.preview-error { position: absolute; padding: 12px; background: #333; }
</style>
