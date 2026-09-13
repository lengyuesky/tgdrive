<template>
  <main class="apps-page">
    <header class="apps-header glass">
      <el-button :icon="ArrowLeft" @click="back">返回文件</el-button>
      <div class="apps-title"><h1>应用中心</h1><p>安装需要的应用，独立管理功能和设置。</p></div>
      <div class="header-actions">
        <el-button :icon="Refresh" :loading="loading || refreshingCatalog" @click="refreshAll">刷新</el-button>
        <el-button type="primary" :icon="Upload" :loading="inspecting" @click="fileInput?.click()">导入插件包</el-button>
      </div>
      <input ref="fileInput" type="file" accept=".tgapp,.zip" hidden @change="importPackage" />
    </header>

    <section class="apps-content">
      <el-alert v-if="error" :title="error" type="error" show-icon :closable="false" />
      <el-alert v-if="tab === 'available' && data.catalog?.state === 'stale'" type="warning" show-icon :closable="false"
        :title="`应用目录信息已过期（${data.catalog.error || '等待后台检查'}），当前显示缓存版本`" />
      <el-alert v-else-if="tab === 'available' && data.catalog?.state === 'unavailable'" type="error" show-icon :closable="false"
        :title="`无法获取远程应用目录（${data.catalog.error || '网络连接失败'}）`" />
      <el-alert v-else-if="tab === 'available' && data.catalog?.state === 'disabled'" type="info" show-icon :closable="false"
        title="应用目录已禁用，可通过上方「导入插件包」手动安装" />
      <div class="apps-tools">
        <div class="apps-tabs" role="tablist" aria-label="应用分类">
          <button role="tab" :aria-selected="tab === 'installed'" :class="{ active: tab === 'installed' }" @click="tab = 'installed'">
            已安装 <span>{{ data.installed.length }}</span>
          </button>
          <button role="tab" :aria-selected="tab === 'available'" :class="{ active: tab === 'available' }" @click="tab = 'available'">
            可安装 <span>{{ availableCount }}</span>
          </button>
        </div>
        <el-input v-model="query" class="app-search" :prefix-icon="Search" placeholder="搜索应用" clearable />
      </div>
      <div v-loading="loading" class="apps-grid">
        <article v-for="card in cards" :key="card.manifest.id" class="app-card glass"
          :class="{ highlighted: route.query.app === card.manifest.id }" :data-app-id="card.manifest.id">
          <div class="card-heading">
            <span class="app-mark">{{ card.manifest.name.slice(0, 1) }}</span>
            <div><h2>{{ card.manifest.name }}</h2><span class="app-version">v{{ card.manifest.version }} · {{ card.manifest.author }}</span></div>
            <el-tag v-if="card.installed" :type="card.installed.enabled ? 'success' : 'info'" size="small">
              {{ card.installed.enabled ? '已启用' : '已停用' }}
            </el-tag>
          </div>
          <p class="app-description">{{ card.manifest.description }}</p>
          <p class="app-source">
            <span>{{ card.installed ? (card.installed.source === 'import' ? '本地导入' : '应用目录') : (card.available?.source === 'github' ? `GitHub (${data.catalog?.repository || 'lengyuesky/tgdrive-app'})` : '应用目录') }}</span>
            <span v-if="card.available?.size" class="size-hint"> · {{ formatBytes(card.available.size) }}</span>
            <span v-if="card.available?.update_available" class="update-hint">有新版本 v{{ card.available.manifest.version }}</span>
          </p>
          <div v-if="card.installed" class="card-actions">
            <el-button type="primary" :disabled="!card.installed.enabled || !!busy" @click="openApp(card.manifest.id)">打开</el-button>
            <el-button v-if="card.manifest.settings.length" :disabled="!!busy" @click="openSettings(card.installed)">设置</el-button>
            <el-button v-if="card.available?.update_available" :disabled="!!busy" @click="previewCatalog(card.available)">更新</el-button>
            <el-dropdown trigger="click" :disabled="!!busy" @command="command($event, card.installed!)">
              <el-button :icon="MoreFilled" :loading="busy === card.manifest.id" aria-label="更多应用操作" />
              <template #dropdown><el-dropdown-menu>
                <el-dropdown-item command="pin">{{ card.installed.pinned ? '取消固定入口' : '固定到侧边栏' }}</el-dropdown-item>
                <el-dropdown-item command="enable">{{ card.installed.enabled ? '停用应用' : '启用应用' }}</el-dropdown-item>
                <el-dropdown-item divided command="uninstall">卸载应用</el-dropdown-item>
              </el-dropdown-menu></template>
            </el-dropdown>
            <span v-if="card.installed.pinned" class="pinned-label"><el-icon><StarFilled /></el-icon> 已固定</span>
          </div>
          <div v-else-if="card.available" class="card-actions">
            <el-button type="primary" :disabled="!!busy" @click="previewCatalog(card.available)">安装</el-button>
            <a :href="api.apps.packageUrl(card.manifest.id, card.available.digest)" class="package-link">下载插件包</a>
            <a v-if="card.available.release_url" :href="card.available.release_url" target="_blank" rel="noopener noreferrer" class="package-link">Release 页面</a>
          </div>
        </article>
      </div>
      <el-empty v-if="!loading && !cards.length" :description="emptyDescription">
        <el-button v-if="tab === 'installed' && !query" @click="tab = 'available'">浏览可安装应用</el-button>
        <el-button v-else-if="tab === 'available' && !query && data.catalog?.state === 'unavailable'" @click="refreshAll">重试连接</el-button>
      </el-empty>
      <p class="apps-help">支持导入 .tgapp 或 .zip 插件包；已有应用可通过导入新版本更新。</p>
    </section>

    <ResponsivePanel v-model="previewVisible" :title="preview?.existing ? '更新应用' : '安装应用'" width="min(560px, 94vw)"
      align-center :close-on-click-modal="!installing" :close-on-press-escape="!installing" :show-close="!installing">
      <template v-if="preview">
        <h3 class="install-name">{{ preview.manifest.name }} <span>v{{ preview.manifest.version }}</span></h3>
        <p class="install-description">{{ preview.manifest.description }}</p>
        <p v-if="preview.existing">当前版本 v{{ preview.existing.manifest.version }}，更新后保留配置和启停状态。</p>
        <div class="permissions-box">
          <strong>此应用需要以下权限</strong>
          <ul v-if="preview.manifest.permissions.length">
            <li v-for="permission in preview.manifest.permissions" :key="permission">
              {{ permissionLabels[permission] ?? permission }}
              <el-tag v-if="preview.existing && preview.added_permissions.includes(permission)" type="warning" size="small">新增</el-tag>
            </li>
          </ul>
          <p v-else>仅访问自身设置和应用数据，不读取网盘文件。</p>
        </div>
        <p class="install-meta">作者：{{ preview.manifest.author }} · 来源：{{ importFile ? '本地导入' : '应用目录' }}</p>
        <p class="install-digest">摘要：<code>{{ preview.digest.slice(0, 16) }}…</code></p>
        <p class="install-meta">SHA256 仅校验包内容完整性，作者为包内声明，不代表身份认证。</p>
        <el-alert v-if="installError" :title="installError" type="error" :closable="false" show-icon />
      </template>
      <template #footer>
        <el-button :disabled="installing" @click="previewVisible = false">取消</el-button>
        <el-button type="primary" :loading="installing" @click="confirmInstall">{{ preview?.existing ? '同意并更新' : '同意并安装' }}</el-button>
      </template>
    </ResponsivePanel>

    <ResponsivePanel v-model="uninstallVisible" title="卸载应用" width="min(460px, 94vw)" align-center :close-on-click-modal="!busy">
      <p>卸载「{{ uninstallTarget?.manifest.name }}」后，其运行页面将关闭，网盘文件和收藏继续保留。</p>
      <el-checkbox v-model="purgeData">同时清除这个应用的数据（设置、阅读记录和书签）</el-checkbox>
      <template #footer>
        <el-button :disabled="!!busy" @click="uninstallVisible = false">取消</el-button>
        <el-button type="danger" :loading="!!busy" @click="confirmUninstall">卸载</el-button>
      </template>
    </ResponsivePanel>
    <AppSettingsDialog v-model="settingsVisible" :app="settingsApp" />
  </main>
