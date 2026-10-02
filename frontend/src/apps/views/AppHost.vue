<template>
  <main class="app-host" :class="{ 'is-immersive': immersive }" :style="{ backgroundColor: immersive ? immersiveBackground : undefined }">
    <header v-show="!immersive" class="host-toolbar glass">
      <el-button text :icon="ArrowLeft" @click="back">返回文件</el-button>
      <div class="host-name">
        <span v-if="app" class="host-icon" aria-hidden="true"><AppIcon :manifest="app.manifest" :revision="app.revision" /></span>
        <span class="host-label">{{ pageTitle || app?.manifest.name || '应用' }} <span v-if="app">v{{ app.manifest.version }}</span></span>
      </div>
      <div class="host-controls">
        <el-button v-if="app" :disabled="!!error || loading" text :icon="Setting" @click="settingsVisible = true">应用设置</el-button>
        <el-button text :icon="Menu" @click="center">应用中心</el-button>
      </div>
    </header>
    <div v-show="!immersive" v-for="task in Array.from(tasks.values())" :key="task.id" class="host-task" role="status">
      <span>{{ task.title }} · {{ task.completed }}{{ task.total == null ? '' : ' / ' + task.total }} · {{ task.state === 'failed' ? '失败，可在应用内重试' : task.state === 'cancelled' ? '已取消' : task.state === 'done' ? '已完成' : '进行中' }}</span>
      <el-button v-if="task.state === 'running'" text @click="cancelTask(task.id)">取消任务</el-button>
      <el-button v-else text @click="tasks.delete(task.id)">关闭</el-button>
    </div>
    <section class="app-stage">
      <div v-if="needsAuthorization" class="host-status">
        <h2>选择{{ app?.manifest.name }}可以读取的目录</h2>
        <p>确认授权后开始使用。可随时在应用设置中修改。</p>
        <el-alert v-if="error" :title="error" type="error" :closable="false" />
        <el-button type="primary" :loading="authorizing" @click="authorizeInitial">选择目录并授权</el-button>
        <el-button text @click="center">稍后设置</el-button>
      </div>
      <iframe v-if="launch" ref="frame" :key="launch.app.revision" :src="launch.frame_url" :title="launch.app.manifest.name"
        sandbox="allow-scripts" allow="autoplay; fullscreen; picture-in-picture" referrerpolicy="no-referrer" />
      <div v-if="!needsAuthorization && (loading || (launch && !connected))" class="host-status">
        <el-icon class="is-loading" :size="28"><Loading /></el-icon><p>正在打开应用…</p>
      </div>
      <div v-else-if="!needsAuthorization && error" class="host-status">
        <el-empty :description="error" />
        <div><el-button @click="start">重新打开</el-button><el-button type="primary" @click="center">前往应用中心</el-button></div>
      </div>
    </section>
    <AppSettingsDialog v-model="settingsVisible" :app="app" @saved="settingsSaved" @scope-saved="onScopeSaved" />
    <ResponsivePanel v-model="fileDetailsVisible" title="文件信息" width="500px" align-center>
      <template v-if="detailsFile"><p>{{ detailsFile.name }}</p><p class="file-path">{{ detailsFile.path }}</p><p>{{ detailsFile.is_dir ? '文件夹' : fmtSize(detailsFile.size) }}</p><p>修改时间：{{ new Date(detailsFile.modified_at * 1000).toLocaleString() }}</p></template>
      <template #footer><el-button @click="fileDetailsVisible = false">关闭</el-button></template>
    </ResponsivePanel>
    <DirPicker ref="picker" />
  </main>
</template>

<script setup lang="ts">
import { reactive, computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter, onBeforeRouteLeave, onBeforeRouteUpdate } from 'vue-router'
import { ArrowLeft, Loading, Menu, Setting } from '@element-plus/icons-vue'
import { api, errMsg, fmtSize } from '@drive/api'
import { connectAppBridge, type AppBridge } from '../bridge'
import { afterHostPaint } from '../paint'
import { HostImmersion } from '../immersive'
import { takeLaunch } from '../prefetch'
import { assetPath, boundedBytes, mapLimit, rangeInput, rangesInput, RANGE_LIMIT, RANGE_PARALLEL } from '../bytes'
import { batchInput } from '../batch'
import { APPS_CHANGED, filesChangeAffectsScope, pathInScope, type AppLaunch, type InstalledApp, type SettingValues } from '../types'
import { onHostEvent } from '../events'
import { isDark } from '@drive/theme'
import { closeTopOverlay } from '@drive/mobile/overlays'
import AppSettingsDialog from '../components/AppSettingsDialog.vue'
import AppIcon from '../components/AppIcon.vue'
import DirPicker from '@drive/components/DirPicker.vue'
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { appConfirm } from '@drive/dialog'
import { parentPath, safeDirectory } from '../integration'
import type { AppFile } from '../types'

