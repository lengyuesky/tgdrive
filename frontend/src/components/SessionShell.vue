<template>
  <div class="session-shell with-tabs" :class="{ 'is-file-page': filePage, 'app-immersive': appImmersive }">
    <div v-if="!isOnline" class="network-banner" role="status">网络已断开，请检查连接。恢复后可重试未完成任务。</div>
    <router-view v-slot="{ Component }">
      <KeepAlive :include="['Files']" :max="1"><component :is="Component" /></KeepAlive>
    </router-view>
    <nav v-if="!appImmersive" class="mobile-tabs" aria-label="主导航">
      <router-link :to="lastFiles" :class="{ active: filePage }" :aria-current="filePage ? 'page' : undefined"><el-icon><Folder /></el-icon><span>文件</span></router-link>
      <router-link to="/transfers" :class="{ active: route.path === '/transfers' }"><span class="tab-icon"><el-icon><UploadFilled /></el-icon><b v-if="queue.activeCount.value" aria-label="活动上传数量">{{ queue.activeCount.value > 99 ? '99+' : queue.activeCount.value }}</b></span><span>传输</span></router-link>
      <router-link to="/apps" :class="{ active: route.path.startsWith('/apps') }"><el-icon><Grid /></el-icon><span>应用</span></router-link>
      <router-link to="/me" :class="{ active: route.path === '/me' || route.path === '/settings' }"><el-icon><User /></el-icon><span>我的</span></router-link>
    </nav>
    <input ref="fileInput" type="file" multiple hidden @change="intake.onPick" />
    <input ref="dirInput" type="file" webkitdirectory hidden @change="intake.onDirPick" />
    <UploadDrawer v-if="route.path !== '/transfers'" />
  </div>
</template>
<script setup lang="ts">
import { computed, onBeforeUnmount, provide, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { Folder, Grid, UploadFilled, User } from '@element-plus/icons-vue'
import { appImmersive } from '../apps/view-state'
import { createTransferController } from '../transfers/controller'
import { createUploadIntake } from '../transfers/intake'
import { TRANSFERS, UPLOAD_INTAKE } from '../transfers/context'
import { isOnline } from '../mobile/viewport'
import UploadDrawer from './UploadDrawer.vue'
const route = useRoute()
const queue = createTransferController()
const intake = createUploadIntake(queue)
const { fileInput, dirInput } = intake
provide(TRANSFERS, queue)
provide(UPLOAD_INTAKE, intake)
const lastFiles = ref('/files/')
const filePage = computed(() => route.path.startsWith('/files'))
watch(() => route.fullPath, (path) => {
  if (filePage.value) lastFiles.value = path
  queue.visible.value = false
}, { immediate: true })
watch(() => queue.visible.value, (open) => { if (route.path === '/transfers' && open) queue.visible.value = false })
onBeforeUnmount(() => document.documentElement.classList.remove('selection-active'))
</script>
