<template>
  <VirtualViewport class="directory-tree" :style="{ '--tree-row-height': rowHeight + 'px' }" :count="rows.length" :estimate-size="() => rowHeight"
    :item-key="(index) => rows[index]!.type + ':' + rows[index]!.path" :row-height="rowHeight">
    <template #default="{ index }">
      <div v-if="rows[index]!.type === 'dir'" class="tree-row" role="treeitem"
        :aria-selected="current === rows[index]!.path" :aria-expanded="(rows[index] as DirRow).expanded"
        :class="{ on: current === rows[index]!.path }" :style="{ paddingLeft: rows[index]!.depth * 16 + 'px' }">
        <button class="expand" :aria-label="(rows[index] as DirRow).expanded ? '收起目录' : '展开目录'"
          @click="tree.toggle(rows[index]!.path)">{{ (rows[index] as DirRow).expanded ? '▾' : '▸' }}</button>
        <button class="target" :title="rows[index]!.path" @click="$emit('select', rows[index]!.path)">
          <el-icon color="#f6a821"><Folder /></el-icon> {{ (rows[index] as DirRow).name }}
        </button>
      </div>
      <div v-else class="tree-row" :style="{ paddingLeft: rows[index]!.depth * 16 + 'px' }">
        <button class="more" :disabled="(rows[index] as MoreRow).loading" @click="tree.loadMore(rows[index]!.path)">
          {{ (rows[index] as MoreRow).loading ? '加载中…' : (rows[index] as MoreRow).error ? '加载失败，点击重试' : '加载更多目录' }}
        </button>
      </div>
    </template>
  </VirtualViewport>
</template>
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted } from 'vue'
import { touchUI } from '../mobile/viewport'
import { Folder } from '@element-plus/icons-vue'
import { api } from '../api'
import { createDirectoryTree, type TreeRow } from '../directory/tree'
import VirtualViewport from './VirtualViewport.vue'
defineProps<{ current: string }>()
defineEmits<{ select: [path: string] }>()
type DirRow = Extract<TreeRow, { type: 'dir' }>
type MoreRow = Extract<TreeRow, { type: 'more' }>
const tree = createDirectoryTree(async (params, signal) => (await api.entries(params, signal)).data)
const { rows } = tree
const rowHeight = computed(() => touchUI.value ? 44 : 34)
onMounted(tree.start)
onBeforeUnmount(tree.dispose)
</script>
<style scoped>
.directory-tree { height: 300px; min-height: 120px; }
.tree-row { display: flex; align-items: center; height: var(--tree-row-height); white-space: nowrap; box-sizing: border-box; }
.tree-row.on { background: var(--el-color-primary-light-9); color: var(--el-color-primary); }
button { height: 100%; border: 0; background: none; color: inherit; cursor: pointer; font: inherit; }
@media (pointer: coarse), (max-width: 767px) { .tree-row .expand { flex-basis: 44px; } }
.expand { flex: 0 0 24px; }
.target { overflow: hidden; text-overflow: ellipsis; text-align: left; flex: 1; }
.more { font-size: 12px; color: var(--el-color-primary); }
button:disabled { cursor: wait; opacity: .6; }
</style>
