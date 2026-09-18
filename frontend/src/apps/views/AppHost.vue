<template>
  <main class="app-host" :class="{ 'is-immersive': immersive }" :style="{ backgroundColor: immersive ? immersiveBackground : undefined }">
    <header v-show="!immersive" class="host-toolbar glass">
      <el-button text :icon="ArrowLeft" @click="back">返回文件</el-button>
      <div class="host-name">
        <span v-if="app" class="host-icon" aria-hidden="true"><AppIcon :manifest="app.manifest" :revision="app.revision" /></span>
        <span class="host-label">{{ app?.manifest.name ?? '应用' }} <span v-if="app">v{{ app.manifest.version }}</span></span>
      </div>
      <div class="host-controls">
        <el-button v-if="app?.manifest.settings.length" :disabled="!!error || loading" text :icon="Setting" @click="settingsVisible = true">应用设置</el-button>
        <el-button text :icon="Menu" @click="center">应用中心</el-button>
      </div>
    </header>
    <section class="app-stage">
      <iframe v-if="launch" ref="frame" :key="launch.app.revision" :src="launch.frame_url" :title="launch.app.manifest.name"
        sandbox="allow-scripts" allow="autoplay; fullscreen; picture-in-picture" referrerpolicy="no-referrer" />
      <div v-if="loading || (launch && !connected)" class="host-status">
        <el-icon class="is-loading" :size="28"><Loading /></el-icon><p>正在打开应用…</p>
      </div>
      <div v-else-if="error" class="host-status">
        <el-empty :description="error" />
        <div><el-button @click="start">重新打开</el-button><el-button type="primary" @click="center">前往应用中心</el-button></div>
      </div>
    </section>
    <AppSettingsDialog v-model="settingsVisible" :app="app" @saved="settingsSaved" @scope-saved="onScopeSaved" />
    <DirPicker ref="picker" />
  </main>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter, onBeforeRouteLeave, onBeforeRouteUpdate } from 'vue-router'
