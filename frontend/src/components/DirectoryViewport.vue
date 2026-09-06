<template>
  <VirtualViewport ref="viewport" :count="rowCount" :estimate-size="estimateSize" :item-key="rowKey"
    :row-height="rowHeight" :max-rows="Math.floor(200 / columns)"
    @near-end="$emit('nearEnd')" @scroll="(offset) => $emit('scroll', offset)" @resize="resize">
    <template v-if="mode === 'list'" #header><slot name="header" /></template>
    <template #default="{ index }">
      <div v-if="row(index).group" class="timeline-heading">
        {{ row(index).group!.label }} · 已加载 {{ row(index).group!.count }} 项
      </div>
      <div v-else class="directory-row" :class="mode" :style="{ gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))` }">
        <div v-for="entry in rowEntries(index)" :key="entry.id" class="directory-cell" :data-entry-id="entry.id">
          <slot :entry="entry" :mode="mode" :generation="generation"
            :media-key="`${entry.id}:${entry.modified_at}:${entry.size}:${entry.sha256 ?? ''}`" />
        </div>
      </div>
    </template>
    <template #footer><slot name="footer" /></template>
  </VirtualViewport>
</template>

<script setup lang="ts">
import { computed, nextTick, ref, shallowRef, triggerRef, watch } from 'vue'
import type { DirectoryEntry } from '../api'
import { TimelineRows, type DirectoryRow, type DirectoryView } from '../directory/layout'
import VirtualViewport from './VirtualViewport.vue'
const props = defineProps<{
  entries: readonly DirectoryEntry[]
  count: number
  mode: DirectoryView
  generation: number
  revision: number
  indexOf: (id: number) => number | undefined
}>()
defineEmits<{ nearEnd: []; scroll: [offset: number] }>()
const viewport = ref<InstanceType<typeof VirtualViewport>>()
const width = ref(800)
const columns = computed(() => props.mode === 'list' ? 1 : Math.max(1, Math.min(20,
  Math.floor(width.value / (props.mode === 'grid' ? 164 : 246)))))
const rowHeight = computed(() => props.mode === 'list' ? 56 : props.mode === 'grid' ? 192 : 272)
const timeline = shallowRef(new TimelineRows(columns.value))
watch(() => [props.generation, props.mode, columns.value] as const, () => {
  timeline.value = new TimelineRows(columns.value)
  if (props.mode === 'timeline') timeline.value.append(props.entries)
  void nextTick(() => viewport.value?.measure())
}, { flush: 'pre' })
watch(() => props.count, () => {
  if (props.mode === 'timeline') {
    timeline.value.append(props.entries)
    triggerRef(timeline)
  }
}, { immediate: true, flush: 'pre' })
watch(() => props.generation, () => viewport.value?.reset(), { flush: 'post' })
const rowCount = computed(() => props.mode === 'timeline' ? timeline.value.rows.length : Math.ceil(props.count / columns.value))
function row(index: number): DirectoryRow {
  return props.mode === 'timeline' ? timeline.value.rows[index]! : {
    start: index * columns.value, count: Math.min(columns.value, props.entries.length - index * columns.value),
  }
}
function rowEntries(index: number) {
  // 每次只切当前虚拟行，不展开或映射全目录。
  const item = row(index)
  return props.entries.slice(item.start, item.start + item.count)
}
function rowKey(index: number) {
  const item = row(index)
  return item.group ? `date:${item.group.label}` : props.entries[item.start]?.id ?? index
}
function estimateSize(index: number) {
  return props.mode === 'timeline' && row(index)?.group ? 36 : rowHeight.value
}
let resizeSequence = 0
async function resize(nextWidth: number) {
  if (nextWidth === width.value) return
  const sequence = ++resizeSequence
  const generation = props.generation
  const mode = props.mode
  const current = row(viewport.value?.firstVisibleIndex() ?? 0)
  const anchor = current && props.entries[current.start]?.id
  width.value = nextWidth
  await nextTick()
  if (sequence !== resizeSequence || generation !== props.generation || mode !== props.mode) return
  viewport.value?.measure()
  if (anchor !== undefined) {
    const index = props.indexOf(anchor)
    if (index !== undefined) viewport.value?.scrollToIndex(props.mode === 'timeline' ? timeline.value.rowOf(index) : Math.floor(index / columns.value))
  }
}
</script>

<style scoped>
.directory-row { display: grid; gap: 12px; height: 100%; padding: 4px 0 8px; box-sizing: border-box; }
.directory-row.list { padding: 0; gap: 0; }
.directory-cell { min-width: 0; overflow: hidden; }
.timeline-heading { font-weight: 600; line-height: 36px; padding-left: 6px; }
</style>