const route = useRoute()
const router = useRouter()
const frame = ref<HTMLIFrameElement>()
const picker = ref<InstanceType<typeof DirPicker>>()
const launch = ref<AppLaunch | null>(null)
const app = ref<InstalledApp | null>(null)
const loading = ref(true)
const connected = ref(false)
const error = ref('')
const settingsVisible = ref(false)
const immersive = ref(false)
const immersiveBackground = ref<string>()
const immersion = new HostImmersion((active, background) => { immersive.value = active; immersiveBackground.value = background })
const from = computed(() => safeDirectory(route.query.from))
const pageTitle = ref(''), needsAuthorization = ref(false), authorizing = ref(false)
const detailsFile = ref<AppFile>(), fileDetailsVisible = ref(false), contextFile = ref<AppFile>()
let exitMessage = ''
interface Task { id: string; title: string; completed: number; total?: number; state: string }
const tasks = reactive(new Map<string, Task>())
function cancelTask(id: string) { port?.postMessage({ type: 'event', name: 'task.cancel', value: { id } }) }
function reportHealth(phase: string, message = '') {
  const active = app.value
  if (active) void api.apps.report(active.manifest.id, active.revision, phase, message.slice(0, 350)).catch(() => {})
}
async function authorizeDirectory(initial: string): Promise<string | null> {
  if (picking) throw new Error('目录选择窗口已经打开')
  picking = true
  const current = generation, active = app.value
  try {
    if (!active) throw new Error('应用已经关闭')
    const path = await picker.value?.open('选择允许应用读取的目录', initial)
    if (path == null || current !== generation) return null
    await appConfirm({ title: '授权目录', message: '允许「' + active.manifest.name + '」读取 ' + path + ' 及其子目录？' + (path === '/' ? '根目录包含整个网盘。' : ''), confirmText: '允许读取' })
    if (current !== generation) return null
    app.value = (await api.apps.authorize(active.manifest.id, path, active.revision)).data
    return path
  } finally { picking = false }
}
async function authorizeInitial() {
  authorizing.value = true; error.value = ''
  try {
    const path = await authorizeDirectory(parentPath(safeDirectory(route.query.open)))
    if (path != null) {
      await start()
    }
  } catch (e) { if (e !== 'cancel' && e !== 'close') error.value = errMsg(e) }
  finally { authorizing.value = false }
}
let generation = 0
let controller = new AbortController()
let port: MessagePort | undefined
let disconnect: AppBridge | undefined
let renewing: Promise<void> | undefined
let filesCursor: string | undefined, replaying = false, replayAgain = false
const tickets = new Map<string, string>()
let reads = 0
let timeout: ReturnType<typeof setTimeout> | undefined
let poll: ReturnType<typeof setInterval> | undefined
let picking = false
let offHostEvents: (() => void) | undefined

function rpcCode(error: unknown): string | undefined {
  return (error as { response?: { data?: { code?: string } } }).response?.data?.code
}

