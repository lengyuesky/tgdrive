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
          <button role="tab" :aria-selected="tab === 'settings'" :class="{ active: tab === 'settings' }" @click="tab = 'settings'">
            应用设置 <span v-if="updateCount" class="update-count">{{ updateCount }}</span>
          </button>
        </div>
        <div class="tools-side">
          <el-input v-model="query" class="app-search" :prefix-icon="Search" placeholder="搜索应用" clearable />
          <el-button v-if="updateCount" type="primary" :icon="Download" class="batch-update" @click="openBatchUpdate">一键更新（{{ updateCount }}）</el-button>
        </div>
      </div>
      <div v-if="tab !== 'settings'" v-loading="loading" class="apps-grid">
        <article v-for="card in cards" :key="card.manifest.id" class="app-tile"
          :class="{ highlighted: route.query.app === card.manifest.id, off: !!card.installed && !card.installed.enabled }"
          :data-app-id="card.manifest.id">
          <button type="button" class="tile-launch" :aria-label="`${card.installed ? '打开' : '安装'}${card.manifest.name}`"
            :disabled="!!card.installed && !card.installed.enabled" @click="launch(card)" @pointerenter="hoverPrefetch(card)">
            <span class="tile-icon" aria-hidden="true"><AppIcon :manifest="card.manifest" :revision="card.installed?.revision" /></span>
            <span v-if="card.available?.update_available" class="tile-badge-dot" aria-hidden="true" />
            <span v-if="card.installed?.pinned" class="tile-badge-pin" aria-hidden="true"><el-icon><StarFilled /></el-icon></span>
          </button>
          <h2 class="tile-name" :title="card.manifest.description">{{ card.manifest.name }}</h2>
          <template v-if="!card.installed && card.available">
            <p class="tile-line">v{{ card.manifest.version }} · {{ card.manifest.author }}</p>
            <p class="tile-desc">{{ card.manifest.description }}</p>
            <p class="app-source">
              <span>{{ card.available.source === 'github' ? `GitHub (${data.catalog?.repository || 'lengyuesky/tgdrive-app'})` : '应用目录' }}</span>
              <span v-if="card.available.size" class="size-hint"> · {{ formatBytes(card.available.size) }}</span>
            </p>
            <div class="tile-actions">
              <el-button type="primary" size="small" :disabled="!!busy" @click="previewCatalog(card.available)">安装</el-button>
            </div>
            <div class="tile-links">
              <a :href="api.apps.packageUrl(card.manifest.id, card.available.digest)" class="package-link">下载插件包</a>
              <a v-if="card.available.source_url" :href="card.available.source_url" target="_blank" rel="noopener noreferrer" class="package-link">来源仓库</a>
            </div>
          </template>
        </article>
      </div>
      <div v-else v-loading="loading" class="manage-list">
        <article v-for="row in manageRows" :key="row.app.manifest.id" class="manage-row"
          :class="{ highlighted: route.query.app === row.app.manifest.id, off: !row.app.enabled }" :data-app-id="row.app.manifest.id">
          <span class="manage-icon" aria-hidden="true"><AppIcon :manifest="row.app.manifest" :revision="row.app.revision" /></span>
          <div class="manage-info">
            <div class="manage-title">
              <strong>{{ row.app.manifest.name }}</strong>
              <span class="app-version">v{{ row.app.manifest.version }}</span>
              <el-tag :type="row.app.enabled ? 'success' : 'info'" size="small">{{ row.app.enabled ? '已启用' : '已停用' }}</el-tag>
              <el-icon v-if="row.app.pinned" class="manage-pin" aria-label="已固定"><StarFilled /></el-icon>
            </div>
            <p class="manage-meta">
              <span>{{ row.app.source === 'import' ? '本地导入' : '应用目录' }}</span>
              <span v-if="row.update" class="update-hint">有新版本 v{{ row.update!.manifest.version }}</span>
            </p>
          </div>
          <div class="manage-actions">
            <el-button v-if="row.update" type="primary" size="small" :disabled="!!busy" @click="previewCatalog(row.update!)">更新</el-button>
            <el-button v-if="row.app.manifest.settings.length || row.app.manifest.permissions.includes('files.read')" size="small" :disabled="!!busy" @click="openSettings(row.app)">设置</el-button>
            <el-dropdown trigger="click" :disabled="!!busy" @command="command($event, row.app)">
              <el-button :icon="MoreFilled" size="small" :loading="busy === row.app.manifest.id" aria-label="更多应用操作" />
              <template #dropdown><el-dropdown-menu>
                <el-dropdown-item command="pin">{{ row.app.pinned ? '取消固定入口' : '固定到侧边栏' }}</el-dropdown-item>
                <el-dropdown-item command="enable">{{ row.app.enabled ? '停用应用' : '启用应用' }}</el-dropdown-item>
                <el-dropdown-item divided command="uninstall">卸载应用</el-dropdown-item>
              </el-dropdown-menu></template>
            </el-dropdown>
          </div>
        </article>
      </div>
      <el-empty v-if="!loading && !(tab === 'settings' ? manageRows.length : cards.length)" :description="emptyDescription">
        <el-button v-if="tab !== 'available' && !query" @click="tab = 'available'">浏览可安装应用</el-button>
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
          <p v-if="preview.manifest.permissions.includes('files.read')" class="scope-note">
            含「读取文件信息」的应用默认可访问整个网盘；安装后可在「应用设置 → 目录范围」收窄到指定目录。
          </p>
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
      <p>卸载「{{ uninstallTarget?.manifest.name }}」后，其运行页面将关闭，网盘文件和收藏继续保留，封面缓存一并删除。</p>
      <el-checkbox v-model="purgeData">同时清除这个应用的数据（设置、阅读记录和书签）</el-checkbox>
      <template #footer>
        <el-button :disabled="!!busy" @click="uninstallVisible = false">取消</el-button>
        <el-button type="danger" :loading="!!busy" @click="confirmUninstall">卸载</el-button>
      </template>
    </ResponsivePanel>
    <ResponsivePanel v-model="batchVisible" title="一键更新" width="min(560px, 94vw)" align-center
      :close-on-click-modal="!batchRunning" :close-on-press-escape="!batchRunning" :show-close="!batchRunning">
      <p class="batch-summary">将逐个下载并安装以下 {{ batchTargets.length }} 个应用的新版本；更新保留配置、启停状态与应用数据，失败不影响其余应用。</p>
      <ul class="batch-list">
        <li v-for="target in batchTargets" :key="target.app.manifest.id" class="batch-item">
          <span class="batch-icon" aria-hidden="true"><AppIcon :manifest="target.app.manifest" :revision="target.app.revision" /></span>
          <div class="batch-info">
            <div class="batch-title">
              <strong>{{ target.app.manifest.name }}</strong>
              <span class="batch-version">v{{ target.app.manifest.version }} → v{{ target.update.manifest.version }}</span>
            </div>
            <p v-if="target.added.length" class="batch-perms">
              新增权限：<el-tag v-for="permission in target.added" :key="permission" type="warning" size="small">{{ permissionLabels[permission] ?? permission }}</el-tag>
            </p>
            <p class="batch-status" :class="target.status">
              <template v-if="target.status === 'running'"><el-icon class="is-loading"><Loading /></el-icon>正在下载并安装…</template>
              <template v-else-if="target.status === 'done'">已更新</template>
              <template v-else-if="target.status === 'failed'">更新失败：{{ target.message }}</template>
              <template v-else>等待更新</template>
            </p>
          </div>
        </li>
      </ul>
      <template #footer>
        <el-button :disabled="batchRunning" @click="batchVisible = false">取消</el-button>
        <el-button type="primary" :loading="batchRunning" @click="onBatchAction">
          {{ batchRunning ? `正在更新 ${batchDone}/${batchTargets.length}` : batchFinished ? '完成' : `全部更新（${batchTargets.length}）` }}
        </el-button>
      </template>
    </ResponsivePanel>

    <AppSettingsDialog v-model="settingsVisible" :app="settingsApp" @scope-saved="load" />
  </main>
