import { computed, onMounted, ref } from 'vue'
import type { Router } from 'vue-router'
import { ElMessage } from 'element-plus'
import { RefreshLeft, SwitchButton } from '@element-plus/icons-vue'
import { appAlert, appConfirm } from '../../dialog'
import {
  api,
  errMsg,
  fmtDuration,
  type AdminConfig,
  type AdminStatus,
  type CfgKey,
  type ConfigPatch,
  type ProxyProvider,
} from '../../api'
import { sizeInput } from './sections'

/** compose 内自建 telegram-bot-api 服务的地址 */
const LOCAL_API_BASE = 'http://telegram-bot-api:8081'
export const API_MODES = [
  { label: '官方 Bot API', value: 'official' },
  { label: '自建 Bot API', value: 'local' },
]
/** api_id/hash 改动后，tgdrive 与自建 Bot API 都要使用宿主机的新 .env。 */
export const RESTART_CMD =
  'docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api'

/**
 * 主配置(.env 可覆盖的那一组):表单、原值快照、加载/保存/恢复默认/测试连接。
 * onConfigured:加载完成且已配置时的回调(用于顺带刷新存储迁移状态)。
 */
export function useAdminConfig(router: Router, onConfigured: () => void, writesBlocked = () => false) {
  const cfg = ref<AdminConfig>()
  const status = ref<AdminStatus>()
  const loading = ref(true)
  const saving = ref(false)
  const testing = ref(false)
  /** 保存账号 / 登出后主动跳转,此时不该再拦截 */
  const leaving = ref(false)

  /** 表单当前值(字符串化,便于比对是否改动) */
  const form = ref({
    api_base: '',
    proxy_enabled: false,
    proxy_provider: 'manual' as ProxyProvider,
    proxy_url: '',
    bot_token: '',
    upload_bot_tokens: '',
    chat_id: '',
    chunk_size: '',
    upload_concurrency: 2,
    download_concurrency: 4,
    send_video: false,
    tmp_dir: '',
    username: '',
    password: '',
    telegram_api_id: '',
    telegram_api_hash: '',
  })
  /** 加载时的原值,用于判断哪些项被改过 */
  const origin = ref({ ...form.value })

  async function load() {
    loading.value = true
    try {
      const [c, s] = await Promise.all([api.admin.config(), api.admin.status()])
      cfg.value = c.data
      status.value = s.data
      form.value = {
        api_base: c.data.values.api_base,
        proxy_enabled: c.data.values.proxy_enabled ?? false,
        proxy_provider: c.data.values.proxy_provider ?? 'manual',
        proxy_url: c.data.values.proxy_url ?? '',
        bot_token: c.data.values.bot_token, // 脱敏值,不动则不提交
        upload_bot_tokens: c.data.values.upload_bot_tokens,
        chat_id: c.data.values.chat_id,
        chunk_size: sizeInput(c.data.values.chunk_size),
        upload_concurrency: c.data.values.upload_concurrency,
        download_concurrency: c.data.values.download_concurrency,
        send_video: c.data.values.send_video,
        tmp_dir: c.data.values.tmp_dir,
        username: c.data.values.username,
        password: c.data.values.password, // 占位符,同上
        telegram_api_id: c.data.values.telegram_api_id,
        telegram_api_hash: c.data.values.telegram_api_hash, // 脱敏值,同上
      }
      origin.value = { ...form.value }
      if (c.data.configured) onConfigured()
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      loading.value = false
    }
  }
  onMounted(load)

  async function reloadStatus() {
    try {
      status.value = (await api.admin.status()).data
    } catch {
      // 队列操作本身已经给过反馈,刷新状态失败不再打扰
    }
  }

  const isOfficial = computed(
    () => form.value.api_base.replace(/\/$/, '') === cfg.value?.official_api_base,
  )

  /** 记住上一次用过的自建地址,来回切换不丢自定义值 */
  let lastLocalBase = ''
  /** 各端点各自记住上一次的分块大小,来回切换互不污染 */
  const lastChunk: Record<'official' | 'local', string> = { official: '', local: '' }
  /** 视频消息开关同理;没动过则取端点默认(自建开、官方关) */
  const lastVideo: Record<'official' | 'local', boolean | null> = { official: null, local: null }
  const apiMode = computed<'official' | 'local'>({
    get: () => (isOfficial.value ? 'official' : 'local'),
    set: (m) => {
      if (m === 'official') {
        if (!isOfficial.value) lastLocalBase = form.value.api_base
        form.value.api_base = cfg.value?.official_api_base ?? 'https://api.telegram.org'
      } else {
        form.value.api_base = lastLocalBase || LOCAL_API_BASE
      }
      // 分块大小跟着端点走:官方默认 18MB,自建默认 1900MB(上限留 100MB 余量)
      lastChunk[m === 'official' ? 'local' : 'official'] = form.value.chunk_size
      const fallback = m === 'official'
        ? (cfg.value?.official_chunk_default ?? 18 * 1024 ** 2)
        : (cfg.value?.local_chunk_default ?? 1900 * 1024 ** 2)
      const next = lastChunk[m] || sizeInput(fallback)
      if (next !== form.value.chunk_size) {
        form.value.chunk_size = next
        ElMessage.info(`分块大小已随端点切换为 ${next}`)
      }
      // 视频消息模式也跟着端点走
      lastVideo[m === 'official' ? 'local' : 'official'] = form.value.send_video
      form.value.send_video = lastVideo[m] ?? m === 'local'
    },
  })
  const uptime = computed(() =>
    status.value ? fmtDuration(status.value.now - status.value.started_at) : '-',
  )
  const taskHealthy = (task: AdminStatus['tasks'][number]) =>
    task.running && !!status.value && status.value.now - task.last_heartbeat <= 120

  /** 只把真正改过的字段发给后端 */
  function buildPatch(): ConfigPatch {
    const p: ConfigPatch = {}
    const f = form.value
    const o = origin.value
    if (f.api_base !== o.api_base) p.api_base = f.api_base
    if (f.proxy_enabled !== o.proxy_enabled) p.proxy_enabled = f.proxy_enabled
    if (f.proxy_provider !== o.proxy_provider) p.proxy_provider = f.proxy_provider
    if (f.proxy_url !== o.proxy_url) p.proxy_url = f.proxy_url
    if (f.bot_token !== o.bot_token) p.bot_token = f.bot_token
    if (f.upload_bot_tokens !== o.upload_bot_tokens) {
      p.upload_bot_tokens = f.upload_bot_tokens
    }
    if (f.chat_id !== o.chat_id) p.chat_id = f.chat_id
    if (f.chunk_size !== o.chunk_size) p.chunk_size = f.chunk_size
    if (f.upload_concurrency !== o.upload_concurrency) p.upload_concurrency = f.upload_concurrency
    if (f.download_concurrency !== o.download_concurrency) p.download_concurrency = f.download_concurrency
    if (f.send_video !== o.send_video) p.send_video = f.send_video
    if (f.tmp_dir !== o.tmp_dir) p.tmp_dir = f.tmp_dir
    if (f.username !== o.username) p.username = f.username
    if (f.password !== o.password) p.password = f.password
    if (f.telegram_api_id !== o.telegram_api_id) p.telegram_api_id = f.telegram_api_id
    if (f.telegram_api_hash !== o.telegram_api_hash) p.telegram_api_hash = f.telegram_api_hash
    return p
  }

  async function save(force = false) {
    if (writesBlocked()) return
    const patch = buildPatch()
    if (!Object.keys(patch).length) return
    if (force) patch.force = true
    saving.value = true
    try {
      const { data } = await api.admin.save(patch)
      if (data.account_changed) {
        leaving.value = true
        await appAlert({
          title: '需要重新登录',
          message: '账号已修改,所有设备的登录状态已失效,请用新账号重新登录。',
          icon: SwitchButton,
          confirmText: '去登录',
        })
        router.push('/login')
        return
      }
      if (data.needs_local_api_restart) {
        await appAlert({
          title: '还差一步',
          message:
            `已保存。api_id / api_hash 同时用于 tgdrive 的 MTProto 流式播放和 telegram-bot-api，还需要在宿主机上：\n\n` +
            `1. 点「导出 .env」,把内容覆盖到项目目录的 .env\n` +
            `2. 执行 ${RESTART_CMD}\n\n` +
            `两个服务重启后新凭据才生效。`,
          kind: 'warning',
          confirmText: '知道了',
        })
      } else {
        ElMessage.success(`已保存并立即生效:${data.changed.join('、')}`)
      }
      await load()
    } catch (e) {
      const msg = errMsg(e)
      // 连不上 Telegram 时给一次强制保存的机会(例如自建端点还没启动)
      if (!force && msg.includes('强制保存')) {
        try {
          await appConfirm({
            title: '连接验证未通过',
            message: msg,
            kind: 'warning',
            confirmText: '仍然保存',
            cancelText: '返回修改',
          })
          await save(true)
        } catch {
          /* 用户选择返回修改 */
        }
      } else {
        ElMessage.error(msg)
      }
    } finally {
      saving.value = false
    }
  }

  async function resetKey(key: CfgKey, label: string) {
    try {
      await appConfirm({
        title: '恢复默认',
        message: `把「${label}」恢复为 .env / 默认值?`,
        kind: 'warning',
        icon: RefreshLeft,
        confirmText: '恢复',
      })
    } catch {
      return
    }
    try {
      if (writesBlocked()) return
      // 恢复空地址时同时恢复开关，避免留下「已启用但无地址」的无效组合。
      const keys: CfgKey[] = ['proxy_url', 'proxy_provider'].includes(key) ? ['proxy_enabled', 'proxy_provider', 'proxy_url'] : [key]
      const { data } = await api.admin.reset(keys)
      if (data.account_changed) {
        leaving.value = true
        await appAlert({
          title: '需要重新登录',
          message: '账号已恢复,请重新登录。',
          icon: SwitchButton,
          confirmText: '去登录',
        })
        router.push('/login')
        return
      }
      ElMessage.success('已恢复')
      await load()
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }

  async function test() {
    testing.value = true
    try {
      const { data } = await api.admin.test({
        api_base: form.value.api_base,
        bot_token: form.value.bot_token,
        chat_id: form.value.chat_id,
        proxy_enabled: form.value.proxy_enabled,
        proxy_provider: form.value.proxy_provider,
        proxy_url: form.value.proxy_url,
      })
      ElMessage.success(`连接正常:@${data.bot}`)
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      testing.value = false
    }
  }

  return {
    cfg,
    status,
    loading,
    saving,
    testing,
    leaving,
    form,
    origin,
    load,
    reloadStatus,
    isOfficial,
    apiMode,
    uptime,
    taskHealthy,
    buildPatch,
    save,
    resetKey,
    test,
  }
}
