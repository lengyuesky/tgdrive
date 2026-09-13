<template>
  <div ref="element" class="virtual-viewport" data-testid="virtual-viewport" @scroll="onScroll">
    <div v-if="$slots.header" ref="header" class="viewport-header"><slot name="header" /></div>
    <div class="virtual-space" :style="{ height: virtualizer.getTotalSize() + 'px' }">
      <div v-for="item in virtualizer.getVirtualItems()" :key="String(item.key)"
        class="virtual-row" :data-row-index="item.index"
        :style="{ height: item.size + 'px', transform: `translateY(${item.start - headerHeight}px)` }">
        <slot :index="item.index" />
      </div>
    </div>
    <slot name="footer" />
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useVirtualizer } from '@tanstack/vue-virtual'
import { boundedRange } from '../directory/layout'

const props = withDefaults(defineProps<{
  count: number
  active?: boolean
  estimateSize: (index: number) => number
  itemKey: (index: number) => string | number
  maxRows?: number
  rowHeight: number
}>(), { maxRows: 200, active: true })
const emit = defineEmits<{
  nearEnd: []
  scroll: [offset: number]
  resize: [width: number]
}>()
const element = ref<HTMLElement>()
const header = ref<HTMLElement>()
const headerHeight = ref(0)
const height = ref(600)
const virtualizer = useVirtualizer(computed(() => ({
  count: props.count,
  enabled: props.active,
  getScrollElement: () => element.value ?? null,
  estimateSize: props.estimateSize,
  getItemKey: props.itemKey,
  overscan: 0,
  scrollMargin: headerHeight.value,
  scrollPaddingStart: headerHeight.value,
  rangeExtractor: (range: { startIndex: number; endIndex: number; count: number }) =>
    boundedRange(range.startIndex, range.endIndex, range.count,
      Math.ceil(height.value / props.rowHeight), props.maxRows),
})))
function checkEnd() {
  const el = element.value
  if (props.active && el && props.count && headerHeight.value + virtualizer.value.getTotalSize() - el.scrollTop - el.clientHeight <= el.clientHeight * 2) {
    emit('nearEnd')
  }
}
function onScroll() {
  emit('scroll', element.value?.scrollTop ?? 0)
  checkEnd()
}
let observer: ResizeObserver | undefined
onMounted(() => {
  observer = new ResizeObserver((sizes) => {
    for (const size of sizes) {
      if (size.target === header.value) headerHeight.value = size.contentRect.height
      else if (size.target === element.value) {
        height.value = size.contentRect.height || 600
        emit('resize', size.contentRect.width)
      }
    }
  })
  if (element.value) observer.observe(element.value)
  if (header.value) observer.observe(header.value)
})
watch(header, (next, previous) => {
  if (previous) observer?.unobserve(previous)
  headerHeight.value = next?.getBoundingClientRect().height ?? 0
  if (next) observer?.observe(next)
})
onBeforeUnmount(() => observer?.disconnect())
watch(() => props.count, async () => {
  await nextTick()
  checkEnd()
})
function firstVisibleIndex() {
  const offset = (element.value?.scrollTop ?? 0) + headerHeight.value
  return virtualizer.value.getVirtualItems().find((item) => item.end > offset)?.index ?? 0
}
function scrollToIndex(index: number) {
  if (index === 0) { reset(); return }
  virtualizer.value.scrollToIndex(index, { align: 'start' })
}
function reset() {
  if (element.value) element.value.scrollTop = 0
  virtualizer.value.scrollToOffset(0)
  emit('scroll', 0)
}
function restoreOffset(offset: number) {
  virtualizer.value.measure()
  if (element.value) element.value.scrollTop = offset
  virtualizer.value.scrollToOffset(offset)
}
defineExpose({ firstVisibleIndex, scrollToIndex, reset, restoreOffset, measure: () => virtualizer.value.measure() })
</script>

<style scoped>
.virtual-viewport { position: relative; overflow: auto; height: min(720px, calc(100dvh - 230px)); max-height: 720px; min-height: 240px; overflow-anchor: none; }
.virtual-space { position: relative; width: 100%; }
.virtual-row { position: absolute; top: 0; left: 0; width: 100%; box-sizing: border-box; }
.viewport-header { position: sticky; top: 0; z-index: 5; background: var(--el-bg-color); }
</style>
