<template>
  <main class="app-center">
    <header class="center-head">
      <router-link to="/files/" class="back-link">返回文件</router-link>
      <h1>应用中心</h1>
      <div class="head-actions">
        <el-button round :loading="loading" :disabled="busy" @click="refresh">刷新目录</el-button>
        <el-button round type="primary" :loading="updating" :disabled="busy || loading || !installedApps.length" @click="updateAll">一键更新{{ updates.length ? `（${updates.length}）` : '' }}</el-button>
        <el-button round :disabled="busy || loading" @click="input?.click()">上传应用包</el-button>
        <input ref="input" type="file" accept=".tgapp,application/zip" hidden @change="upload" />
      </div>
    </header>
    <el-alert v-if="error" :title="error" type="error" :closable="false" />
    <section v-if="updateResults.length" class="update-results glass" aria-live="polite"><h2>更新进度</h2><ul><li v-for="result in updateResults" :key="result.candidate.app.manifest.id" :class="result.status"><div><strong>{{ result.candidate.app.manifest.name }}</strong> {{ result.candidate.app.manifest.version }} → {{ result.candidate.entry.manifest.version }}<span>{{ updateLabel(result) }}</span></div><p v-if="result.note">{{ result.note }}</p><el-button v-if="result.status === 'confirmation'" :disabled="busy || loading" @click="reviewUpdate(result)">查看更新</el-button><el-button v-if="result.status === 'failed'" :disabled="busy || loading" @click="retryUpdate(result)">重试更新</el-button></li></ul></section>
    <section>
      <h2>已安装</h2>
      <p v-if="!installedApps.length" class="empty">尚未安装应用。可以从目录选择，也可以上传 .tgapp 包。</p>
      <!-- 图标点一下就打开；其余信息与操作收在图标下方的「更多」里，展开后占满所在行的下方。 -->
      <div class="launcher">
        <template v-for="app in installedApps" :key="app.manifest.id">
          <div class="app-tile" :class="{ off: !app.enabled, expanded: expanded === app.manifest.id }">
            <button type="button" class="tile-launch" :aria-label="canOpen(app) ? `打开 ${app.manifest.name}` : `${app.manifest.name}（${tileNote(app)}），查看设置`" :title="canOpen(app) ? `打开 ${app.manifest.name}` : tileNote(app)" @click="launch(app)">
              <span class="tile-icon">
                <img v-if="iconUrl(app)" :src="iconUrl(app)" alt="" crossorigin="anonymous" />
                <span v-else class="tile-letter" :style="{ background: tint(app.manifest.id) }" aria-hidden="true">{{ initial(app.manifest.name) }}</span>
                <i v-if="updatesById.has(app.manifest.id)" class="tile-badge" title="有可用更新"></i>
                <i v-if="app.manifest.backend && app.enabled" class="tile-state" :class="app.backend?.state ?? 'stopped'" :title="`应用后端${backendLabel(app)}`"></i>
              </span>
              <span class="tile-name">{{ app.manifest.name }}</span>
              <span v-if="tileNote(app)" class="tile-note">{{ tileNote(app) }}</span>
            </button>
            <button type="button" class="tile-more" :aria-label="`${app.manifest.name}：更多操作`" :aria-expanded="expanded === app.manifest.id" :aria-controls="`app-panel-${app.manifest.id}`" title="更多操作" @click="toggle(app)"><el-icon><MoreFilled /></el-icon></button>
          </div>
          <article v-if="expanded === app.manifest.id" :id="`app-panel-${app.manifest.id}`" class="app-panel glass">
            <header class="panel-head">
              <div><h3>{{ app.manifest.name }}</h3><span class="panel-meta">{{ app.manifest.version }} · {{ app.enabled ? '已启用' : '已停用' }}{{ app.pinned ? ' · 已固定' : '' }}</span></div>
              <button type="button" class="panel-close" aria-label="收起" title="收起" @click="expanded = ''"><el-icon><Close /></el-icon></button>
            </header>
            <p class="panel-desc">{{ app.manifest.description }}</p>
            <p v-if="updatesById.has(app.manifest.id)" class="update-available">可更新至 {{ updatesById.get(app.manifest.id)?.entry.manifest.version }} · 自动沿用 {{ app.package.target }}</p>
            <dl class="panel-facts">
              <dt>目录授权</dt><dd class="scope-summary">{{ scopeLabel(app) }}</dd>
              <template v-if="app.manifest.backend">
                <dt>应用后端</dt><dd class="backend-note">{{ app.manifest.backend.run_mode === 'resident' ? '常驻后端' : '按需后端' }} · {{ backendLabel(app) }}</dd>
                <template v-if="app.manifest.backend.external_service"><dt>对外服务</dt><dd class="backend-note">已声明{{ rootSelection?.app_id === app.manifest.id ? ' · 当前使用根地址' : '' }} · <a :href="`${SERVICE_BASE}/${encodeURIComponent(app.manifest.id)}/`" target="_blank" rel="noreferrer">查看固定前缀入口</a></dd></template>
              </template>
            </dl>
            <div class="card-actions">
              <el-button v-if="updatesById.has(app.manifest.id)" type="primary" :disabled="busy || loading" @click="updateOne(app)">更新</el-button>
              <el-button :disabled="busy" @click="authorize(app)">目录授权</el-button>
              <el-button :disabled="busy" @click="state(app, { enabled: !app.enabled })">{{ app.enabled ? '停用' : '启用' }}</el-button>
              <el-button :disabled="busy" @click="state(app, { pinned: !app.pinned })">{{ app.pinned ? '取消固定' : '固定' }}</el-button>
              <el-button v-if="app.manifest.backend" :disabled="!app.enabled || busy" @click="restart(app)">重启后端</el-button>
              <el-button v-if="app.manifest.backend" :disabled="busy" @click="showLogs(app)">后端日志</el-button>
              <el-button :disabled="busy" @click="showUsage(app)">占用</el-button>
              <el-button :disabled="busy" @click="clearCache(app)">清缓存</el-button>
              <el-button class="danger-action" type="danger" plain :disabled="busy" @click="uninstallApp = app; purge = false">卸载</el-button>
            </div>
          </article>
        </template>
      </div>
    </section>
    <section>
      <h2>可安装</h2>
      <p v-if="catalog?.status.state === 'disabled'" class="empty">联网目录未配置，可设置 TGDRIVE_APP_CATALOG_REPOSITORY；包上传仍可用。</p>
      <p v-if="catalog?.status.state === 'stale'" class="notice">目录读取失败，正在使用上次成功快照；已安装应用不受影响。</p>
      <p v-if="catalog?.status.state === 'ready' && !catalog.result.catalog.entries.length && !catalog.result.incompatible.length" class="empty">当前目录尚未发布应用，可以上传 .tgapp 应用包安装。</p>
      <ul v-if="available.length" class="catalog-list glass">
        <li v-for="entry in available" :key="`${entry.manifest.id}-${entry.manifest.version}`">
          <span class="tile-letter catalog-icon" :style="{ background: tint(entry.manifest.id) }" aria-hidden="true">{{ initial(entry.manifest.name) }}</span>
          <div class="catalog-text"><h3>{{ entry.manifest.name }} <small>{{ entry.manifest.version }}</small></h3><p>{{ entry.manifest.description }}</p><span class="scope-summary">{{ entry.manifest.files.write ? '声明文件读写' : entry.manifest.files.read ? '声明文件只读' : '不读取网盘文件' }}</span></div>
          <div class="catalog-actions"><el-select v-if="entry.manifest.backend" v-model="targets[entry.manifest.id]" :disabled="busy" aria-label="后端架构" placeholder="自动选择架构"><el-option v-if="entry.packages['linux-amd64']" label="Linux amd64" value="linux-amd64"/><el-option v-if="entry.packages['linux-arm64']" label="Linux arm64" value="linux-arm64"/></el-select><el-button round type="primary" :disabled="busy || loading" @click="previewCatalog(entry)">安装预览</el-button></div>
        </li>
      </ul>
      <p v-if="catalog?.status.state === 'unavailable'" class="empty">目录暂时不可用，可以稍后刷新或上传应用包。</p><details v-if="catalog?.result.incompatible.length"><summary>不兼容版本</summary><ul><li v-for="entry in catalog.result.incompatible" :key="`${entry.id}-${entry.version}`">{{ entry.id }} {{ entry.version }}：{{ entry.reason }}</li></ul></details>
    </section>
    <section>
      <h2>宿主设置</h2>
      <div class="host-settings">
        <div class="root-selection glass"><h3>根地址服务</h3><p>根地址只交给一个已声明对外服务的应用；应用自身仍负责客户端鉴权。未选择时访问根地址会返回宿主管理入口。</p><el-select :model-value="rootSelection?.app_id ?? ''" :disabled="busy" aria-label="根地址应用" @change="chooseRoot"><el-option label="不分配根地址" value=""/><el-option v-for="app in externalApps" :key="app.manifest.id" :label="`${app.manifest.name}${app.enabled ? '' : '（已停用）'}`" :value="app.manifest.id" :disabled="!app.enabled"/></el-select></div>
        <div class="resource-limits glass"><h3>应用资源限制</h3><p>全部应用后端合计共用这一组上限，不按应用分别设置，应用自己也不能声明。填 0 表示不限制；保存后立即生效，不需要重启应用。</p>
          <div class="limit-fields"><label>内存上限（MiB）<input v-model.number="limitForm.memory" type="number" min="0" step="64" :disabled="busy" aria-label="应用内存上限"/></label><label>CPU 上限（核）<input v-model.number="limitForm.cpu" type="number" min="0" step="0.1" :disabled="busy" aria-label="应用 CPU 上限"/></label><el-button type="primary" :disabled="busy || !limitsChanged" @click="saveLimits">保存限制</el-button></div>
          <p v-if="resourceLimits" class="limit-state">当前：内存 {{ limitLabel(resourceLimits.memory_bytes === '0' ? '' : formatBytes(resourceLimits.memory_bytes), resourceLimits.memory_enforced) }}；CPU {{ limitLabel(resourceLimits.cpu_millis ? `${resourceLimits.cpu_millis / 1000} 核` : '', resourceLimits.cpu_enforced) }}</p></div>
      </div>
    </section>
    <ResponsivePanel v-model="previewVisible" :title="previewUpdate ? '更新预览' : '安装预览'" width="600px" append-to-body>
      <template v-if="preview"><h2>{{ preview.manifest.name }} {{ preview.manifest.version }}</h2><p>{{ preview.manifest.description }}</p><dl class="detail-list"><dt>作者声明</dt><dd>{{ preview.manifest.author }}</dd><dt>应用包</dt><dd>{{ preview.package.target }} · {{ formatBytes(preview.package.size) }}</dd><dt>读取声明</dt><dd>{{ preview.manifest.files.read ? '允许请求读取授权' : '不读取文件' }}{{ preview.added_file_read ? '（本次新增）' : '' }}</dd><dt>写入声明</dt><dd>{{ preview.manifest.files.write ? '允许请求读写授权' : '不写入文件' }}{{ preview.added_file_write ? '（本次新增）' : '' }}</dd><dt>应用后端</dt><dd>{{ preview.manifest.backend ? `原生后端 · ${preview.manifest.backend.run_mode === 'resident' ? '常驻运行' : '首次请求按需启动'}` : '纯页面应用' }}</dd><dt>对外服务</dt><dd>{{ preview.manifest.backend?.external_service ? '声明根地址与固定前缀服务，仍由应用自行鉴权' : '未声明' }}</dd><dt>外网</dt><dd>{{ preview.external_network ? '后端可以使用自己的配置自由访问外网' : '页面不能直接访问外站网络' }}</dd></dl><p>安装不会自动授予网盘目录；更新保留键值与已有目录身份授权，撤掉写入声明时自动降为只读。</p></template>
      <template #footer><el-button :disabled="busy" @click="previewVisible = false">取消</el-button><el-button type="primary" :loading="busy" @click="install">{{ previewUpdate ? '确认更新' : '确认安装' }}</el-button></template>
    </ResponsivePanel>
    <ResponsivePanel :model-value="!!uninstallApp" title="卸载应用" width="500px" append-to-body @update:model-value="(open: boolean) => { if (!open) uninstallApp = undefined }"><p>卸载 {{ uninstallApp?.manifest.name }}，撤销所有页面访问；不会删除网盘文件。</p><el-checkbox v-model="purge">同时清除持久应用数据（默认保留）</el-checkbox><p v-if="purge" class="danger-note">将清除应用 KV 和固定数据库，重新安装不能找回。</p><template #footer><el-button @click="uninstallApp = undefined">取消</el-button><el-button type="danger" :loading="busy" @click="uninstall">确认卸载</el-button></template></ResponsivePanel>
    <ResponsivePanel v-model="usageVisible" title="应用占用" width="500px" append-to-body><dl v-if="occupancy" class="detail-list"><dt>键值记录</dt><dd>{{ occupancy.kv_entries }} / {{ occupancy.kv_entry_limit }}</dd><dt>键值字节</dt><dd>{{ formatBytes(occupancy.kv_bytes) }} / {{ formatBytes(occupancy.kv_byte_limit) }}</dd><dt>可再生缓存</dt><dd>{{ formatBytes(occupancy.cache_bytes) }} / {{ formatBytes(occupancy.cache_byte_limit) }}</dd><dt v-if="occupancy.database_bytes">后端数据库</dt><dd v-if="occupancy.database_bytes">{{ formatBytes(occupancy.database_bytes) }}</dd></dl></ResponsivePanel>
    <ResponsivePanel v-model="logsVisible" title="应用后端日志" width="800px" append-to-body><p>仅主人可查看。日志由应用生成，可能包含应用自己的敏感配置，请勿公开分享。</p><p v-if="logs?.truncated">仅显示最近的有界日志片段。</p><h3>标准输出</h3><pre class="backend-log">{{ logs?.stdout || '暂无输出' }}</pre><h3>标准错误</h3><pre class="backend-log">{{ logs?.stderr || '暂无输出' }}</pre></ResponsivePanel>
    <ScopeEditor ref="scope" />
  </main>
