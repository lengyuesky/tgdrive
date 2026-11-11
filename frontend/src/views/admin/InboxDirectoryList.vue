<template>
  <div class="inbox-directories">
    <div v-for="(directory, index) in route.directories" :key="index" class="inbox-directory">
      <div class="directory-main">
        <el-radio
          :model-value="automaticIndex"
          :value="index"
          @update:model-value="setAutomaticInboxDirectory(route, index)"
        >自动收件</el-radio>
        <el-input
          :model-value="directory.path"
          placeholder="/收件箱/目录"
          :aria-label="`收件目录 ${index + 1}`"
          @update:model-value="value => setInboxDirectoryPath(directory, value)"
        />
        <el-button @click="chooseInboxDirectory(directory)">选择</el-button>
      </div>
      <div class="directory-actions">
        <span v-if="directory.missing" class="directory-missing">目录已删除，请重新选择</span>
        <el-button link :disabled="index === 0" :aria-label="`上移目录 ${index + 1}`" @click="moveInboxDirectory(route, index, -1)">上移</el-button>
        <el-button link :disabled="index === route.directories.length - 1" :aria-label="`下移目录 ${index + 1}`" @click="moveInboxDirectory(route, index, 1)">下移</el-button>
        <el-button link type="danger" :disabled="route.directories.length <= 1" :aria-label="`删除目录 ${index + 1}`" @click="removeInboxDirectory(route, index)">删除</el-button>
      </div>
    </div>
    <div class="directory-footer">
      <el-button :disabled="route.directories.length >= 20" @click="addInboxDirectory(route)">添加目录</el-button>
      <span>{{ route.directories.length }}/20 · 频道使用自动收件目录，私聊手动选择</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { InboxTypeDirectories } from '../../api'
import { useAdminContext } from './useAdmin'

const props = defineProps<{ route: InboxTypeDirectories }>()
const automaticIndex = computed(() => props.route.directories.findIndex(directory => directory.automatic))
const {
  setAutomaticInboxDirectory, setInboxDirectoryPath, chooseInboxDirectory,
  moveInboxDirectory, removeInboxDirectory, addInboxDirectory,
} = useAdminContext()
</script>

<style scoped>
.inbox-directories { display: grid; gap: 12px; width: 100%; min-width: 0; }
.inbox-directory { padding: 10px 12px; border: 1px solid var(--el-border-color); border-radius: 10px; min-width: 0; }
.directory-main { display: flex; align-items: center; gap: 10px; min-width: 0; }
.directory-main .el-radio { margin-right: 0; flex: 0 0 auto; }
.directory-main .el-input { min-width: 0; flex: 1; }
.directory-actions { display: flex; align-items: center; justify-content: flex-end; gap: 8px; margin-top: 6px; }
.directory-missing { color: var(--el-color-danger); margin-right: auto; font-size: 12px; }
.directory-footer { display: flex; flex-wrap: wrap; align-items: center; gap: 10px; }
.directory-footer span { color: var(--el-text-color-secondary); font-size: 12px; }
@media (max-width: 600px) {
  .directory-main { flex-wrap: wrap; }
  .directory-main .el-radio { flex-basis: 100%; }
  .directory-main .el-input { flex-basis: calc(100% - 76px); }
  .directory-actions { flex-wrap: wrap; }
  .directory-missing { flex-basis: 100%; }
}
</style>