</template>

<script setup lang="ts">
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, MoreFilled, Refresh, Search, StarFilled, Upload } from '@element-plus/icons-vue'
import { api, errMsg } from '@drive/api'
import { notifyAppsChanged, permissionLabels, type AppIndex, type AppManifest, type AppPreview, type CatalogApp, type InstalledApp } from '../types'
import AppSettingsDialog from '../components/AppSettingsDialog.vue'

const router = useRouter()
const route = useRoute()
const data = ref<AppIndex>({ installed: [], available: [], api_version: 1, host_version: '' })
const loading = ref(true)
const refreshingCatalog = ref(false)
const error = ref('')
const tab = ref<'available' | 'installed'>('installed')
const query = ref('')
const busy = ref('')
const fileInput = ref<HTMLInputElement>()
const inspecting = ref(false)
const preview = ref<AppPreview | null>(null)
const importFile = ref<File>()
const previewVisible = ref(false)
const installing = ref(false)
const installError = ref('')
const settingsVisible = ref(false)
const settingsApp = ref<InstalledApp | null>(null)
const uninstallVisible = ref(false)
const uninstallTarget = ref<InstalledApp | null>(null)
const purgeData = ref(false)
const from = computed(() => typeof route.query.from === 'string' && route.query.from.startsWith('/') ? route.query.from : '/')
const availableCount = computed(() => data.value.available.filter((app) => !data.value.installed.some((item) => item.manifest.id === app.manifest.id)).length)