</template>

<script setup lang="ts">
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, Download, Loading, MoreFilled, Refresh, Search, StarFilled, Upload } from '@element-plus/icons-vue'
import { api, errMsg } from '@drive/api'
import { notifyAppsChanged, permissionLabels, type AppIndex, type AppManifest, type AppPreview, type CatalogApp, type InstalledApp } from '../types'
import { prefetchLaunch } from '../prefetch'
import AppIcon from '../components/AppIcon.vue'
import AppSettingsDialog from '../components/AppSettingsDialog.vue'

const router = useRouter()
const route = useRoute()
const data = ref<AppIndex>({ installed: [], available: [], api_version: 1, host_version: '' })
const loading = ref(true)
const refreshingCatalog = ref(false)
const error = ref('')
const tab = ref<'available' | 'installed' | 'settings'>('installed')
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
type ManageRow = { app: InstalledApp; update?: CatalogApp }
/** 应用设置标签：已安装应用的行式管理列表，同样支持搜索过滤。 */
const manageRows = computed<ManageRow[]>(() => {
  const q = query.value.trim().toLocaleLowerCase()
  return data.value.installed
    .map((app) => ({ app, update: data.value.available.find((item) => item.manifest.id === app.manifest.id && item.update_available) }))
    .filter(({ app }) => !q || `${app.manifest.name} ${app.manifest.description} ${app.manifest.id}`.toLocaleLowerCase().includes(q))
})
/** 待更新应用数，作为「应用设置」标签徽标；无更新时不显示。 */
const updateCount = computed(() => manageRows.value.filter((row) => row.update).length)
const cards = computed<Card[]>(() => {
  const all: Card[] = tab.value === 'installed'
    ? data.value.installed.map((app) => ({ manifest: app.manifest, installed: app, available: data.value.available.find((item) => item.manifest.id === app.manifest.id) }))
    : data.value.available.filter((app) => !data.value.installed.some((item) => item.manifest.id === app.manifest.id)).map((app) => ({ manifest: app.manifest, available: app }))
  const q = query.value.trim().toLocaleLowerCase()
  return all.filter(({ manifest }) => !q || `${manifest.name} ${manifest.description} ${manifest.id}`.toLocaleLowerCase().includes(q))
})