function dispose() {
  generation++
  tasks.clear(); pageTitle.value = ''; exitMessage = ''; contextFile.value = undefined
  immersion.reset()
  tickets.clear()
  picker.value?.close()
  controller.abort()
  disconnect?.()
  disconnect = undefined
  renewing = undefined
  port = undefined
  clearTimeout(timeout)
  clearInterval(poll)
  connected.value = false
  launch.value = null
}
function fail(message: string) {
  reportHealth('failed', message)
  dispose()
  loading.value = false
  error.value = message
  settingsVisible.value = false
}
async function start() {
  dispose()
  const current = generation
  controller = new AbortController()
  loading.value = true
  error.value = ''
  needsAuthorization.value = false
  app.value = null
  try {
    // 优先复用 60 秒内悬停预取的 launch 响应，省一次往返；过期则正常请求。
    const data = takeLaunch(String(route.params.id))
      ?? (await api.apps.launch(String(route.params.id), controller.signal)).data
    if (current !== generation) return
    app.value = data.app
    if (data.app.scope_mode === 'none' && data.app.manifest.permissions.includes('files.read')) { needsAuthorization.value = true; return }
    if (typeof route.query.open === 'string') {
      const id = safeDirectory(route.query.open)
      try { contextFile.value = (await api.apps.open(data.app.manifest.id, id)).data }
      catch (e) {
        if ((e as { response?: { status?: number } }).response?.status === 403) { needsAuthorization.value = true; return }
        throw e
      }
      if (current !== generation) return
    }
    filesCursor = data.files_cursor
    reportHealth('loading')
    launch.value = data
    timeout = setTimeout(() => { if (current === generation && !connected.value) fail('应用连接超时，请重新打开或更新插件包') }, 12_000)
    poll = setInterval(checkState, 15_000)
  } catch (e) {
    if (current !== generation) return
    const status = (e as { response?: { status: number } }).response?.status
    error.value = status === 404 ? '此应用尚未安装，请前往应用中心安装' : errMsg(e)
    reportHealth('failed', error.value)
  } finally { if (current === generation) loading.value = false }
}
/** 临期/过期续签：修订未变时原位热换会话（iframe 不重建），并清空内嵌旧过期时间的媒体票据。 */
function renew(): Promise<void> {
  if (!renewing) {
    renewing = (async () => {
      const active = launch.value
      if (!active) throw new Error('应用已经关闭')
      const { data } = await api.apps.launch(active.app.manifest.id, controller.signal)
      const current = launch.value
      if (!current || data.app.revision !== current.app.revision)
        throw new Error('应用已停用、卸载或更新，请重新打开')
      launch.value = data
      app.value = data.app
      tickets.clear()
    })().finally(() => { renewing = undefined })
  }
  return renewing
}
async function checkState() {
  const active = launch.value
  const current = generation
  if (!active) return
  // 会话临期（15 分钟内）主动续签：正常使用下会话不再过期。
  if (active.expires_at - Date.now() / 1000 < 900) {
    try { await renew() } catch { /* 续签失败时由下方 ping 按状态码处理。 */ }
    if (current !== generation) return
  }
  const fresh = launch.value
  if (!fresh) return
  try { await api.apps.rpc(fresh.app.manifest.id, fresh.session, 'app.ping', {}, controller.signal) }
  catch (e) {
    if (current !== generation) return
    const status = (e as { response?: { status: number } }).response?.status
    // 休眠唤醒等场景会话已过期：先静默续签一次，成功则继续运行。
    if (status === 403 && rpcCode(e) === 'session_expired') {
      try { await renew(); if (current === generation) return } catch { /* 继续按原错误处理。 */ }
    }
    if (status && [401, 403, 404, 409].includes(status)) fail(status === 404 ? '应用已卸载，请前往应用中心重新安装' : errMsg(e))
  }
}
function back() { router.push('/files' + from.value) }
function center() { router.push({ path: '/apps', query: { from: from.value, app: String(route.params.id) } }) }

