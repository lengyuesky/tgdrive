<template>
  <main class="app-container" :class="{ immersive: appImmersive }" :style="appImmersive && background ? { background } : undefined">
    <header v-if="!appImmersive" class="app-toolbar"><el-button @click="leave">关闭应用</el-button><h1>{{ title }}</h1><span aria-live="polite">{{ scopeSummary }}</span><el-button @click="authorize">目录授权</el-button></header>
    <el-alert v-if="error" :title="error" type="error" :closable="false" />
    <p v-if="loading" class="loading">正在启动应用页面…</p>
    <div v-if="bootstrap" class="frame-wrap"><iframe ref="frame" :src="frameUrl" :title="title" sandbox="allow-scripts allow-downloads" referrerpolicy="no-referrer" @load="frameLoaded" /></div>
    <aside v-if="tasks.length && !appImmersive" class="task-list" aria-label="应用任务"><div v-for="task in tasks" :key="task.id"><span>{{ task.title }} · {{ task.completed }}{{ task.total != null ? ` / ${task.total}` : '' }} · {{ task.state }}</span><el-button v-if="task.cancellable && task.state === 'running'" text @click="channel?.notice({ event: 'ui.task_cancel', data: { task_id: task.id } })">请求取消</el-button></div></aside>
    <ScopeEditor ref="scope" /><DirPicker ref="picker" />
  </main>