const emptyDescription = computed(() => {
  if (query.value.trim()) return '没有匹配的应用'
  if (tab.value !== 'available') return '还没有安装应用'
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
/** 桌面端悬停预取：仅对已启用应用发起；launch 无副作用，点击后由 AppHost 复用。 */
function hoverPrefetch(card: Card) {
  if (card.installed?.enabled) prefetchLaunch(card.manifest.id)
}
/** 手机桌面式交互：点击图标即打开应用（可安装则进入安装预览）。 */
function launch(card: Card) {
  if (card.installed) openApp(card.manifest.id)
  else if (card.available) previewCatalog(card.available)
}

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
/** 一键更新：单次确认后逐个安装目录包；每个应用独立原子提交，失败不影响其余。 */
type BatchStatus = 'pending' | 'running' | 'done' | 'failed'
interface BatchTarget {
  app: InstalledApp
  update: CatalogApp
  added: string[]
  status: BatchStatus
  message: string
}
const batchVisible = ref(false)
const batchRunning = ref(false)
const batchFinished = ref(false)
const batchDone = ref(0)
const batchTargets = ref<BatchTarget[]>([])

function openBatchUpdate() {
  const rows = manageRows.value.filter((row) => row.update)
  if (!rows.length || batchRunning.value) return
  const clone = <T,>(value: T): T => JSON.parse(JSON.stringify(value)) as T
  batchTargets.value = rows.map(({ app, update }) => ({
    app: clone(app),
    update: clone(update!),
    added: update!.manifest.permissions.filter((permission) => !app.manifest.permissions.includes(permission)),
    status: 'pending',
    message: '',
  }))
  batchFinished.value = false
  batchDone.value = 0
  batchVisible.value = true
}
function onBatchAction() {
  if (batchFinished.value) { batchVisible.value = false; return }
  void runBatchUpdate()
}
async function runBatchUpdate() {
  if (batchRunning.value) return
  batchRunning.value = true
  let failed = 0
  for (const target of batchTargets.value) {
    target.status = 'running'
    try {
      await api.apps.installCatalog(target.app.manifest.id, target.update.digest, target.app.revision)
      target.status = 'done'
      batchDone.value++
    } catch (e) {
      target.status = 'failed'
      target.message = errMsg(e)
      failed++
    }
  }
  batchRunning.value = false
  batchFinished.value = true
  notifyAppsChanged()
  await load()
  if (!failed) {
    ElMessage.success(`已更新 ${batchTargets.value.length} 个应用`)
    batchVisible.value = false
  } else {
    ElMessage.warning(`更新完成：成功 ${batchTargets.value.length - failed} 个，失败 ${failed} 个`)
  }
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
.apps-content { max-width: 1020px; margin: 0 auto; padding: 32px 24px; }
.apps-tools { display: flex; align-items: center; justify-content: space-between; gap: 20px; margin: 0 0 24px; }
.apps-tabs { display: flex; gap: 8px; }
.apps-tabs button { font: inherit; color: var(--el-text-color-secondary); background: transparent; padding: 11px 16px; border: 1px solid transparent; border-radius: 12px; cursor: pointer; white-space: nowrap; }
.apps-tabs button.active { color: var(--el-color-primary); background: var(--glass-bg-heavy); border-color: var(--glass-border); }
.apps-tabs span { font-size: 12px; margin-left: 5px; }
.apps-tabs .update-count { color: #ef4444; font-weight: 600; }
.app-search { max-width: 260px; }
.tools-side { display: flex; align-items: center; gap: 10px; min-width: 0; }
.batch-update { flex-shrink: 0; margin: 0; }
/* 手机桌面式图标网格 */
.apps-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(156px, 1fr)); gap: 16px; min-height: 90px; }
.app-tile { display: flex; flex-direction: column; align-items: center; text-align: center; padding: 18px 12px 16px; border: 1px solid transparent; border-radius: 20px; transition: background-color .2s, border-color .2s; }
.app-tile:hover { background: var(--glass-bg); border-color: var(--glass-border); }
.app-tile.highlighted { background: var(--glass-bg); border-color: var(--el-color-primary); box-shadow: 0 0 0 3px rgba(64, 158, 255, 0.18); }
.app-tile.off { opacity: 0.78; }
.app-tile.off .tile-icon { filter: grayscale(0.85); }
.tile-launch { position: relative; display: block; padding: 0; border: 0; background: transparent; line-height: 0; cursor: pointer; }
.tile-launch:disabled { cursor: default; }
.tile-launch:focus-visible { outline: 2px solid var(--el-color-primary); outline-offset: 4px; border-radius: 26px; }
.tile-icon { display: grid; place-items: center; width: 72px; height: 72px; border-radius: 22px; color: #fff; font-size: 32px; font-weight: 600; line-height: 1; text-transform: uppercase; box-shadow: 0 10px 22px rgba(15, 23, 42, 0.25), inset 0 1px 0 rgba(255, 255, 255, 0.35); transition: transform .2s ease; }
.app-tile:hover .tile-icon { transform: translateY(-2px) scale(1.04); }
.tile-badge-dot { position: absolute; top: -4px; right: -4px; width: 14px; height: 14px; border-radius: 50%; background: #ef4444; border: 2px solid #fff; animation: tile-badge-pulse 2s ease-out infinite; }
@keyframes tile-badge-pulse { 0% { box-shadow: 0 0 0 0 rgba(239, 68, 68, 0.45); } 70%, 100% { box-shadow: 0 0 0 8px rgba(239, 68, 68, 0); } }
.tile-badge-pin { position: absolute; top: -6px; left: -6px; width: 22px; height: 22px; border-radius: 50%; display: grid; place-items: center; color: #fff; font-size: 12px; background: #f59e0b; border: 2px solid #fff; box-shadow: 0 2px 6px rgba(245, 158, 11, 0.5); }
.tile-name { margin: 12px 0 0; font-size: 14px; line-height: 1.4; word-break: break-word; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.app-version { font-size: 12px; color: var(--el-text-color-secondary); word-break: break-word; }
.update-hint { margin: 6px 0 0; font-size: 12px; color: var(--el-color-primary); }
.tile-line { margin: 6px 0 0; font-size: 12px; color: var(--el-text-color-secondary); word-break: break-word; }
.tile-desc { margin: 6px 0 0; font-size: 12px; line-height: 1.5; color: var(--el-text-color-secondary); overflow-wrap: anywhere; display: -webkit-box; -webkit-line-clamp: 1; -webkit-box-orient: vertical; overflow: hidden; max-width: 100%; }
.app-source { margin: 6px 0 0; font-size: 11px; color: var(--el-text-color-placeholder); overflow-wrap: anywhere; }
.size-hint { font-size: 11px; color: var(--el-text-color-placeholder); }
.tile-actions { display: flex; align-items: center; justify-content: center; gap: 6px; flex-wrap: wrap; margin-top: 12px; }
.tile-actions .el-button { margin: 0; }
.tile-links { display: flex; align-items: center; justify-content: center; gap: 12px; margin-top: 8px; }
/* 应用设置：行式管理列表 */
.manage-list { display: flex; flex-direction: column; gap: 8px; min-height: 90px; }
.manage-row { display: flex; align-items: center; gap: 14px; padding: 14px 16px; border: 1px solid var(--glass-border); border-radius: 16px; transition: background-color .2s, border-color .2s; }
.manage-row:hover { background: var(--glass-bg); }
.manage-row.highlighted { border-color: var(--el-color-primary); box-shadow: 0 0 0 3px rgba(64, 158, 255, 0.18); }
.manage-row.off .manage-icon { filter: grayscale(0.85); }
.manage-icon { display: grid; place-items: center; flex-shrink: 0; width: 44px; height: 44px; border-radius: 13px; color: #fff; font-size: 20px; font-weight: 600; line-height: 1; text-transform: uppercase; box-shadow: 0 4px 10px rgba(15, 23, 42, 0.25), inset 0 1px 0 rgba(255, 255, 255, 0.35); }
.manage-info { flex: 1; min-width: 0; }
.manage-title { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
.manage-title strong { font-size: 14px; word-break: break-word; }
.manage-pin { color: #f59e0b; font-size: 14px; flex-shrink: 0; }
.manage-meta { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; margin: 4px 0 0; font-size: 12px; color: var(--el-text-color-secondary); }
.manage-meta .update-hint { margin: 0; }
.manage-actions { display: flex; align-items: center; gap: 6px; flex-shrink: 0; }
.manage-actions .el-button { margin: 0; }
/* 一键更新弹窗：逐个安装的实时状态清单 */
.batch-summary { margin: 0 0 14px; font-size: 13px; line-height: 1.7; color: var(--el-text-color-secondary); }
.batch-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; max-height: min(50vh, 420px); overflow: auto; }
.batch-item { display: flex; gap: 12px; padding: 12px; border: 1px solid var(--glass-border); border-radius: 12px; }
.batch-icon { flex-shrink: 0; display: block; width: 40px; height: 40px; border-radius: 12px; overflow: hidden; }
.batch-info { flex: 1; min-width: 0; }
.batch-title { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; font-size: 14px; }
.batch-title strong { word-break: break-word; }
.batch-version { font-size: 12px; color: var(--el-text-color-secondary); word-break: break-word; }
.batch-perms { display: flex; align-items: center; flex-wrap: wrap; gap: 4px; margin: 6px 0 0; font-size: 12px; color: var(--el-text-color-secondary); }
.batch-status { display: flex; align-items: center; gap: 6px; margin: 6px 0 0; font-size: 12px; color: var(--el-text-color-secondary); }
.batch-status.done { color: var(--el-color-success); }
.batch-status.failed { color: var(--el-color-danger); overflow-wrap: anywhere; }
.batch-status .el-icon { font-size: 14px; }
.package-link { color: var(--el-color-primary); text-decoration: none; font-size: 12px; }
.package-link:hover { text-decoration: underline; }
.apps-help { margin: 28px 0 0; text-align: center; color: var(--el-text-color-secondary); font-size: 12px; }
.install-name { margin: 0 0 14px; }
.install-name span { font-size: 13px; font-weight: normal; color: var(--el-text-color-secondary); }
.install-description { line-height: 1.7; overflow-wrap: anywhere; }
.permissions-box { background: var(--tile-bg); border: 1px solid var(--tile-border); border-radius: 12px; padding: 16px; margin: 18px 0; }
.permissions-box ul { padding-left: 20px; margin-bottom: 0; }
.permissions-box li { margin: 10px 0; line-height: 1.5; }
.scope-note { font-size: 12px; color: var(--el-text-color-secondary); margin: 10px 0 0; }
.install-meta, .install-digest { font-size: 12px; color: var(--el-text-color-secondary); }
.install-digest code { font-family: monospace; background: var(--tile-bg); padding: 2px 6px; border-radius: 4px; }
.el-alert { margin-bottom: 18px; }
@media (max-width: 767px) {
  .apps-page { min-height: 0; }
  .apps-header { gap: 10px; padding: 12px 16px; flex-wrap: wrap; }
  .apps-title { min-width: 0; }
  .apps-title h1 { font-size: 18px; white-space: nowrap; }
  .apps-title p { display: none; }
  .header-actions { width: 100%; display: flex; justify-content: flex-end; gap: 8px; }
  .apps-content { padding: 16px 12px; }
  .apps-tools { flex-wrap: wrap; gap: 12px; }
  .tools-side { width: 100%; }
  .apps-tabs { width: 100%; overflow: auto; gap: 4px; }
  .apps-tabs button { min-height: 44px; padding: 10px; }
  .app-search { max-width: none; }
  .apps-grid { grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 12px; }
  .app-tile { padding: 14px 8px 12px; }
  .tile-icon { width: 64px; height: 64px; border-radius: 19px; font-size: 28px; }
  .manage-list { gap: 6px; }
  .manage-row { flex-wrap: wrap; gap: 10px; padding: 12px; }
  .manage-actions { width: 100%; justify-content: flex-end; flex-wrap: wrap; }
}
@media (prefers-reduced-motion: reduce) {
  .tile-badge-dot { animation: none; }
  .tile-icon { transition: none; }
  .app-tile:hover .tile-icon { transform: none; }
}
</style>