async function invoke(method: string, params: Record<string, unknown>, requestSignal: AbortSignal) {
  const active = launch.value
  const current = generation
  if (!active) throw new Error('应用已经关闭')
  const signal = AbortSignal.any([controller.signal, requestSignal])
  const request = async (name: string, input: unknown = params, retried = false): Promise<any> => {
    const live = launch.value
    if (!live) throw new Error('应用已经关闭')
    try {
      const { data } = await api.apps.rpc(live.app.manifest.id, live.session, name, input, signal)
      if (current !== generation) throw new Error('应用已经关闭')
      return data
    } catch (e) {
      // 会话过期：静默续签后重放一次；续签失败（修订变化/断网）按原错误抛出。
      if (!retried && rpcCode(e) === 'session_expired') {
        await renew()
        return request(name, input, true)
      }
      throw e
    }
  }
  const grantUrl = async (ref: { id: number; content_version: string }) => {
    const key = `${ref.id}:${ref.content_version}`
    let url = tickets.get(key)
    if (!url) {
      const grant = await request('media.url', { ...ref, kind: 'preview' })
      if (typeof grant.url !== 'string' || !/^\/api\/apps\/media\/[A-Za-z0-9_.-]+$/.test(grant.url)) throw new Error('文件票据地址无效')
      url = grant.url as string
      if (tickets.size >= 128) tickets.delete(tickets.keys().next().value!)
      tickets.set(key, url)
    }
    return url
  }
  if (method === 'files.readRange' || method === 'files.readRanges' || method === 'assets.read') {
    if (active.app.manifest.api_version < 2) throw new Error('此能力需要应用 API v2')
    if (reads >= 4) throw new Error('文件读取并发已达到上限，请稍后重试')
    reads++
    try {
      if (method === 'assets.read') {
        const path = assetPath(params.path)
        const url = `/api/apps/runtime/${active.app.manifest.id}/${active.app.revision}/${path}`
        const response = await fetch(url, { signal, credentials: 'omit', redirect: 'error' })
        return await boundedBytes(response, 8 * RANGE_LIMIT, signal)
      }
      const fetchAt = async (url: string, offset: number, length: number, rangeSignal: AbortSignal) => {
        const response = await fetch(url, { signal: rangeSignal, credentials: 'omit', redirect: 'error', headers: { Range: `bytes=${offset}-${offset + length - 1}` } })
        return await boundedBytes(response, RANGE_LIMIT, rangeSignal, { offset, length })
      }
      if (method === 'files.readRanges') {
        const batch = rangesInput(params)
        // 票据只申请一次，各段限 4 路并行；任一段失败即取消同批其余在途请求。
        const url = await grantUrl(batch.ref)
        const local = new AbortController()
        const localSignal = AbortSignal.any([signal, local.signal])
        try {
          return await mapLimit(batch.ranges, RANGE_PARALLEL, (range) => fetchAt(url, range.offset, range.length, localSignal))
        } catch (error) { local.abort(); throw error }
      }
      const range = rangeInput(params)
      return await fetchAt(await grantUrl(range.ref), range.offset, range.length, signal)
    } finally { reads-- }
  }
  if (method === 'rpc.batch') {
    if (active.app.manifest.api_version < 2) throw new Error('此能力需要应用 API v2')
    return request('rpc.batch', batchInput(params))
  }
  if (!method.startsWith('ui.')) return request(method)
  if (method === 'ui.setImmersive') {
    return immersion.request(params, active.app.manifest.api_version, () => request('app.ping', {}), signal,
      async () => { await nextTick(); await afterHostPaint(signal) })
  }
  await request('app.ping', {})
  switch (method) {
    case 'ui.report': {
      if (!['ready', 'failed'].includes(String(params.phase)) || typeof params.message !== 'string') throw new Error('诊断信息无效')
      reportHealth(String(params.phase), params.message); return null
    }
    case 'ui.setTitle': {
      if (typeof params.title !== 'string' || params.title.length > 120) throw new Error('页面标题无效')
      pageTitle.value = params.title; return null
    }
    case 'ui.setExitMessage': {
      if (typeof params.message !== 'string' || params.message.length > 300) throw new Error('离开提示无效')
      exitMessage = params.message; return null
    }
    case 'ui.authorizeDirectory': return authorizeDirectory(safeDirectory(params.initial))
    case 'ui.showFile': case 'ui.fileDetails': {
      const file = await request('files.stat', params)
      if (method === 'ui.fileDetails') { detailsFile.value = file; fileDetailsVisible.value = true }
      else setTimeout(() => { if (current === generation) void router.push({ path: '/files' + parentPath(file.path), query: { reveal: file.name } }) }, 0)
      return null
    }
    case 'ui.task': {
      const { id, title, completed, total, state } = params
      if (typeof id !== 'string' || !/^[a-z0-9-]{1,40}$/.test(id) || typeof title !== 'string' || title.length > 120 || !Number.isSafeInteger(completed) || Number(completed) < 0 || (total != null && (!Number.isSafeInteger(total) || Number(total) < Number(completed))) || !['running', 'done', 'cancelled', 'failed'].includes(String(state))) throw new Error('任务状态无效')
      if (!tasks.has(id) && tasks.size >= 8) throw new Error('最多显示八个任务')
      tasks.set(id, { id, title, completed: Number(completed), total: total == null ? undefined : Number(total), state: String(state) }); return null
    }
    case 'ui.close': back(); return null
    case 'ui.openSettings': settingsVisible.value = true; return null
    case 'ui.pickDirectory': {
      if (picking) throw new Error('目录选择窗口已经打开')
      picking = true
      try {
        const scope = active.app.scope ?? []
        let initial = typeof params.initial === 'string' && params.initial.length <= 2048 ? params.initial : '/'
        if (scope.length && !pathInScope(scope, initial)) initial = scope[0]!
        const selected = await picker.value?.open(`${active.app.manifest.name}：选择目录`, initial)
        if (current !== generation) throw new Error('应用已经关闭')
        if (selected != null && (app.value?.scope_mode === 'none' || !pathInScope(app.value?.scope, selected))) throw new Error('所选目录不在应用允许的访问范围内，请先在应用设置中调整目录范围')
        return selected ?? null
      } finally { picking = false }
    }
    case 'ui.download': {
      const result = await request('media.url', { path: params.path, kind: 'download' })
      if (typeof result.url !== 'string' || !/^\/api\/apps\/media\/[A-Za-z0-9_.-]+$/.test(result.url)) throw new Error('下载地址无效')
      const link = document.createElement('a')
      link.href = result.url
      link.download = ''
      document.body.appendChild(link)
      link.click()
      link.remove()
      return null
    }
    default: throw new Error('不支持的界面操作')
  }
}

