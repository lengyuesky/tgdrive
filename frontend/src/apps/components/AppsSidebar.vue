<template>
  <section class="apps-sidebar" aria-label="应用">
    <div class="apps-heading">应用</div>
    <button class="app-entry" aria-label="应用中心" @click="openCenter">
      <el-icon class="center-icon"><Menu /></el-icon>
      <span>应用中心</span>
    </button>
    <button v-for="app in pinned" :key="app.manifest.id" class="app-entry"
      :aria-label="app.manifest.name" :title="app.manifest.name" @click="openApp(app.manifest.id)" @pointerenter="prefetchLaunch(app.manifest.id)">
      <span class="app-icon" aria-hidden="true"><AppIcon :manifest="app.manifest" :revision="app.revision" /></span>
      <span class="app-name">{{ app.manifest.name }}</span>
    </button>
  </section>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { Menu } from '@element-plus/icons-vue'
import { api } from '@drive/api'
import { APPS_CHANGED, type InstalledApp } from '../types'
import { prefetchLaunch } from '../prefetch'
import AppIcon from './AppIcon.vue'

const props = defineProps<{ from: string }>()
const emit = defineEmits<{ navigate: [] }>()
const router = useRouter()
const pinned = ref<InstalledApp[]>([])
let alive = true
async function refresh() {
  try {
    const { data } = await api.apps.list()
    if (alive) pinned.value = (data.installed ?? []).filter((app) => app.enabled && app.pinned)
  } catch { /* 应用中心入口在网络故障时仍然可用。 */ }
}
function onStorage(event: StorageEvent) { if (event.key === APPS_CHANGED) void refresh() }
function openCenter() { emit('navigate'); router.push({ path: '/apps', query: { from: props.from } }) }
function openApp(id: string) { emit('navigate'); router.push({ path: `/apps/${id}`, query: { from: props.from } }) }
onMounted(() => {
  void refresh()
  window.addEventListener(APPS_CHANGED, refresh)
  window.addEventListener('storage', onStorage)
  window.addEventListener('focus', refresh)
})
onUnmounted(() => {
  alive = false
  window.removeEventListener(APPS_CHANGED, refresh)
  window.removeEventListener('storage', onStorage)
  window.removeEventListener('focus', refresh)
})
</script>

<style scoped>
.apps-sidebar { border-top: 1px solid var(--glass-border); margin-top: 10px; padding-top: 12px; }
.apps-heading { padding: 0 8px 7px; font-size: 12px; font-weight: 600; color: var(--text-soft); }
.app-entry { display: flex; align-items: center; gap: 8px; width: 100%; min-height: 38px; padding: 7px 8px; border: 0; border-radius: 8px; background: transparent; color: var(--el-text-color-primary); font: inherit; font-size: 14px; text-align: left; cursor: pointer; }
.app-entry:hover { background: var(--tile-bg-hover); }
.app-entry:focus-visible { outline: 2px solid var(--el-color-primary); outline-offset: -2px; }
.center-icon { width: 22px; color: var(--el-color-primary); font-size: 18px; flex-shrink: 0; }
.app-icon { display: block; flex-shrink: 0; width: 22px; height: 22px; border-radius: 7px; color: #fff; font-size: 12px; }
.app-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
@media (pointer: coarse), (max-width: 767px) { .app-entry { min-height: 44px; } }
</style>