type Card = { manifest: AppManifest; installed?: InstalledApp; available?: CatalogApp }
const cards = computed<Card[]>(() => {
  const all: Card[] = tab.value === 'installed'
    ? data.value.installed.map((app) => ({ manifest: app.manifest, installed: app, available: data.value.available.find((item) => item.manifest.id === app.manifest.id) }))
    : data.value.available.filter((app) => !data.value.installed.some((item) => item.manifest.id === app.manifest.id)).map((app) => ({ manifest: app.manifest, available: app }))
  const q = query.value.trim().toLocaleLowerCase()
  return all.filter(({ manifest }) => !q || `${manifest.name} ${manifest.description} ${manifest.id}`.toLocaleLowerCase().includes(q))
})

const emptyDescription = computed(() => {
  if (query.value.trim()) return '没有匹配的应用'
  if (tab.value === 'installed') return '还没有安装应用'
  if (data.value.catalog?.state === 'disabled') return '应用目录已禁用'
  if (refreshingCatalog.value && !data.value.available.length) return '正在检查应用目录'
  if (data.value.catalog?.state === 'unavailable') return '应用目录暂不可用'
  if (!data.value.available.length) return '目录为空或没有兼容当前主程序的应用'
  return '当前目录中的应用都已安装'
})

function formatBytes(bytes?: number) {
  if (!bytes || bytes <= 0) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
}

async function load() {
  loading.value = true
  error.value = ''
  try { data.value = (await api.apps.list()).data }
  catch (e) { error.value = errMsg(e) }
  finally { loading.value = false }
}

async function refreshCatalog(force: boolean) {
  if (refreshingCatalog.value) return
  refreshingCatalog.value = true
  if (force) error.value = ''
  try { data.value = (await api.apps.refreshCatalog(force)).data }
  catch (e) {
    if (force) {
      try { data.value = (await api.apps.list()).data } catch { /* 保留已有安装列表。 */ }
      error.value = errMsg(e)
    }
  }
  finally { refreshingCatalog.value = false }
}
function refreshAll() { return refreshCatalog(true) }

