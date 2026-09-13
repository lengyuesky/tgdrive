<template>
  <aside class="sidebar glass" :class="{ 'in-drawer': drawer }" aria-label="目录侧边栏">
    <div class="side-head">
      <el-icon><FolderOpened /></el-icon>
      <span>目录</span>
      <div class="spacer" />
      <el-tooltip content="新建文件夹">
        <el-button text circle size="small" :icon="FolderAdd" @click="emit('mkdir')" />
      </el-tooltip>
      <el-tooltip content="刷新目录树">
        <el-button text circle size="small" :icon="Refresh" @click="emit('refresh')" />
      </el-tooltip>
    </div>
    <button class="side-root" :class="{ on: current === '/' }" @click="emit('navigate', '/')">
      <el-icon color="#f6a821"><Folder /></el-icon>
      <span>全部文件</span>
      <span class="spacer" />
      <span v-if="fileCount !== undefined" class="side-count">{{ fileCount }}</span>
    </button>
    <VirtualDirectoryTree :key="treeKey" :current="current" @select="emit('navigate', $event)" />
    <div class="side-sep" />
    <button class="side-root" :class="{ on: favoritesActive }" @click="emit('action', 'favorites')">
      <el-icon color="#f59e0b"><StarFilled /></el-icon>
      <span>我的收藏</span>
    </button>
    <button class="side-root" @click="emit('action', 'trash')">
      <el-icon color="#94a3b8"><DeleteFilled /></el-icon>
      <span>回收站</span>
    </button>
    <button class="side-root" @click="emit('action', 'shares')">
      <el-icon color="#0ea5e9"><Share /></el-icon>
      <span>分享管理</span>
    </button>
    <AppsSidebar :from="current" @navigate="emit('appNavigate')" />
  </aside>
</template>

<script setup lang="ts">
import { DeleteFilled, Folder, FolderAdd, FolderOpened, Refresh, Share, StarFilled } from '@element-plus/icons-vue'
import VirtualDirectoryTree from './VirtualDirectoryTree.vue'
import AppsSidebar from '@apps/components/AppsSidebar.vue'

defineProps<{ current: string; treeKey: number; fileCount?: number; favoritesActive: boolean; drawer?: boolean }>()
const emit = defineEmits<{
  navigate: [path: string]
  mkdir: []
  refresh: []
  action: [name: 'favorites' | 'trash' | 'shares']
  appNavigate: []
}>()
</script>

<style scoped>
.sidebar { width: 224px; flex-shrink: 0; border-radius: 16px; padding: 12px 10px; position: sticky; top: 76px; max-height: calc(100vh - 100px); overflow: auto; box-sizing: border-box; }
.side-head { display: flex; align-items: center; gap: 6px; font-weight: 700; font-size: 14px; padding: 0 6px 8px; border-bottom: 1px solid var(--glass-border); margin-bottom: 8px; }
.side-root { display: flex; align-items: center; gap: 6px; width: 100%; border: 0; background: transparent; color: inherit; font: inherit; padding: 7px 8px; border-radius: 8px; font-size: 14px; text-align: left; cursor: pointer; margin-bottom: 2px; }
.side-root:hover { background: var(--tile-bg-hover); }
.side-root:focus-visible { outline: 2px solid var(--el-color-primary); outline-offset: -2px; }
.side-root.on { color: var(--el-color-primary); background: rgba(64, 158, 255, 0.12); font-weight: 600; }
.side-count { font-size: 12px; color: var(--text-soft); background: var(--tile-bg); border-radius: 999px; padding: 1px 8px; }
.side-sep { height: 1px; background: var(--glass-border); margin: 8px 4px; }
.spacer { flex: 1; }
.sidebar.in-drawer { width: 100%; position: static; max-height: none; padding: 0; border: 0; border-radius: 0; background: transparent; box-shadow: none; backdrop-filter: none; }
@media (pointer: coarse), (max-width: 767px) { .side-root { min-height: 44px; } }
</style>