function onReady(event: MessageEvent) {
  if (!launch.value || port || event.source !== frame.value?.contentWindow || event.origin !== 'null'
    || event.data?.channel !== 'tgdrive-app-v1' || event.data?.type !== 'ready') return
  const channel = new MessageChannel()
  port = channel.port1
  disconnect = connectAppBridge(port, invoke, errMsg)
  const active = launch.value, current = generation, signal = controller.signal
  connected.value = true
  clearTimeout(timeout)
  // DOM 更新不等于已绘制；先移除遮罩并完成宿主绘制，再让沙箱显示可交互内容。
  void nextTick().then(() => afterHostPaint(signal)).then(() => {
    if (current !== generation || !frame.value?.contentWindow) { channel.port2.close(); return }
    frame.value.contentWindow.postMessage({ channel: 'tgdrive-app-v1', type: 'connect',
      context: { id: active.app.manifest.id, name: active.app.manifest.name, version: active.app.manifest.version, api_version: active.app.manifest.api_version, dark: isDark.value,
        scope: { mode: app.value?.scope_mode ?? 'all', paths: [...(app.value?.scope ?? [])] }, launch: contextFile.value ? { action: 'open', file: { ...contextFile.value } } : undefined, data_schema: active.app.manifest.integration?.data_schema,
        capabilities: active.app.manifest.api_version >= 2 ? ['files.changes', 'ui.authorizeDirectory', 'ui.showFile', 'ui.fileDetails', 'ui.setTitle', 'ui.setExitMessage', 'ui.task', 'app.lifecycle', 'app.launch','ui.setImmersive', 'ui.immersiveBackground', 'media.bytes', 'files.readRanges', 'storage.events', 'files.scope', 'rpc.batch', 'covers'] : [] } }, '*', [channel.port2])
  }).catch((error) => {
    channel.port2.close()
    if (current === generation && !signal.aborted) fail(errMsg(error))
  })
}
function settingsSaved(values: SettingValues) {
  port?.postMessage({ type: 'event', name: 'settings.changed', value: values })
}
function onScopeSaved(_paths: string[]) {
  const active = app.value
  if (active) void api.apps.get(active.manifest.id).then(({ data }) => { if (app.value?.revision === data.revision) app.value = data })
}
async function replayFiles(): Promise<boolean> {
  if (!filesCursor || !launch.value || !app.value?.manifest.permissions.includes('files.read')) return false
  if (replaying) { replayAgain = true; return false }
  replaying = true
  const current = generation
  try {
    do {
      replayAgain = false
      const live = launch.value
      if (!live) return false
      const { data } = await api.apps.rpc(live.app.manifest.id, live.session, 'files.changes', { cursor: filesCursor }, controller.signal)
      if (current !== generation) return false
      filesCursor = data.cursor
      if (data.paths === null || data.paths.length) port?.postMessage({ type: 'event', name: 'files.changed', value: data.paths === null ? undefined : { paths: data.paths } })
    } while (replayAgain)
    return true
  } catch { return false }
  finally { replaying = false }
}
/** 服务端事件流转发：只发标识不发明文值，插件自行按需重拉。 */
function forwardHostEvents(event: { kind: string; app_id?: string; key?: string; paths?: string[] }) {
  if (!port || !app.value) return
  if (event.kind === 'files') {
    // 已知变更目录且与应用目录范围无关时不转发，批量上传不会让无关插件全量重扫。
    if (app.value.scope_mode === 'none' || !filesChangeAffectsScope(app.value.scope, event.paths)) return
    if (filesCursor) void replayFiles().then(ok => { if (!ok) port?.postMessage({ type: 'event', name: 'files.changed', value: event.paths ? { paths: event.paths } : undefined }) })
    else port.postMessage({ type: 'event', name: 'files.changed', value: event.paths ? { paths: event.paths } : undefined })
    return
  }
  if (event.kind === 'sync.hint' && !filesCursor) { port.postMessage({ type: 'event', name: 'sync.hint' }); return }
  if (event.kind === 'sync.hint') { void replayFiles().then(filesRecovered => port?.postMessage({ type: 'event', name: 'sync.hint', value: { filesRecovered } })); return }
  if (event.app_id !== app.value.manifest.id) return
  if (event.kind === 'app.storage') port.postMessage({ type: 'event', name: 'storage.changed', value: { key: event.key } })
  else if (event.kind === 'app.settings') port.postMessage({ type: 'event', name: 'settings.changed' })
  else if (event.kind === 'app.scope') {
    // 新范围以服务端为准；拉取后更新本地副本，用于目录选择的预校验。
    api.apps.get(app.value.manifest.id).then(({ data }) => {
      if (app.value && data.revision === app.value.revision) app.value = data
    }).catch(() => { /* 拉取失败时由逐次请求的服务端校验兜底。 */ })
    port.postMessage({ type: 'event', name: 'scope.changed' })
  }
}
function onStorage(event: StorageEvent) { if (event.key === APPS_CHANGED) void checkState() }
async function beforeNavigate(to: { path: string }) {
  if (to.path === '/login') return true
  if (closeTopOverlay()) return false
  if (exitMessage) {
    try { await appConfirm({ title: '离开应用', message: exitMessage, confirmText: '离开' }) } catch { return false }
  }
  if (app.value?.manifest.api_version === 2) await disconnect?.beforeClose()
  return true
}
onBeforeRouteLeave(beforeNavigate)
onBeforeRouteUpdate(beforeNavigate)
watch(isDark, (dark) => port?.postMessage({ type: 'event', name: 'theme.changed', value: { dark } }))
watch(() => [route.params.id, route.query.open], start)
function beforeUnload(event: BeforeUnloadEvent) { if (exitMessage) { event.preventDefault(); event.returnValue = '' } }
onMounted(() => {
  window.addEventListener('beforeunload', beforeUnload)
  window.addEventListener('message', onReady)
  window.addEventListener(APPS_CHANGED, checkState)
  window.addEventListener('storage', onStorage)
  offHostEvents = onHostEvent(forwardHostEvents)
  void start()
})
onUnmounted(() => {
  dispose()
  offHostEvents?.()
  offHostEvents = undefined
  window.removeEventListener('beforeunload', beforeUnload)
  window.removeEventListener('message', onReady)
  window.removeEventListener(APPS_CHANGED, checkState)
  window.removeEventListener('storage', onStorage)
})
</script>