onMounted(async () => {
  await load()
  // 每次打开都普通检查，后端负责缓存 TTL 和网络节流；已安装列表不等待联网。
  if (data.value.catalog?.source === 'github') await refreshCatalog(false)
})
function back() { router.push('/files' + from.value) }
function openApp(id: string) { router.push({ path: `/apps/${id}`, query: { from: from.value } }) }
function openSettings(app: InstalledApp) { settingsApp.value = app; settingsVisible.value = true }

function previewCatalog(app: CatalogApp) {
  const existing = data.value.installed.find((item) => item.manifest.id === app.manifest.id) ?? null
  preview.value = { manifest: app.manifest, digest: app.digest, existing,
    added_permissions: app.manifest.permissions.filter((permission) => !existing?.manifest.permissions.includes(permission)) }
  importFile.value = undefined
  installError.value = ''
  previewVisible.value = true
}
async function importPackage(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  if (!file || inspecting.value) return
  if (file.size > 16 * 1024 * 1024) { ElMessage.error('插件包不能超过 16 MiB'); return }
  inspecting.value = true
  try {
    preview.value = (await api.apps.inspect(file)).data
    importFile.value = file
    installError.value = ''
    previewVisible.value = true
  } catch (e) { ElMessage.error(errMsg(e)) }
  finally { inspecting.value = false }
}
async function confirmInstall() {
  if (!preview.value || installing.value) return
  installing.value = true
  installError.value = ''
  const info = preview.value
  try {
    if (importFile.value) await api.apps.install(importFile.value, info.digest, info.existing?.revision)
    else await api.apps.installCatalog(info.manifest.id, info.digest, info.existing?.revision)
    previewVisible.value = false
    tab.value = 'installed'
    notifyAppsChanged()
    ElMessage.success(info.existing ? '应用已更新' : '应用已安装')
    await load()
  } catch (e) { installError.value = errMsg(e) }
  finally { installing.value = false }
}
async function command(action: string, app: InstalledApp) {
  if (action === 'uninstall') {
    uninstallTarget.value = app; purgeData.value = false; uninstallVisible.value = true
    return
  }
  if (busy.value) return
  busy.value = app.manifest.id
  try {
    await api.apps.state(app.manifest.id, action === 'pin' ? { pinned: !app.pinned } : { enabled: !app.enabled })
    notifyAppsChanged()
    await load()
  } catch (e) { ElMessage.error(errMsg(e)) }
  finally { busy.value = '' }
}
async function confirmUninstall() {
  if (!uninstallTarget.value || busy.value) return
  busy.value = uninstallTarget.value.manifest.id
  try {
    await api.apps.uninstall(busy.value, purgeData.value)
    uninstallVisible.value = false
    notifyAppsChanged()
    ElMessage.success('应用已卸载')
    await load()
  } catch (e) { ElMessage.error(errMsg(e)) }
  finally { busy.value = '' }
}
</script>

