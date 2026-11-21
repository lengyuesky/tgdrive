<template>
  <Teleport to="body">
    <div ref="dialog" class="image-preview" role="dialog" aria-modal="true" aria-label="图片预览" @click.self="$emit('close')">
      <header>
        <span>{{ current?.name }} · 仅浏览当前已加载的图片</span>
        <button ref="closeButton" aria-label="关闭图片预览" @click="$emit('close')">关闭</button>
      </header>
      <div ref="stage" class="preview-stage" @touchstart="touchStart" @touchmove="touchMove" @touchend="touchEnd" @touchcancel="resetGesture">
        <img v-for="item in windowItems" :key="item.key" :src="item.url" :alt="item.entry.name"
          :class="{ neighbor: item.index !== index }" :aria-hidden="item.index !== index"
          :style="item.index === index ? { transform: `translate(${pan.x}px, ${pan.y}px) scale(${scale}) rotate(${rotation}deg)` } : undefined"
          @error="failed.add(item.key)" />
        <p v-if="currentItem && failed.has(currentItem.key)" class="preview-error">图片加载失败，请下载后查看。</p>
      </div>
      <footer>
        <button aria-label="上一张图片" :disabled="previous < 0" @click="step(previous)">上一张</button>
        <button aria-label="缩小图片" @click="scale = Math.max(0.25, scale - 0.25)">缩小</button>
        <button aria-label="还原图片" @click="resetImage">还原</button>
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
import { isTopOverlay, registerOverlay } from '../mobile/overlays'
import { distance, boundedScale, swipeDirection, type Point } from '../mobile/image-gestures'
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
const stage = ref<HTMLElement>()
const pan = reactive({ x: 0, y: 0 })
const scale = ref(1)
let releaseOverlay: (() => void) | undefined
let start: Point | undefined
let previousPoint: Point | undefined
let startDistance = 0
let startScale = 1
let multiple = false
const point = (touch: Touch): Point => ({ x: touch.clientX, y: touch.clientY })
function resetGesture() { start = undefined; previousPoint = undefined; startDistance = 0; multiple = false }
function resetImage() { scale.value = 1; rotation.value = 0; pan.x = 0; pan.y = 0; resetGesture() }
function touchStart(event: TouchEvent) {
  if (event.touches.length === 1) { start = point(event.touches[0]!); previousPoint = start }
  if (event.touches.length > 1) { multiple = true; startDistance = distance(point(event.touches[0]!), point(event.touches[1]!)); startScale = scale.value }
}
function touchMove(event: TouchEvent) {
  if (event.touches.length === 2 && startDistance) {
    event.preventDefault(); scale.value = boundedScale(startScale * distance(point(event.touches[0]!), point(event.touches[1]!)) / startDistance)
  } else if (event.touches.length === 1 && previousPoint && scale.value > 1) {
    event.preventDefault()
    const next = point(event.touches[0]!)
    const maxX = (stage.value?.clientWidth ?? 300) * (scale.value - 1) / 2
    const maxY = (stage.value?.clientHeight ?? 500) * (scale.value - 1) / 2
    pan.x = Math.max(-maxX, Math.min(maxX, pan.x + next.x - previousPoint.x))
    pan.y = Math.max(-maxY, Math.min(maxY, pan.y + next.y - previousPoint.y))
    previousPoint = next
  }
}
function touchEnd(event: TouchEvent) {
  if (event.touches.length) return
  if (start && event.changedTouches[0]) {
    const end = point(event.changedTouches[0])
    const direction = swipeDirection(end.x - start.x, end.y - start.y, scale.value, multiple)
    if (direction) step(direction > 0 ? next.value : previous.value)
  }
  resetGesture()
}
const rotation = ref(0)
const closeButton = ref<HTMLButtonElement>()
const dialog = ref<HTMLElement>()
const close = () => emit('close')
function step(index: number) { if (index >= 0) emit('update:index', index) }
watch(() => props.index, () => { resetImage(); failed.clear() })
function keydown(event: KeyboardEvent) {
  if (!isTopOverlay(close) || event.isComposing || event.keyCode === 229) return
  if (event.key === 'Escape') close()
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
onMounted(() => { releaseOverlay = registerOverlay(close, () => dialog.value ?? null); closeButton.value?.focus(); document.addEventListener('keydown', keydown, true) })
onUnmounted(() => { document.removeEventListener('keydown', keydown, true); releaseOverlay?.() })
</script>

<style scoped>
.image-preview { position: fixed; inset: 0; padding: var(--safe-top, 0px) var(--safe-right, 0px) var(--safe-bottom, 0px) var(--safe-left, 0px); box-sizing: border-box; z-index: 3000; background: rgba(0, 0, 0, .92); color: #fff; display: flex; flex-direction: column; }
.image-preview header, .image-preview footer { display: flex; align-items: center; justify-content: center; gap: 12px; padding: 12px; z-index: 1; background: #151515; flex-wrap: wrap; }
.image-preview header span { flex: 1; overflow-wrap: anywhere; }
.image-preview button { color: #fff; background: #333; border: 1px solid #888; border-radius: 6px; padding: 8px 12px; cursor: pointer; }
.image-preview button:disabled { opacity: .4; cursor: default; }
.preview-stage { touch-action: none; min-height: 0; flex: 1; display: flex; align-items: center; justify-content: center; overflow: auto; position: relative; }
.preview-stage img { max-width: 100%; max-height: 100%; object-fit: contain; }
.preview-stage img.neighbor { display: none; }
.preview-error { position: absolute; padding: 12px; background: #333; }
@media (max-width: 767px) {
  .image-preview header { padding: 8px 12px; }.image-preview header span { font-size: 13px; max-height: 3em; overflow: hidden; }
  .image-preview footer { gap: 4px; padding: 8px; display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); }
  .image-preview button { min-height: 44px; font-size: 14px; padding: 8px; }
  .preview-stage { overflow: hidden; }
}
</style>