import { ArrowLeft, Loading, Menu, Setting } from '@element-plus/icons-vue'
import { api, errMsg } from '@drive/api'
import { connectAppBridge, type AppBridge } from '../bridge'
import { afterHostPaint } from '../paint'
import { HostImmersion } from '../immersive'
import { takeLaunch } from '../prefetch'
import { assetPath, boundedBytes, rangeInput, rangesInput, RANGE_LIMIT } from '../bytes'
import { APPS_CHANGED, pathInScope, type AppLaunch, type InstalledApp, type SettingValues } from '../types'
import { onHostEvent } from '../events'
import { isDark } from '@drive/theme'
import { closeTopOverlay } from '@drive/mobile/overlays'
import AppSettingsDialog from '../components/AppSettingsDialog.vue'
import AppIcon from '../components/AppIcon.vue'
import DirPicker from '@drive/components/DirPicker.vue'

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
const from = computed(() => typeof route.query.from === 'string' && route.query.from.startsWith('/') ? route.query.from : '/')
let generation = 0
let controller = new AbortController()
let port: MessagePort | undefined
let disconnect: AppBridge | undefined
let renewing: Promise<void> | undefined
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
  offHostEvents?.()
  offHostEvents = undefined
  connected.value = false
  launch.value = null
}
function fail(message: string) {
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
  app.value = null
  try {
    // 优先复用 60 秒内悬停预取的 launch 响应，省一次往返；过期则正常请求。
    const data = takeLaunch(String(route.params.id))
      ?? (await api.apps.launch(String(route.params.id), controller.signal)).data
    if (current !== generation) return
    app.value = data.app
    launch.value = data
    timeout = setTimeout(() => { if (current === generation && !connected.value) fail('应用连接超时，请重新打开或更新插件包') }, 12_000)
    poll = setInterval(checkState, 15_000)
  } catch (e) {
    if (current !== generation) return
    const status = (e as { response?: { status: number } }).response?.status
    error.value = status === 404 ? '此应用尚未安装，请前往应用中心安装' : errMsg(e)
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
      const fetchRange = async (ref: { id: number; content_version: string }, offset: number, length: number) => {
        const url = await grantUrl(ref)
        const response = await fetch(url, { signal, credentials: 'omit', redirect: 'error', headers: { Range: `bytes=${offset}-${offset + length - 1}` } })
        return await boundedBytes(response, RANGE_LIMIT, signal, { offset, length })
      }
      if (method === 'files.readRanges') {
        const batch = rangesInput(params)
        const blocks: Uint8Array[] = []
        for (const range of batch.ranges) blocks.push(await fetchRange(batch.ref, range.offset, range.length))
        return blocks
      }
      const range = rangeInput(params)
      return await fetchRange(range.ref, range.offset, range.length)
    } finally { reads-- }
  }
  if (!method.startsWith('ui.')) return request(method)
  if (method === 'ui.setImmersive') {
    return immersion.request(params, active.app.manifest.api_version, () => request('app.ping', {}), signal,
      async () => { await nextTick(); await afterHostPaint(signal) })
  }
  await request('app.ping', {})
  switch (method) {
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
        if (selected != null && !pathInScope(scope, selected)) throw new Error('所选目录不在应用允许的访问范围内，请先在应用设置中调整目录范围')
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
        capabilities: active.app.manifest.api_version >= 2 ? ['ui.setImmersive', 'ui.immersiveBackground', 'media.bytes', 'files.readRanges', 'storage.events', 'files.scope'] : [] } }, '*', [channel.port2])
  }).catch((error) => {
    channel.port2.close()
    if (current === generation && !signal.aborted) fail(errMsg(error))
  })
}
function settingsSaved(values: SettingValues) {
  port?.postMessage({ type: 'event', name: 'settings.changed', value: values })
}
function onScopeSaved(paths: string[]) {
  if (app.value) app.value = { ...app.value, scope: paths }
}
/** 服务端事件流转发：只发标识不发明文值，插件自行按需重拉。 */
function forwardHostEvents(event: { kind: string; app_id?: string; key?: string }) {
  if (!port || !app.value) return
  if (event.kind === 'files') { port.postMessage({ type: 'event', name: 'files.changed' }); return }
  if (event.kind === 'sync.hint') { port.postMessage({ type: 'event', name: 'sync.hint' }); return }
  if (event.app_id !== app.value.manifest.id) return
  if (event.kind === 'app.storage') port.postMessage({ type: 'event', name: 'storage.changed', value: { key: event.key } })
  else if (event.kind === 'app.settings') port.postMessage({ type: 'event', name: 'settings.changed' })
  else if (event.kind === 'app.scope') {
    // 新范围以服务端为准；拉取后更新本地副本，用于目录选择的预校验。
    api.apps.get(app.value.manifest.id).then(({ data }) => {
      if (app.value && data.revision === app.value.revision) app.value = { ...app.value, scope: data.scope }
    }).catch(() => { /* 拉取失败时由逐次请求的服务端校验兜底。 */ })
    port.postMessage({ type: 'event', name: 'scope.changed' })
  }
}
function onStorage(event: StorageEvent) { if (event.key === APPS_CHANGED) void checkState() }
async function beforeNavigate(to: { path: string }) {
  if (to.path === '/login') return true
  if (closeTopOverlay()) return false
  if (app.value?.manifest.api_version === 2) await disconnect?.beforeClose()
  return true
}
onBeforeRouteLeave(beforeNavigate)
onBeforeRouteUpdate(beforeNavigate)
watch(isDark, (dark) => port?.postMessage({ type: 'event', name: 'theme.changed', value: { dark } }))
watch(() => route.params.id, start)
onMounted(() => {
  window.addEventListener('message', onReady)
  window.addEventListener(APPS_CHANGED, checkState)
  window.addEventListener('storage', onStorage)
  offHostEvents = onHostEvent(forwardHostEvents)
  void start()
})
onUnmounted(() => {
  dispose()
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
