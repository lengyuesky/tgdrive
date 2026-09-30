import { computed, inject, onMounted, onUnmounted, provide, ref, watch, type Component, type InjectionKey } from 'vue'
import { onBeforeRouteLeave, useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import {
  Brush,
  Coin,
  Cpu,
  Delete,
  Document,
  Download,
  Histogram,
  Monitor,
  Notebook,
  SwitchButton,
  Upload,
  WarnTriangleFilled,
} from '@element-plus/icons-vue'
import { appConfirm } from '../../dialog'
import { closeTopOverlay } from '../../mobile/overlays'
import { api, errMsg, fmtSize, type DetectedChat, type InboxFailure } from '../../api'
import { CFG_SECTION, SECTIONS, SECTION_STORE, isSectionKey, type SectionKey } from './sections'
import { useAdminConfig } from './useAdminConfig'
import { useAdminExtras } from './useAdminExtras'
import { useBackendLogs } from './useBackendLogs'
import { useRestore } from './useRestore'
import { useTaskQueues } from './useTaskQueues'

interface Tile {
  label: string
  value: string
  icon: Component
  tone?: 'warn'
  action?: () => void
}

interface QuickAction {
  label: string
  desc: string
  icon: Component
  action: () => void
  danger?: boolean
}

/**
 * 系统设置页的全部状态与动作。由 Admin.vue 在 setup 中调用一次并 provide,
 * 各分区子组件通过 useAdminContext() 注入同一份状态,所以跨分区编辑不会丢失,
 * 改动计数 / 脏分区 / 保存条也始终基于同一份表单。
 */
function createAdminState() {
  const router = useRouter()
  const route = useRoute()

  // ---- 分区导航 ----
  /** 优先用地址栏里的分区(便于收藏/跳转),其次是上次停留的分区 */
  function initialSection(): SectionKey {
    const q = String(route.query.s ?? '')
    if (isSectionKey(q)) return q
    const saved = localStorage.getItem(SECTION_STORE) ?? ''
    return isSectionKey(saved) ? saved : 'overview'
  }
  const section = ref<SectionKey>(initialSection())
  const activeSection = computed(
    () => SECTIONS.find((s) => s.key === section.value) ?? SECTIONS[0],
  )

  function go(key: SectionKey) {
    section.value = key
    localStorage.setItem(SECTION_STORE, key)
    // 只改 query,组件不会重建,离开守卫也不会触发
    if (route.query.s !== key) router.replace({ query: { ...route.query, s: key } })
  }

  // 注意调用顺序即 onMounted 的注册顺序:主配置 → 日志 → 迁移轮询 → 维护设置 → beforeunload,与拆分前一致
  // eslint-disable-next-line prefer-const
  let queues: ReturnType<typeof useTaskQueues>
  const config = useAdminConfig(router, () => void queues.loadStorageMigration(true), () => restore.locked.value)
  const { cfg, status, form, leaving, load, buildPatch, save, reloadStatus } = config
  const logs = useBackendLogs(status, section)
  const ext = useAdminExtras(() => restore.locked.value, () => restore.locked.value || restore.status.value.safe_mode)
  const restore = useRestore(() => !config.loading.value && !!ext.extras.value && !changed.value.total && !config.saving.value && !ext.extrasSaving.value && !ext.backingUp.value)
  const { extras, loadExtras, saveExtras, extrasPatchKeys, hasRuleDraft } = ext
  queues = useTaskQueues({
    cfg,
    status,
    extras,
    section,
    reloadStatus,
    extKeys: () => changed.value.extKeys,
  })

  watch(section, (key) => {
    if (key === 'logs') void logs.loadBackendLogs()
    if (key === 'queues') void queues.loadStorageMigration(true)
  })

  onMounted(loadExtras)

  // ---- 收件死信 ----
  const inboxFailureDialog = ref(false)
  const inboxFailureLoading = ref(false)
  const inboxFailures = ref<InboxFailure[]>([])

  async function openInboxFailures() {
    inboxFailureDialog.value = true
    inboxFailureLoading.value = true
    try {
      inboxFailures.value = (await api.admin.inboxFailures()).data.items
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      inboxFailureLoading.value = false
    }
  }

  async function removeInboxFailure(updateId: number) {
    try {
      await api.admin.removeInboxFailure(updateId)
      inboxFailures.value = inboxFailures.value.filter((item) => item.update_id !== updateId)
      if (status.value) status.value.inbox_failures = inboxFailures.value.length
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }

  async function clearInboxFailures() {
    try {
      await appConfirm({
        title: '清空死信',
        message: '清空全部收件死信记录?\n原始 Telegram update 载荷将无法从后台找回。',
        kind: 'danger',
        icon: Delete,
        confirmText: '清空',
      })
    } catch {
      return
    }
    try {
      await api.admin.clearInboxFailures()
      inboxFailures.value = []
      if (status.value) status.value.inbox_failures = 0
      ElMessage.success('死信记录已清空')
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }

  // ---- 检测频道 / 用户 ID ----
  const detectDialog = ref(false)
  const detecting = ref(false)
  const detectBot = ref('')
  const detectedChats = ref<DetectedChat[]>([])

  async function detect() {
    detecting.value = true
    try {
      const { data } = await api.admin.detectChats({
        api_base: form.value.api_base,
        bot_token: form.value.bot_token,
        proxy_enabled: form.value.proxy_enabled,
        proxy_provider: form.value.proxy_provider,
        proxy_url: form.value.proxy_url,
      })
      detectBot.value = data.bot
      detectedChats.value = data.chats
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      detecting.value = false
    }
  }
  function openDetect() {
    detectDialog.value = true
    detect()
  }
  function useChatId(c: DetectedChat) {
    form.value.chat_id = String(c.id)
    detectDialog.value = false
    ElMessage.success(`已填入存储频道 ${c.id},记得点「保存并生效」`)
  }
  function addWhitelist(c: DetectedChat) {
    if (!extras.value) return
    const ids = extras.value.inbox_allowed_chats.split(',').map((s) => s.trim()).filter(Boolean)
    if (ids.includes(String(c.id))) {
      ElMessage.info('已经在白名单里了')
      return
    }
    ids.push(String(c.id))
    extras.value.inbox_allowed_chats = ids.join(',')
    ElMessage.success(`已把 ${c.id} 填进白名单,记得点「保存并生效」`)
  }

  // ---- 维护 ----
  async function cleanup() {
    try {
      await appConfirm({
        title: '清理残留上传',
        message: '清理上传中断留下的残留记录,并尽力删除已发到频道的分块消息。\n正在进行的上传不受影响。',
        kind: 'warning',
        icon: Brush,
        confirmText: '开始清理',
      })
    } catch {
      return
    }
    try {
      const { data } = await api.admin.cleanup()
      ElMessage.success(data.cleaned ? `已清理 ${data.cleaned} 条残留消息` : '没有残留')
      await load()
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }

  async function logoutAll() {
    try {
      await appConfirm({
        title: '登出所有会话',
        message: '所有设备(含本机)的登录状态会立即失效,需要重新登录。\nWebDAV 客户端不受影响(用的是账号密码)。',
        kind: 'warning',
        icon: SwitchButton,
        confirmText: '全部登出',
      })
    } catch {
      return
    }
    try {
      await api.admin.logoutAll()
      leaving.value = true
      await router.push('/login')
    } catch (error) { ElMessage.error(`登出失败：${errMsg(error)}`) }
  }

  // ---- 导出 .env ----
  const envDialog = ref(false)
  const envText = ref('')
  async function exportEnv() {
    try {
      const { data } = await api.admin.envExport()
      envText.value = data
      envDialog.value = true
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }
  async function copyEnv() {
    try {
      await navigator.clipboard.writeText(envText.value)
      ElMessage.success('已复制到剪贴板')
    } catch {
      // 非 HTTPS 访问时浏览器禁用剪贴板 API
      ElMessage.warning('浏览器不允许自动复制,请手动全选复制')
    }
  }

  // ---- 改动追踪:哪些分区有未保存的编辑,一共几项 ----
  const changed = computed(() => {
    const cfgKeys = Object.keys(buildPatch())
    const extKeys = extrasPatchKeys()
    if (hasRuleDraft() && !extKeys.includes('inbox_rules')) extKeys.push('inbox_rules')
    return { cfgKeys, extKeys, total: cfgKeys.length + extKeys.length }
  })

  const dirtySections = computed(() => {
    const set = new Set<SectionKey>()
    for (const k of changed.value.cfgKeys) {
      const sec = CFG_SECTION[k]
      if (sec) set.add(sec)
    }
    for (const k of changed.value.extKeys) {
      set.add(
        k.startsWith('auto_')
          ? 'queues'
          : k.startsWith('inbox')
            ? 'inbox'
            : 'backup',
      )
    }
    return set
  })

  /** 导航右侧的提示角标:待配置 / 死信条数 / 备份没开 */
  const badges = computed(() => {
    const out: Partial<Record<SectionKey, { text: string; tone: 'danger' | 'warn' }>> = {}
    if (cfg.value && !cfg.value.configured) out.telegram = { text: '待配置', tone: 'danger' }
    // 队列积压挂在「后台任务」上——那里才有开关和「立即执行」
    const queued = status.value?.delete_queue?.pending ?? 0
    if (queued) out.queues = { text: String(queued), tone: 'warn' }
    if (status.value?.inbox_failures) {
      out.inbox = { text: String(status.value.inbox_failures), tone: 'warn' }
    }
    const e = extras.value
    if (e && !e.backup_telegram_enabled && !e.backup_webdav_enabled && !e.backup_s3_enabled) {
      out.backup = { text: '未启用', tone: 'warn' }
    }
    return out
  })

  async function saveAll() {
    if (restore.locked.value) return
    if (!changed.value.total) {
      ElMessage.info('没有改动')
      return
    }
    // 先存维护设置:保存账号可能立刻跳登录页
    if (changed.value.extKeys.length) await saveExtras()
    if (changed.value.cfgKeys.length) await save()
  }

  async function discardAll() {
    if (restore.locked.value) return
    await Promise.all([load(), loadExtras()])
    ElMessage.info('已放弃未保存的改动')
  }

  async function reloadAll() {
    if (restore.locked.value) return
    await Promise.all([load(), loadExtras()])
  }

  // ---- 离开前确认,避免切分区后忘了保存 ----
  onBeforeRouteLeave(async (to) => {
    if (to.path === '/login' || leaving.value) return true
    if (closeTopOverlay()) return false
    if (!changed.value.total) return true
    try {
      await appConfirm({
        title: '放弃未保存的改动?',
        message: `还有 ${changed.value.total} 项改动没有保存,离开后会丢失。`,
        kind: 'warning',
        confirmText: '直接离开',
        cancelText: '留在本页',
      })
      return true
    } catch {
      return false
    }
  })

  function onBeforeUnload(e: BeforeUnloadEvent) {
    if (changed.value.total) e.preventDefault()
  }
  onMounted(() => window.addEventListener('beforeunload', onBeforeUnload))
  onUnmounted(() => window.removeEventListener('beforeunload', onBeforeUnload))

  // ---- 概览 ----
  const statTiles = computed<Tile[]>(() => {
    const s = status.value
    return [
      { label: '文件 / 目录', value: `${s?.files ?? 0} / ${s?.dirs ?? 0}`, icon: Document },
      { label: '占用空间', value: s ? fmtSize(s.total_size) : '-', icon: Coin },
      { label: '分块大小', value: s ? fmtSize(s.chunk_size) : '-', icon: Histogram },
      {
        label: '上传 Bot / 并发',
        value: s ? `${s.upload_bot_count} / ${s.upload_concurrency}` : '-',
        icon: Cpu,
      },
      {
        label: '元数据库',
        value: s ? fmtSize(s.db_size) : '-',
        icon: Monitor,
        action: () => go('backup'),
      },
      { label: '回收站', value: `${ext.trashCount.value} 项`, icon: Delete, action: () => go('backup') },
      {
        label: '待删消息',
        value: String(s?.pending_deletes ?? 0),
        icon: Brush,
        tone: s?.pending_deletes ? 'warn' : undefined,
        action: () => go('queues'),
      },
      {
        label: '收件死信',
        value: String(s?.inbox_failures ?? 0),
        icon: WarnTriangleFilled,
        tone: s?.inbox_failures ? 'warn' : undefined,
        action: s?.inbox_failures ? openInboxFailures : undefined,
      },
    ]
  })

  const quickActions = computed<QuickAction[]>(() => [
    {
      label: '后台任务',
      desc: 'Telegram 消息清理,手动执行',
      icon: Cpu,
      action: () => go('queues'),
    },
    {
      label: '查看后台日志',
      desc: '操作进度与失败原因',
      icon: Notebook,
      action: () => go('logs'),
    },
    {
      label: ext.backingUp.value ? '备份中…' : '立即备份',
      desc: '生成快照并上传到已启用渠道',
      icon: Upload,
      action: ext.backupNow,
    },
    { label: '下载数据库', desc: '把当前元数据库存到本机', icon: Download, action: ext.downloadBackup },
    { label: '导出 .env', desc: '查看并复制当前生效配置', icon: Document, action: exportEnv },
    { label: '清理残留上传', desc: '删除中断上传留下的分块', icon: Brush, action: cleanup },
    {
      label: '登出所有会话',
      desc: '所有设备都要重新登录',
      icon: SwitchButton,
      action: logoutAll,
      danger: true,
    },
  ])

  const ctx = {
    restore,
    restoreLocked: restore.locked,
    router,
    section,
    activeSection,
    go,
    ...config,
    ...logs,
    ...ext,
    ...queues,
    inboxFailureDialog,
    inboxFailureLoading,
    inboxFailures,
    openInboxFailures,
    removeInboxFailure,
    clearInboxFailures,
    detectDialog,
    detecting,
    detectBot,
    detectedChats,
    detect,
    openDetect,
    useChatId,
    addWhitelist,
    cleanup,
    logoutAll,
    envDialog,
    envText,
    exportEnv,
    copyEnv,
    changed,
    dirtySections,
    badges,
    saveAll,
    discardAll,
    reloadAll,
    statTiles,
    quickActions,
  }
  return ctx
}

export type AdminContext = ReturnType<typeof createAdminState>

const ADMIN_KEY: InjectionKey<AdminContext> = Symbol('admin')

/** Admin.vue 的 setup 中调用一次:创建共享状态并 provide 给各分区子组件 */
export function useAdmin(): AdminContext {
  const ctx = createAdminState()
  provide(ADMIN_KEY, ctx)
  return ctx
}

/** 分区子组件注入 Admin.vue 提供的共享状态 */
export function useAdminContext(): AdminContext {
  const ctx = inject(ADMIN_KEY)
  if (!ctx) throw new Error('useAdminContext 必须在 Admin.vue 内使用')
  return ctx
}
