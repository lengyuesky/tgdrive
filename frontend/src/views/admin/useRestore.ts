import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, errMsg, type RestoreStatus, type RestorePrepare, type RestoreConfirm, type RestoreAuth } from '../../api'
import { holdAuthRedirect } from '../../http'

export const restorePhaseLabel: Record<RestoreStatus['phase'], string> = {
  idle: '尚未准备', preparing: '正在下载和预检', ready: '预检通过，等待确认',
  restarting: '等待服务重启', succeeded: '恢复成功', failed: '恢复失败',
  cancelled: '已取消', rolled_back: '恢复失败，已回滚',
}

/** 只读取服务端持久结果，不持久化密码，也绝不自动重发恢复或解除安全模式请求。 */
export function useRestore(canStart: () => boolean) {
  const status = ref<RestoreStatus>({ phase: 'idle', warnings: [], safe_mode: false })
  const opened = ref(false)
  const busy = ref(false)
  const loaded = ref(false)
  const error = ref('')
  const waiting = ref(false)
  const loginRequired = ref(false)
  const active = computed(() => ['preparing', 'ready', 'restarting'].includes(status.value.phase))
  const locked = computed(() => opened.value || busy.value || active.value || waiting.value)
  let disposed = false
  let timer: ReturnType<typeof setTimeout> | undefined
  let releaseAuth: (() => void) | undefined
  let reading = false
  let revision = 0

  function stopTimer() { clearTimeout(timer); timer = undefined }
  function waitForRestart() {
    waiting.value = true
    releaseAuth ??= holdAuthRedirect()
  }
  function accept(next: RestoreStatus) {
    if (disposed) return
    status.value = next
    loaded.value = true
    if (next.phase === 'restarting') waitForRestart()
    // 一旦确认重启，保持等待页面直到用户主动重新登录。
  }
  function report(e: unknown) {
    const code = (e as { response?: { status?: number } }).response?.status
    if (code === 401) {
      loginRequired.value = true
      error.value = waiting.value ? '登录会话已失效，请等待服务启动后重新登录。' : '认证失败：请检查当前登录密码和两步验证码；会话失效时请重新网页登录。Basic / 应用密码不能用于恢复。'
    } else {
      error.value = waiting.value ? '服务重启中或网络暂不可用，请稍候再登录。不会自动重复提交操作。' : errMsg(e)
    }
  }
  function schedule() {
    stopTimer()
    if (!disposed && !loginRequired.value && (active.value || opened.value || waiting.value)) {
      timer = setTimeout(() => void refresh(), 2000)
    }
  }
  async function refresh() {
    if (disposed || reading || busy.value) return
    reading = true
    const at = revision
    try {
      const { data } = await api.admin.restore.status()
      if (!disposed && at === revision) { accept(data); error.value = '' }
    } catch (e) { if (!disposed && at === revision) report(e) }
    finally { reading = false; if (!disposed) schedule() }
  }
  async function open() {
    if (!canStart() && !active.value && !waiting.value) {
      error.value = '请先保存或放弃设置改动，并等待正在进行的保存、备份完成。恢复向导不会自动保存配置。'
      return
    }
    opened.value = true
    loginRequired.value = false
    await refresh()
  }
  async function run(action: () => Promise<{ data: RestoreStatus }>, restart = false) {
    if (busy.value || disposed || waiting.value) return
    busy.value = true
    error.value = ''
    loginRequired.value = false
    revision++
    stopTimer()
    // 先保护其他后台轮询，避免确认响应到达之前的 401 抢先跳页。
    const release = restart ? holdAuthRedirect() : undefined
    try { accept((await action()).data) }
    catch (e) {
      if (disposed) return
      // 无响应或网关错误时，服务端可能已经接受确认；绝不自动重试。
      const code = (e as { response?: { status?: number } }).response?.status
      if (restart && (!code || code >= 500)) waitForRestart()
      report(e)
    } finally { release?.(); busy.value = false; if (!disposed) schedule() }
  }
  function prepare(body: RestorePrepare) {
    if (!loaded.value || active.value || !canStart() || !body.acknowledge_forward) return
    return run(() => api.admin.restore.prepare(body))
  }
  function confirm(body: RestoreConfirm) {
    if (status.value.phase !== 'ready' || status.value.id !== body.id || body.confirmation !== '恢复' || !canStart()) return
    return run(() => api.admin.restore.confirm(body), true)
  }
  function resume(body: RestoreAuth) {
    if (!status.value.safe_mode || active.value || !canStart()) return
    return run(() => api.admin.restore.resume(body), true)
  }
  function cancel() {
    if (!status.value.id || !['preparing', 'ready'].includes(status.value.phase)) return
    return run(() => api.admin.restore.cancel(status.value.id!))
  }
  onMounted(() => void refresh())
  onUnmounted(() => { disposed = true; revision++; stopTimer(); releaseAuth?.() })
  return { status, opened, busy, loaded, error, waiting, loginRequired, active, locked, open, refresh, prepare, confirm, cancel, resume }
}

export type RestoreController = ReturnType<typeof useRestore>