<style scoped>
.apps-page { min-height: 100vh; color: var(--el-text-color-primary); }
.apps-header { display: flex; align-items: center; gap: 22px; padding: 20px 32px; border-width: 0 0 1px; }
.apps-title { flex: 1; }
.apps-title h1 { margin: 0; font-size: 23px; }
.apps-title p { margin: 6px 0 0; color: var(--el-text-color-secondary); font-size: 13px; }
.header-actions { display: flex; gap: 8px; }
.header-actions .el-button { margin: 0; }
.apps-content { max-width: 1160px; margin: 0 auto; padding: 32px 24px; }
.apps-tools { display: flex; align-items: center; justify-content: space-between; gap: 20px; margin: 0 0 24px; }
.apps-tabs { display: flex; gap: 8px; }
.apps-tabs button { font: inherit; color: var(--el-text-color-secondary); background: transparent; padding: 11px 16px; border: 1px solid transparent; border-radius: 12px; cursor: pointer; white-space: nowrap; }
.apps-tabs button.active { color: var(--el-color-primary); background: var(--glass-bg-heavy); border-color: var(--glass-border); }
.apps-tabs span { font-size: 12px; margin-left: 5px; }
.app-search { max-width: 260px; }
.apps-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(310px, 1fr)); gap: 20px; min-height: 90px; }
.app-card { border-radius: 20px; padding: 24px; display: flex; flex-direction: column; }
.app-card.highlighted { border-color: var(--el-color-primary); }
.card-heading { display: flex; align-items: center; gap: 12px; }
.card-heading > div { flex: 1; min-width: 0; }
.app-mark { background: linear-gradient(135deg, #6366f1, #0ea5e9); color: white; display: grid; place-items: center; width: 48px; height: 48px; border-radius: 15px; font-size: 23px; flex-shrink: 0; }
.card-heading h2 { margin: 0 0 5px; font-size: 18px; word-break: break-word; }
.app-version, .app-source { font-size: 12px; color: var(--el-text-color-secondary); word-break: break-word; }
.app-description { font-size: 14px; line-height: 1.8; margin: 20px 0 8px; flex: 1; overflow-wrap: anywhere; }
.app-source { margin: 8px 0 20px; }
.update-hint { display: block; margin-top: 8px; color: var(--el-color-primary); }
.card-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.card-actions .el-button { margin: 0; }
.pinned-label { display: flex; align-items: center; gap: 4px; font-size: 12px; color: var(--text-soft); }
.package-link { color: var(--el-color-primary); text-decoration: none; font-size: 13px; padding: 8px; }
.apps-help { margin: 28px 0 0; text-align: center; color: var(--el-text-color-secondary); font-size: 12px; }
.install-name { margin: 0 0 14px; }
.install-name span { font-size: 13px; font-weight: normal; color: var(--el-text-color-secondary); }
.install-description { line-height: 1.7; overflow-wrap: anywhere; }
.permissions-box { background: var(--tile-bg); border: 1px solid var(--tile-border); border-radius: 12px; padding: 16px; margin: 18px 0; }
.permissions-box ul { padding-left: 20px; margin-bottom: 0; }
.permissions-box li { margin: 10px 0; line-height: 1.5; }
.install-meta, .install-digest { font-size: 12px; color: var(--el-text-color-secondary); }
.install-digest code { font-family: monospace; background: var(--tile-bg); padding: 2px 6px; border-radius: 4px; }
.size-hint { font-size: 11px; color: var(--el-text-color-placeholder); }
.el-alert { margin-bottom: 18px; }
@media (max-width: 650px) {
  .apps-header { gap: 14px; padding: 16px; flex-wrap: wrap; }
  .apps-title p { display: none; }
  .header-actions { width: 100%; justify-content: flex-end; }
  .apps-content { padding: 22px 14px; }
  .apps-tools { flex-wrap: wrap; gap: 14px; }
  .app-search { max-width: none; }
  .apps-grid { grid-template-columns: minmax(0, 1fr); }
}
@media (max-width: 767px) {
  .apps-page { min-height: 0; }.apps-header { gap: 8px; padding: 12px; flex-wrap: wrap; }.apps-title { min-width: 0; }.header-actions { flex-wrap: wrap; }
  .apps-content { padding: 16px 12px; }.apps-tools { flex-wrap: wrap; gap: 12px; }.apps-tabs { width: 100%; overflow: auto; gap: 4px; }.apps-tabs button { min-height: 44px; padding: 10px; }
  .apps-grid { grid-template-columns: minmax(0, 1fr); gap: 14px; }.app-card { padding: 18px; }.app-search { max-width: none; }
}
</style>