</template>
<script setup lang="ts">
import { computed, nextTick, onMounted, onBeforeUnmount, reactive, ref, shallowReactive, shallowRef } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Close, MoreFilled } from '@element-plus/icons-vue'
import ResponsivePanel from '../components/ResponsivePanel.vue'
import ScopeEditor from './ScopeEditor.vue'
import { center, installedApps, rootSelection, loadInstalled, iconUrl, formatBytes, usage, type BackendLogs } from './client'
import { SERVICE_BASE } from './contract'
import { LIMITS } from './contract'
import type { InstalledApp, CatalogResponse, CatalogEntry, InstallPreview, PackageTarget, DataUsage, AppStatePatch, AppResourceLimits } from './contract'
const router = useRouter(), input = ref<HTMLInputElement>(), scope = ref<InstanceType<typeof ScopeEditor>>()
const loading = ref(false), busy = ref(false), error = ref(''), catalog = ref<CatalogResponse>()
const targets = ref<Record<string, PackageTarget>>({}), preview = ref<InstallPreview>(), previewVisible = ref(false)
interface UpdateCandidate { app: InstalledApp; entry: CatalogEntry }
interface UpdateResult { candidate: UpdateCandidate; status: 'waiting' | 'checking' | 'updating' | 'updated' | 'confirmation' | 'failed'; note: string }
const updating = ref(false), updateResults = shallowRef<UpdateResult[]>([]), previewUpdate = shallowRef<UpdateCandidate>()
const uninstallApp = ref<InstalledApp>(), purge = ref(false), occupancy = ref<DataUsage>(), usageVisible = ref(false)
const logs = ref<BackendLogs>(), logsVisible = ref(false)
// 宿主唯一一组应用资源限制：表单用 MiB 与核，线上用字节与毫秒/秒；0 表示不限制。
const MiB = 1024 * 1024
const resourceLimits = shallowRef<AppResourceLimits>(), limitForm = reactive({ memory: 0, cpu: 0 })
const limitsChanged = computed(() => !!resourceLimits.value && (limitBytes() !== Number(resourceLimits.value.memory_bytes) || limitMillis() !== resourceLimits.value.cpu_millis))
function limitBytes() { return Math.round(Number(limitForm.memory) || 0) * MiB }
function limitMillis() { return Math.round((Number(limitForm.cpu) || 0) * 1000) }
function fillLimits(limits: AppResourceLimits) { resourceLimits.value = limits; limitForm.memory = Math.round(Number(limits.memory_bytes) / MiB); limitForm.cpu = limits.cpu_millis / 1000 }
function limitLabel(value: string, enforced: boolean) { return value ? `${value} · ${enforced ? '已强制' : '未强制（没有可写的 cgroup）'}` : '不限制' }
async function saveLimits() {
  await execute(async () => {
    // 空白或非数字不当作 0，避免误清空后把限制保存成不限制。
    if ([limitForm.memory, limitForm.cpu].some((value) => typeof value !== 'number' || !Number.isFinite(value) || value < 0)) throw new Error('请填写内存与 CPU 上限，0 表示不限制')
    const memory = limitBytes(), cpu = limitMillis()
    if (memory !== 0 && (memory < LIMITS.MIN_SHARED_BACKEND_MEMORY_BYTES || memory > LIMITS.MAX_SHARED_BACKEND_MEMORY_BYTES)) throw new Error('内存上限需为 0（不限制）或 64 MiB～1 TiB')
    if (cpu !== 0 && (cpu < LIMITS.MIN_SHARED_BACKEND_CPU_MILLIS || cpu > LIMITS.MAX_SHARED_BACKEND_CPU_MILLIS)) throw new Error('CPU 上限需为 0（不限制）或 0.1～256 核')
    fillLimits(await center.setLimits({ memory_bytes: String(memory), cpu_millis: cpu }))
  })
}
const externalApps = computed(() => installedApps.value.filter((app) => app.manifest.backend?.external_service || rootSelection.value?.app_id === app.manifest.id))
const statusTimer = window.setInterval(() => { if (!busy.value) void loadInstalled(true).catch(() => undefined) }, 5000)
onBeforeUnmount(() => window.clearInterval(statusTimer))
const latestEntries = computed(() => {
  const latest = new Map<string, CatalogEntry>()
  for (const entry of catalog.value?.result.catalog.entries ?? []) {
    const old = latest.get(entry.manifest.id)
    if (!old || compareVersion(entry.manifest.version, old.manifest.version) > 0) latest.set(entry.manifest.id, entry)
  }
  return [...latest.values()]
})
const available = computed(() => latestEntries.value.filter((entry) => !installedApps.value.some((app) => app.manifest.id === entry.manifest.id)))
const updates = computed<UpdateCandidate[]>(() => installedApps.value.flatMap((app) => {
  const entry = (catalog.value?.result.catalog.entries ?? [])
    .filter((entry) => entry.manifest.id === app.manifest.id && entry.packages[app.package.target] && compareVersion(entry.manifest.version, app.manifest.version) > 0)
    .sort((left, right) => compareVersion(right.manifest.version, left.manifest.version))[0]
  return entry ? [{ app, entry }] : []
}))
const updatesById = computed(() => new Map(updates.value.map((candidate) => [candidate.app.manifest.id, candidate])))
function compareVersion(a: string, b: string) { const left = a.split('.').map(BigInt), right = b.split('.').map(BigInt); for (let i = 0; i < 3; i++) { if (left[i] !== right[i]) return left[i] > right[i] ? 1 : -1 } return 0 }
function message(failure: unknown) { return failure instanceof Error ? failure.message : '应用操作失败' }
async function execute(operation: () => Promise<unknown>) { if (busy.value) return; busy.value = true; error.value = ''; try { await operation() } catch (failure) { error.value = message(failure) } finally { busy.value = false } }
async function refresh() { if (busy.value || loading.value) return; loading.value = true; error.value = ''; try { await loadInstalled(true); catalog.value = await center.refresh() } catch (failure) { error.value = message(failure) } finally { loading.value = false } }
onMounted(async () => { loading.value = true; try { await loadInstalled(true); fillLimits(await center.limits()); catalog.value = await center.catalog() } catch (failure) { error.value = message(failure) } finally { loading.value = false } })
async function upload(event: Event) { const file = (event.target as HTMLInputElement).files?.[0]; if (!file) return; await execute(async () => { const receipt = await center.upload(file); preview.value = await center.preview({ type: 'uploaded', package_id: receipt.package_id }); previewUpdate.value = undefined; previewVisible.value = true }); if (input.value) input.value.value = '' }
async function catalogPreview(entry: CatalogEntry, target: PackageTarget) {
  const artifact = entry.packages[target]
  if (!artifact) throw new Error('该目标的包不可用')
  return await center.preview({ type: 'catalog', app_id: entry.manifest.id, version: entry.manifest.version, target, sha256: artifact.sha256 })
}
async function previewCatalog(entry: CatalogEntry) { await execute(async () => { const target = entry.manifest.backend ? targets.value[entry.manifest.id] ?? (entry.packages['linux-amd64'] ? 'linux-amd64' : 'linux-arm64') : 'web'; preview.value = await catalogPreview(entry, target); previewUpdate.value = undefined; previewVisible.value = true }) }
async function install() {
  if (!preview.value) return
  const selected = preview.value, candidate = previewUpdate.value
  await execute(async () => {
    const result = candidate && updateResults.value.find((result) => result.candidate.app.manifest.id === candidate.app.manifest.id)
    if (result) { result.status = 'updating'; result.note = '' }
    try {
      const installed = await center.install({ preview_id: selected.preview_id })
      rememberInstalled(installed)
      if (result) result.status = 'updated'
      previewVisible.value = false; previewUpdate.value = undefined
    } catch (failure) {
      if (result) { result.status = 'failed'; result.note = message(failure) }
      throw failure
    }
    await loadInstalled(true)
  })
}
function rememberInstalled(app: InstalledApp) {
  installedApps.value = installedApps.value.some((current) => current.manifest.id === app.manifest.id)
    ? installedApps.value.map((current) => current.manifest.id === app.manifest.id ? app : current)
    : [...installedApps.value, app]
}
function updateLabel(result: UpdateResult) {
  return { waiting: '等待更新', checking: '正在检查应用包', updating: '正在更新', updated: '更新完成', confirmation: '需要确认新增权限', failed: '更新失败' }[result.status]
}
function addedPermissions(candidate: UpdateCandidate, selected: InstallPreview) {
  const added: string[] = []
  if (selected.added_file_read) added.push('文件读取')
  if (selected.added_file_write) added.push('文件写入')
  if (selected.starts_backend && !candidate.app.manifest.backend) added.push('应用后端与外网访问')
  if (selected.manifest.backend?.external_service && !candidate.app.manifest.backend?.external_service) added.push('对外服务')
  if (selected.manifest.backend?.run_mode === 'resident' && candidate.app.manifest.backend?.run_mode === 'on_demand') added.push('常驻运行')
  return added
}
async function updatePreview(candidate: UpdateCandidate) {
  const selected = await catalogPreview(candidate.entry, candidate.app.package.target)
  if (selected.previous_state_revision !== candidate.app.state_revision) throw new Error('应用状态已改变，请刷新后再更新')
  return selected
}
async function updateBatch(candidates: UpdateCandidate[], showConfirmation: boolean, preserveResults = false) {
  const results = candidates.map((candidate) => shallowReactive<UpdateResult>({ candidate, status: 'waiting', note: '' }))
  updateResults.value = preserveResults
    ? [...updateResults.value.filter((result) => !candidates.some((candidate) => candidate.app.manifest.id === result.candidate.app.manifest.id)), ...results]
    : results
  for (const result of results) {
    result.status = 'checking'
    try {
      const selected = await updatePreview(result.candidate)
      const added = addedPermissions(result.candidate, selected)
      if (added.length) {
        result.status = 'confirmation'; result.note = `本次新增：${added.join('、')}。查看更新后确认。`
        if (showConfirmation) { preview.value = selected; previewUpdate.value = result.candidate; previewVisible.value = true }
        continue
      }
      result.status = 'updating'
      rememberInstalled(await center.install({ preview_id: selected.preview_id }))
      result.status = 'updated'
    } catch (failure) { result.status = 'failed'; result.note = message(failure) }
  }
  await loadInstalled(true)
  const completed = results.filter((result) => result.status === 'updated').length
  const failed = results.filter((result) => result.status === 'failed').length
  const confirmation = results.filter((result) => result.status === 'confirmation').length
  if (failed) ElMessage.warning(`已更新 ${completed} 个应用，${failed} 个失败，可重试`)
  else if (confirmation) ElMessage.info(`已更新 ${completed} 个应用，${confirmation} 个需确认新增权限`)
  else ElMessage.success(`已更新 ${completed} 个应用`)
}
async function updateOne(app: InstalledApp) {
  if (loading.value) return
  await execute(async () => {
    const candidate = updatesById.value.get(app.manifest.id)
    if (!candidate) throw new Error('没有可用于当前架构的新版应用')
    updating.value = true
    try { await updateBatch([candidate], true) } finally { updating.value = false }
  })
}
async function updateAll() {
  if (loading.value) return
  await execute(async () => {
    updating.value = true
    try {
      await loadInstalled(true)
      catalog.value = await center.refresh()
      if (!['ready', 'stale'].includes(catalog.value.status.state)) throw new Error('应用目录暂时不可用，请稍后重试或上传应用包')
      const candidates = updates.value
      if (!candidates.length) { updateResults.value = []; ElMessage.info('当前架构的已安装应用均无可用更新'); return }
      await updateBatch(candidates, false)
    } finally { updating.value = false }
  })
}
async function reviewUpdate(result: UpdateResult) {
  await execute(async () => {
    preview.value = await updatePreview(result.candidate)
    previewUpdate.value = result.candidate; previewVisible.value = true
  })
}
async function retryUpdate(result: UpdateResult) {
  await execute(async () => {
    const candidate = updatesById.value.get(result.candidate.app.manifest.id)
    if (!candidate) throw new Error('应用状态已改变，请刷新后再更新')
    updating.value = true
    try { await updateBatch([candidate], true, true) } finally { updating.value = false }
  })
}
function open(app: InstalledApp) { void router.push(`/apps/${encodeURIComponent(app.manifest.id)}`) }
// 同一时间只展开一个应用的详情，避免图标网格被多块面板打散。
const expanded = ref('')
function canOpen(app: InstalledApp) { return app.enabled && !!app.manifest.page }
function tileNote(app: InstalledApp) { return !app.enabled ? '已停用' : !app.manifest.page ? '后台服务' : '' }
function reveal(id: string) { expanded.value = id; void nextTick(() => document.getElementById(`app-panel-${id}`)?.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' })) }
function toggle(app: InstalledApp) { if (expanded.value === app.manifest.id) expanded.value = ''; else reveal(app.manifest.id) }
// 打不开的应用（已停用，或只有后台服务没有页面）点图标时改为展开详情，说明原因并给出可用操作。
function launch(app: InstalledApp) { if (!canOpen(app)) reveal(app.manifest.id); else if (!busy.value) open(app) }
// 应用没有自带图标时用名称首字加一块由应用 ID 决定的底色，同一应用每次颜色一致。
function initial(name: string) { return [...name.trim()][0]?.toUpperCase() ?? '?' }
function tint(id: string) { let hue = 0; for (const character of id) hue = (hue * 31 + character.codePointAt(0)!) % 360; return `linear-gradient(140deg, hsl(${hue} 72% 64%), hsl(${(hue + 42) % 360} 66% 48%))` }
async function authorize(app: InstalledApp) { await execute(async () => { await scope.value?.open(app); await loadInstalled(true) }) }
async function state(app: InstalledApp, change: Pick<AppStatePatch, 'enabled' | 'pinned'>) { await execute(async () => { await center.state(app.manifest.id, { expected_state_revision: app.state_revision, ...change }); await loadInstalled(true) }) }
async function clearCache(app: InstalledApp) { await execute(async () => { await center.clearCache(app.manifest.id); ElMessage.success('可再生缓存已清空') }) }
async function showUsage(app: InstalledApp) { await execute(async () => { occupancy.value = await usage(app); usageVisible.value = true }) }
async function uninstall() { if (!uninstallApp.value) return; await execute(async () => { await center.uninstall(uninstallApp.value!, purge.value); uninstallApp.value = undefined; await loadInstalled(true) }) }
async function restart(app: InstalledApp) { await execute(async () => { await center.restart(app.manifest.id); await loadInstalled(true); ElMessage.success('应用后端已重启') }) }
async function showLogs(app: InstalledApp) { await execute(async () => { logs.value = await center.logs(app.manifest.id); logsVisible.value = true }) }
async function chooseRoot(id: string) { await execute(async () => { const current = rootSelection.value ?? await center.root(); rootSelection.value = await center.setRoot({ expected_revision: current.revision, app_id: id || null }); await loadInstalled(true) }) }
function backendLabel(app: InstalledApp) { const state = app.backend?.state; return state === 'ready' ? '已就绪' : state === 'starting' ? '正在启动' : state === 'failed' ? '启动失败，可查看日志并重启' : '已停止' }
function scopeLabel(app: InstalledApp) { const scope = app.scope.scope; return scope.mode === 'none' ? '尚未授予目录' : scope.mode === 'all' ? `全部目录 · ${scope.access === 'read_write' ? '读写' : '只读'}` : `指定 ${scope.directories.length} 个目录 · ${scope.directories.some((grant) => grant.access === 'read_write') ? '含读写' : '只读'}` }
</script>
<style scoped>
.app-center { max-width: 1120px; margin: auto; padding: 24px; }
.center-head { display: flex; flex-wrap: wrap; align-items: center; gap: 10px; margin-bottom: 20px; }
.center-head h1 { flex: 1; margin: 0; font-size: 24px; font-weight: 650; }
.head-actions { display: flex; flex-wrap: wrap; gap: 10px; }
.head-actions .el-button { margin: 0; }
.back-link { color: var(--el-text-color-secondary); text-decoration: none; }
section { margin: 32px 0; }
h2 { margin: 0 0 14px; font-size: 16px; font-weight: 600; color: var(--el-text-color-regular); }
h3 { margin: 0; font-size: 16px; }
h3 small { font-weight: 400; font-size: 12px; color: var(--el-text-color-secondary); }
.empty { color: var(--el-text-color-secondary); padding: 12px 0; }
.notice { color: var(--el-color-warning); }
.danger-note { color: var(--el-color-danger); }

/* dense 让展开的整行面板落到所在行下方，后面的图标仍补回原来的行。 */
.launcher { display: grid; grid-template-columns: repeat(auto-fill, minmax(112px, 1fr)); grid-auto-flow: row dense; gap: 12px 6px; }
.app-tile { display: flex; flex-direction: column; align-items: center; padding: 12px 4px 4px; border-radius: 20px; transition: background-color .18s; }
.app-tile.expanded { background: var(--tile-bg); }
.tile-launch { display: flex; flex: 1; flex-direction: column; align-items: center; gap: 8px; width: 100%; padding: 0; border: 0; background: none; color: inherit; font: inherit; cursor: pointer; -webkit-tap-highlight-color: transparent; }
.tile-icon { position: relative; flex: none; width: 72px; height: 72px; border-radius: 22.5%; background: var(--tile-media-bg); box-shadow: 0 6px 18px rgba(60, 70, 120, .18), 0 0 0 1px var(--tile-border); transition: transform .18s cubic-bezier(.2, .8, .3, 1.2), box-shadow .18s; }
.tile-icon img, .tile-letter { display: flex; width: 100%; height: 100%; border-radius: inherit; object-fit: cover; }
.tile-letter { align-items: center; justify-content: center; color: #fff; font-size: 30px; font-weight: 650; text-shadow: 0 1px 2px rgba(0, 0, 0, .18); user-select: none; }
.tile-launch:hover .tile-icon { transform: translateY(-2px) scale(1.04); box-shadow: 0 10px 24px rgba(60, 70, 120, .26), 0 0 0 1px var(--tile-border); }
.tile-launch:active .tile-icon { transform: scale(.94); }
.tile-launch:focus-visible { outline: none; }
.tile-launch:focus-visible .tile-icon { box-shadow: 0 0 0 3px var(--el-color-primary); }
.app-tile.off .tile-icon img, .app-tile.off .tile-letter { filter: grayscale(1); opacity: .55; }
.tile-badge, .tile-state { position: absolute; border: 2px solid var(--el-bg-color); border-radius: 50%; }
.tile-badge { top: -4px; right: -4px; width: 12px; height: 12px; background: var(--el-color-danger); }
.tile-state { right: -3px; bottom: -3px; width: 10px; height: 10px; background: var(--el-text-color-placeholder); }
.tile-state.ready { background: var(--el-color-success); }
.tile-state.starting { background: var(--el-color-warning); }
.tile-state.failed { background: var(--el-color-danger); }
.tile-name { max-width: 100%; padding: 0 2px; font-size: 13px; line-height: 1.35; text-align: center; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; overflow: hidden; overflow-wrap: anywhere; }
.tile-note { margin-top: -6px; font-size: 11px; color: var(--el-text-color-secondary); }
.tile-more { display: grid; place-items: center; flex: none; width: 44px; height: 26px; margin-top: 4px; padding: 0; border: 0; border-radius: 13px; background: none; color: var(--el-text-color-secondary); font-size: 18px; cursor: pointer; transition: background-color .15s, color .15s; -webkit-tap-highlight-color: transparent; }
.tile-more:hover, .tile-more[aria-expanded="true"] { background: var(--tile-bg-hover); color: var(--el-color-primary); }
.tile-more:focus-visible { outline: 2px solid var(--el-color-primary); outline-offset: 1px; }

.app-panel { grid-column: 1 / -1; padding: 18px 20px 20px; border-radius: 18px; animation: panel-in .2s ease-out; }
@keyframes panel-in { from { opacity: 0; transform: translateY(-6px); } }
.panel-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; }
.panel-meta { font-size: 12px; color: var(--el-text-color-secondary); }
.panel-close { display: grid; place-items: center; flex: none; width: 32px; height: 32px; margin: -6px -8px 0 0; padding: 0; border: 0; border-radius: 50%; background: none; color: var(--el-text-color-secondary); font-size: 16px; cursor: pointer; }
.panel-close:hover { background: var(--tile-bg-hover); }
.panel-desc { margin: 10px 0 0; line-height: 1.6; }
.update-available { margin: 8px 0 0; color: var(--el-color-primary); font-size: 13px; }
.panel-facts { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; margin: 14px 0 0; font-size: 13px; }
dt { color: var(--el-text-color-secondary); }
dd { margin: 0; overflow-wrap: anywhere; }
.scope-summary, .backend-note { color: var(--el-text-color-regular); font-size: 13px; }
.backend-note a { color: var(--el-color-primary); }
.card-actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 16px; }
.card-actions .el-button { margin: 0; }
.card-actions .danger-action { margin-left: auto; }

.catalog-list { list-style: none; margin: 0; padding: 4px 18px; border-radius: 18px; }
.catalog-list li { display: flex; align-items: center; gap: 14px; padding: 14px 0; }
.catalog-list li + li { border-top: 1px solid var(--el-border-color-lighter); }
.catalog-icon { flex: none; width: 48px; height: 48px; border-radius: 22.5%; font-size: 21px; }
.catalog-text { flex: 1; min-width: 0; }
.catalog-text p { margin: 4px 0; font-size: 13px; line-height: 1.5; color: var(--el-text-color-regular); }
.catalog-text .scope-summary { font-size: 12px; color: var(--el-text-color-secondary); }
.catalog-actions { display: flex; flex: none; align-items: center; gap: 8px; }
.catalog-actions .el-select { width: 140px; }
.catalog-actions .el-button { margin: 0; }

.host-settings { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 16px; align-items: start; }
.root-selection, .resource-limits { padding: 18px 20px; border-radius: 18px; }
.root-selection p, .resource-limits p { margin: 8px 0 14px; color: var(--el-text-color-secondary); font-size: 13px; line-height: 1.6; }
.root-selection .el-select { max-width: 360px; }
.limit-fields { display: flex; flex-wrap: wrap; align-items: flex-end; gap: 12px; }
.limit-fields label { display: grid; gap: 6px; font-size: 13px; color: var(--el-text-color-regular); }
.limit-fields input { width: 140px; height: 32px; padding: 0 11px; border: 1px solid var(--el-border-color); border-radius: var(--el-border-radius-base); background: var(--el-fill-color-blank); color: var(--el-text-color-regular); font: inherit; }
.limit-fields input:focus { outline: none; border-color: var(--el-color-primary); }
.resource-limits .limit-state { margin: 14px 0 0; }

.update-results { padding: 16px 20px; border-radius: 18px; }
.update-results ul { list-style: none; padding: 0; margin: 0; display: grid; gap: 14px; }
.update-results li div { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
.update-results li span { margin-left: auto; color: var(--el-text-color-secondary); font-size: 13px; }
.update-results li.updated span { color: var(--el-color-success); }
.update-results li.failed span, .update-results li.failed p { color: var(--el-color-danger); }
.update-results li.confirmation span { color: var(--el-color-warning); }
.update-results li p { margin: 6px 0; font-size: 13px; overflow-wrap: anywhere; }
.update-results .el-button { margin-top: 6px; }
.detail-list { display: grid; grid-template-columns: 110px 1fr; gap: 12px; }
.backend-log { white-space: pre-wrap; overflow-wrap: anywhere; max-height: 260px; overflow: auto; padding: 12px; background: var(--el-fill-color-light); border-radius: 8px; font-size: 12px; }

@media (max-width: 600px) {
  .app-center { padding: 16px; }
  .head-actions { flex-basis: 100%; }
  .launcher { grid-template-columns: repeat(auto-fill, minmax(76px, 1fr)); gap: 10px 2px; }
  .tile-icon { width: 60px; height: 60px; }
  .tile-letter { font-size: 25px; }
  .tile-name { font-size: 12px; }
  .app-panel { padding: 16px; }
  .card-actions .danger-action { margin-left: 0; }
  .catalog-list li { flex-wrap: wrap; }
  .catalog-actions { flex-basis: 100%; justify-content: flex-end; }
  .host-settings { grid-template-columns: 1fr; }
  .detail-list { grid-template-columns: 90px 1fr; }
}
@media (prefers-reduced-motion: reduce) {
  .tile-icon, .app-panel { transition: none; animation: none; }
}
</style>