<style scoped>
.app-host { height: var(--app-height, 100dvh); display: flex; flex-direction: column; color: var(--el-text-color-primary); box-sizing: border-box; overflow: hidden; padding: var(--safe-top, 0px) var(--safe-right, 0px) var(--safe-bottom, 0px) var(--safe-left, 0px); }
/* 固定到视口，避免宿主历史滚动位置与 iOS 独立窗口高度差把安全区带离屏幕。 */
.app-host.is-immersive { position: fixed; inset: 0; height: auto; z-index: 50; background: var(--glass-bg-heavy); }
.host-toolbar { display: flex; align-items: center; gap: 12px; padding: 8px 16px; flex-shrink: 0; border-width: 0 0 1px; }
.host-name { flex: 1; min-width: 0; display: flex; align-items: center; gap: 8px; font-size: 14px; font-weight: 600; }
.host-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.host-label span { color: var(--el-text-color-secondary); margin-left: 6px; font-size: 11px; font-weight: normal; }
.host-icon { flex-shrink: 0; width: 18px; height: 18px; }
.host-task { display: flex; align-items: center; justify-content: space-between; padding: 4px 16px; font-size: 13px; gap: 8px; }
.file-path { overflow-wrap: anywhere; }
.host-controls { display: flex; gap: 6px; }
.host-controls .el-button { margin: 0; }
.app-stage { flex: 1; min-height: 0; min-width: 0; position: relative; overflow: hidden; }
.app-stage iframe { border: 0; width: 100%; height: 100%; display: block; background: #10111c; }
.host-status { position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; background: var(--glass-bg-heavy); padding: 20px; text-align: center; }
@media (max-width: 600px) {
  .host-toolbar { padding: 6px; gap: 3px; }
  .host-toolbar :deep(.el-button) { padding: 8px; }
  .host-name .host-label span { display: none; }
  .host-toolbar { flex-wrap: wrap; }
  .host-name { min-width: 60px; }
  .host-controls { margin-left: auto; }

}
</style>