</template>
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { onBeforeRouteLeave, onBeforeRouteUpdate, useRoute, useRouter } from 'vue-router'
import ScopeEditor from './ScopeEditor.vue'
import DirPicker from '../components/DirPicker.vue'
import { center, endpoint, pageLifecycle, AppApiError, loadInstalled } from './client'
import { createUiChannel } from './ui-channel'
import { appImmersive } from './view-state'
import { isDark } from '../theme'
import { BOOTSTRAP_FRAGMENT_KEY, LIMITS, type FileNode, type InstalledApp, type PageBootstrap, type RuntimeInfo, type TaskProgress } from './contract'
const route = useRoute(), router = useRouter(), frame = ref<HTMLIFrameElement>(), scope = ref<InstanceType<typeof ScopeEditor>>(), picker = ref<InstanceType<typeof DirPicker>>()
const app = ref<InstalledApp>(), bootstrap = ref<PageBootstrap>(), frameUrl = ref(''), title = ref('应用页面'), error = ref(''), loading = ref(false), background = ref<string>(), tasks = ref<TaskProgress[]>([])
const scopeSummary = computed(() => {
  const scope = app.value?.scope.scope
  return !scope || scope.mode === 'none' ? '尚未授权文件夹' : scope.mode === 'all' ? '已授权全部文件夹' : `已授权 ${scope.directories.length} 个文件夹`
})
let channel: ReturnType<typeof createUiChannel> | undefined, monitor: ReturnType<typeof setInterval> | undefined, sequence = 0
const originalTitle = document.title
function restoreShell() { appImmersive.value = false; background.value = undefined; tasks.value = []; document.title = originalTitle }
function encode(value: PageBootstrap) { const bytes = new TextEncoder().encode(JSON.stringify(value)); return btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '') }
function origin() { return window.location.origin }
async function start() {
  const current = ++sequence; loading.value = true; error.value = ''; restoreShell(); bootstrap.value = undefined
  try {
    const id = String(route.params.appId); app.value = await center.app(id)
    const open = typeof route.query.open === 'string' ? route.query.open : undefined
    let launch
    try { launch = await center.launch(id, open ? { open_node_id: open } : {}) }
    catch (failure) {
      if (!(failure instanceof AppApiError) || failure.code !== 'scope_denied') throw failure
      const granted = await scope.value?.open(app.value, '此打开方式尚未获得所选文件的目录授权。取消不会把文件详情交给应用。')
      if (!granted) { await router.replace('/apps'); return }
      launch = await center.launch(id, open ? { open_node_id: open } : {})
    }
    if (current !== sequence) { await pageLifecycle<void>(launch.bootstrap.session_token, 'session.close').catch(() => undefined); return }
    const value = launch.bootstrap; value.theme = isDark.value ? 'dark' : 'light'
    const url = new URL(launch.frame_url, origin())
    const expected = endpoint('assets', { app_id: value.app.id, sha256: app.value.package.sha256, path: app.value.manifest.page!.entry })
    // 查询参数是宿主标明的页面策略修订：策略一变地址随之改变，浏览器不会沿用带旧策略的缓存文档。
    if (value.host_origin !== origin() || url.origin !== origin() || url.pathname !== expected || !/^\?policy=[0-9a-f]{16}$/.test(url.search)) throw new Error('应用启动地址或消息源不匹配')
    url.hash = `${BOOTSTRAP_FRAGMENT_KEY}=${encode(value)}`; bootstrap.value = value; frameUrl.value = url.href; title.value = value.app.name; document.title = title.value
    let selectedDirectory: FileNode | undefined
    channel = createUiChannel(() => frame.value?.contentWindow ?? null, value.ui_nonce, {
      async immersive(active, color) { appImmersive.value = active; background.value = active ? color : undefined; await nextTick() },
      title(text) { title.value = text; document.title = text },
      async pick(initial, signal) {
        selectedDirectory = undefined
        signal.addEventListener('abort', () => picker.value?.close(), { once: true })
        let path = '/'
        if (initial) { const node = await center.node({ by: 'id', id: initial }); if (node.kind === 'directory') path = node.path }
        if (signal.aborted) return null
        const selected = await picker.value?.open('选择文件夹', path)
        if (!selected || signal.aborted) return null
        const node = await center.node({ by: 'path', path: selected })
        if (signal.aborted || node.kind !== 'directory') return null
        selectedDirectory = node; return node
      },
      async scope(request, signal) {
        const selected = selectedDirectory; selectedDirectory = undefined
        signal.addEventListener('abort', () => scope.value?.close(), { once: true })
        if (signal.aborted) return null
        const result = await scope.value?.open(app.value!, request.reason, request.preferred_access, selected) ?? null
        if (result && app.value) app.value.scope = result
        return result
      },
      task(progress) { const index = tasks.value.findIndex((task) => task.id === progress.id); if (index >= 0) tasks.value[index] = progress; else { if (tasks.value.length >= LIMITS.MAX_UI_REQUESTS) tasks.value = tasks.value.filter((task) => task.state === 'running').slice(-LIMITS.MAX_UI_REQUESTS + 1); tasks.value.push(progress) } },
      async closeSession() { await pageLifecycle<void>(value.session_token, 'session.close').catch(() => undefined) },
      async leave() { await leave() },
    })
    if (monitor) clearInterval(monitor)
    monitor = setInterval(async () => {
      try { await pageLifecycle<RuntimeInfo>(value.session_token, 'runtime.info') }
      catch { if (bootstrap.value === value) { error.value = '应用会话已失效，请从应用中心重新打开。'; dispose() } }
    }, 15_000)
  } catch (failure) { if (current === sequence) { error.value = failure instanceof Error ? failure.message : '页面启动失败'; restoreShell() } }
  finally { if (current === sequence) loading.value = false }
}
function receive(event: MessageEvent) { void channel?.receive(event) }
window.addEventListener('message', receive)
function frameLoaded() { channel?.notice({ event: 'ui.theme_changed', data: { theme: isDark.value ? 'dark' : 'light' } }) }
watch(isDark, () => frameLoaded())
function dispose() { channel?.dispose(); channel = undefined; if (monitor) clearInterval(monitor); monitor = undefined; scope.value?.close(); picker.value?.close(); bootstrap.value = undefined; restoreShell() }
async function close() { scope.value?.close(); picker.value?.close(); await channel?.beforeClose(); dispose() }
async function leave() { await close(); const target = typeof route.query.from === 'string' && route.query.from.startsWith('/files') ? route.query.from : '/apps'; await router.replace(target) }
async function authorize() { if (!app.value) return; const result = await scope.value?.open(app.value); if (result) app.value.scope = result; await loadInstalled(true) }
onBeforeRouteLeave(async (to) => { if (to.path === '/login') { const token = bootstrap.value?.session_token; if (token) void pageLifecycle<void>(token, 'session.close').catch(() => undefined); dispose(); return true } await close(); return true })
onBeforeRouteUpdate(async () => { await close(); return true })
watch(() => route.fullPath, () => { void start() }, { immediate: true })
onBeforeUnmount(() => { sequence++; window.removeEventListener('message', receive); void channel?.beforeClose(); dispose() })
</script>
<style scoped>
.app-container { display:flex;flex-direction:column;min-height:calc(var(--app-height,100vh) - 76px);padding:16px;box-sizing:border-box; }.app-toolbar{display:flex;align-items:center;gap:12px;min-height:52px;flex-wrap:wrap}.app-toolbar h1{flex:1;font-size:18px;margin:0}.app-toolbar span{color:var(--el-text-color-secondary);font-size:12px}.frame-wrap{flex:1;min-height:0;height:calc(var(--app-height,100vh) - 164px)}iframe{display:block;width:100%;height:100%;min-height:calc(var(--app-height,100vh) - 164px);border:0;border-radius:12px;background:var(--el-bg-color)}.app-container.immersive{position:fixed;inset:0;z-index:1000;padding:0;min-height:0;height:var(--app-height,100dvh)}.immersive .frame-wrap,.immersive iframe{height:100%;min-height:0;border-radius:0}.task-list{padding:12px;max-height:180px;overflow:auto}.task-list>div{display:flex;align-items:center;justify-content:space-between;gap:12px;font-size:13px;padding:6px 0}.loading{color:var(--el-text-color-secondary)}@media(max-width:600px){.app-toolbar span{display:none}.app-container{padding:8px}.frame-wrap,iframe{min-height:calc(var(--app-height,100vh) - 148px)}}
</style>
