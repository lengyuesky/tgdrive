<template>
  <ResponsivePanel v-model="visible" :title="manifest?.name || '应用详情'" width="min(780px, 95vw)" align-center>
    <template v-if="manifest">
      <p>{{ manifest.description }}</p>
      <p>v{{ manifest.version }} · {{ manifest.author }} · 需要宿主 {{ manifest.min_host_version }} 或更高版本</p>
      <p v-if="installed">文件授权：{{ installed.scope_mode === 'none' ? '未授权' : installed.scope_mode === 'selected' ? installed.scope.join('、') : '全盘' }}</p>
      <template v-if="manifest.integration">
        <h3>支持格式</h3><p>{{ manifest.integration.formats.join('、') || '未声明' }}</p>
        <h3>使用说明</h3><ul><li v-for="line in manifest.integration.limitations" :key="line">{{ line }}</li></ul>
        <h3>更新说明</h3><p class="changelog">{{ manifest.integration.changelog || '未提供更新说明' }}</p>
        <div class="screenshots"><img v-for="(path, i) in manifest.integration.screenshots" :key="path" :src="screenshot(path, i)" :alt="`${manifest.name}界面 ${i + 1}`" loading="lazy" /></div>
        <div class="links"><a v-if="manifest.integration.homepage" :href="manifest.integration.homepage" target="_blank" rel="noopener noreferrer">项目主页</a><a v-if="manifest.integration.support" :href="manifest.integration.support" target="_blank" rel="noopener noreferrer">反馈问题</a></div>
      </template>
      <template v-if="installed">
        <h3>运行与恢复</h3>
        <p v-if="health?.lifecycle">最近状态：{{ phases[health.lifecycle.phase] || health.lifecycle.phase }} · 数据版本 {{ health.lifecycle.schema_version }}</p>
        <p v-if="health?.lifecycle?.message">{{ health.lifecycle.message }}</p>
        <p v-if="health?.recovery">可恢复到 v{{ health.recovery.version }} · {{ new Date(health.recovery.created_at * 1000).toLocaleString() }} · {{ fmtSize(health.recovery.bytes) }}</p>
        <p v-else>尚无更新前快照；下一次更新时自动保存。</p>
        <el-button v-if="health?.recovery" type="warning" :loading="recovering" @click="recover">恢复更新前版本与数据</el-button>
      </template>
      <el-alert v-if="error" :title="error" type="error" :closable="false" />
    </template>
    <template #footer><el-button @click="visible = false">关闭</el-button></template>
  </ResponsivePanel>
</template>
<script setup lang="ts">
import { ref } from 'vue'
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { appConfirm } from '@drive/dialog'
import { errMsg, fmtSize } from '@drive/api'
import { api } from '@drive/api'
const appApi = api.apps
import { notifyAppsChanged, type AppManifest, type InstalledApp, type CatalogApp, type AppHealth } from '../types'
const emit = defineEmits<{ changed: [] }>()
const visible = ref(false), manifest = ref<AppManifest>(), installed = ref<InstalledApp>(), available = ref<CatalogApp>()
const health = ref<AppHealth>(), error = ref(''), recovering = ref(false)
const phases: Record<string, string> = { installed: '等待打开', loading: '正在加载', ready: '已就绪', failed: '打开或迁移失败', migrating: '正在迁移数据', restored: '已恢复快照' }
let generation = 0
async function open(info: AppManifest, app?: InstalledApp, catalog?: CatalogApp) {
  const current = ++generation
  manifest.value = info; installed.value = app; available.value = catalog; health.value = undefined; error.value = ''; visible.value = true
  if (app) try { const { data } = await appApi.health(app.manifest.id); if (current === generation) health.value = data } catch (e) { if (current === generation) error.value = errMsg(e) }
}
function screenshot(path: string, index: number) {
  if (installed.value && installed.value.manifest.version === manifest.value?.version) return `/api/apps/runtime/${installed.value.manifest.id}/${installed.value.revision}/${path}`
  return `/api/apps/catalog/${manifest.value!.id}/screenshot/${index}?digest=${available.value?.digest ?? ''}`
}
async function recover() {
  const app = installed.value, snapshot = health.value?.recovery
  if (!app || !snapshot) return
  try { await appConfirm({ title: '恢复应用', message: `恢复到 v${snapshot.version}，并把设置、进度、书签和媒体库恢复至 ${new Date(snapshot.created_at * 1000).toLocaleString()}。该时间之后的应用数据将被覆盖；网盘文件和当前目录授权保留。`, confirmText: '恢复版本与数据', kind: 'warning' }) } catch { return }
  recovering.value = true; error.value = ''
  try { const { data } = await appApi.recover(app.manifest.id, app.revision); notifyAppsChanged(); emit('changed'); await open(data.manifest, data) }
  catch (e) { error.value = errMsg(e) }
  finally { recovering.value = false }
}
defineExpose({ open })
</script>
<style scoped>.changelog { white-space: pre-wrap; }.screenshots { display: flex; gap: 12px; overflow: auto; }.screenshots img { max-width: 100%; width: 560px; object-fit: contain; border-radius: 10px; }.links { display: flex; gap: 20px; margin: 16px 0; }p, li { overflow-wrap: anywhere; }</style>
