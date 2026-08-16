<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, Brush, Delete, Download, Moon, Refresh, RefreshLeft, Setting, Sunny, SwitchButton } from '@element-plus/icons-vue'
import { appAlert, appConfirm } from '../dialog'
import { isDark, toggleDark } from '../theme'
import {
  api,
  errMsg,
  fmtDuration,
  fmtSize,
  fmtTime,
  type AdminConfig,
  type AdminStatus,
  type CfgKey,
  type ConfigPatch,
  type DetectedChat,
  type Extras,
  type InboxRouteRule,
  type InboxFailure,
} from '../api'
import DirPicker from '../components/DirPicker.vue'

const router = useRouter()
const cfg = ref<AdminConfig>()
const status = ref<AdminStatus>()
const loading = ref(true)
const saving = ref(false)
const testing = ref(false)
const inboxFailureDialog = ref(false)
const inboxFailureLoading = ref(false)
const inboxFailures = ref<InboxFailure[]>([])

/** 表单当前值(字符串化,便于比对是否改动) */
const form = ref({
  api_base: '',
  bot_token: '',
  chat_id: '',
  chunk_size: '',
  upload_concurrency: 2,
  send_video: false,
  tmp_dir: '',
  username: '',
  password: '',
  telegram_api_id: '',
  telegram_api_hash: '',
})
/** 加载时的原值,用于判断哪些项被改过 */
const origin = ref({ ...form.value })

/** 字节数 → "18MB" 这类可直接回填的写法 */
function sizeInput(n: number): string {
  const units: [number, string][] = [
    [1024 ** 3, 'GB'],
    [1024 ** 2, 'MB'],
    [1024, 'KB'],
  ]
  for (const [base, unit] of units) {
    if (n >= base && n % base === 0) return `${n / base}${unit}`
  }
  return String(n)
}

async function load() {
  loading.value = true
  try {
    const [c, s] = await Promise.all([api.admin.config(), api.admin.status()])
    cfg.value = c.data
    status.value = s.data
    form.value = {
      api_base: c.data.values.api_base,
      bot_token: c.data.values.bot_token, // 脱敏值,不动则不提交
      chat_id: c.data.values.chat_id,
      chunk_size: sizeInput(c.data.values.chunk_size),
      upload_concurrency: c.data.values.upload_concurrency,
      send_video: c.data.values.send_video,
      tmp_dir: c.data.values.tmp_dir,
      username: c.data.values.username,
      password: c.data.values.password, // 占位符,同上
      telegram_api_id: c.data.values.telegram_api_id,
      telegram_api_hash: c.data.values.telegram_api_hash, // 脱敏值,同上
    }
    origin.value = { ...form.value }
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    loading.value = false
  }
}
onMounted(load)

const isOfficial = computed(
  () => form.value.api_base.replace(/\/$/, '') === cfg.value?.official_api_base,
)

/** compose 内自建 telegram-bot-api 服务的地址 */
const LOCAL_API_BASE = 'http://telegram-bot-api:8081'
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

/** 只把真正改过的字段发给后端 */
function buildPatch(): ConfigPatch {
  const p: ConfigPatch = {}
  const f = form.value
  const o = origin.value
  if (f.api_base !== o.api_base) p.api_base = f.api_base
  if (f.bot_token !== o.bot_token) p.bot_token = f.bot_token
  if (f.chat_id !== o.chat_id) p.chat_id = f.chat_id
  if (f.chunk_size !== o.chunk_size) p.chunk_size = f.chunk_size
  if (f.upload_concurrency !== o.upload_concurrency) p.upload_concurrency = f.upload_concurrency
  if (f.send_video !== o.send_video) p.send_video = f.send_video
  if (f.tmp_dir !== o.tmp_dir) p.tmp_dir = f.tmp_dir
  if (f.username !== o.username) p.username = f.username
  if (f.password !== o.password) p.password = f.password
  if (f.telegram_api_id !== o.telegram_api_id) p.telegram_api_id = f.telegram_api_id
  if (f.telegram_api_hash !== o.telegram_api_hash) p.telegram_api_hash = f.telegram_api_hash
  return p
}
const dirty = computed(() => Object.keys(buildPatch()).length > 0)

/** api_id/hash 改动后，tgdrive 与自建 Bot API 都要使用宿主机的新 .env。 */
const RESTART_CMD =
  'docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api'

async function save(force = false) {
  const patch = buildPatch()
  if (!Object.keys(patch).length) {
    ElMessage.info('没有改动')
    return
  }
  if (force) patch.force = true
  saving.value = true
  try {
    const { data } = await api.admin.save(patch)
    if (data.account_changed) {
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
    const { data } = await api.admin.reset([key])
    if (data.account_changed) {
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
    })
    ElMessage.success(`连接正常:@${data.bot}`)
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    testing.value = false
  }
}

// ---- 检测频道 / 用户 ID ----
const detectDialog = ref(false)
const detecting = ref(false)
const detectBot = ref('')
const detectedChats = ref<DetectedChat[]>([])

const KIND_LABEL: Record<string, string> = {
  channel: '频道',
  private: '用户',
  user: '用户',
  group: '群组',
  supergroup: '群组',
}
const isUserKind = (k: string) => k === 'private' || k === 'user'

async function detect() {
  detecting.value = true
  try {
    const { data } = await api.admin.detectChats({
      api_base: form.value.api_base,
      bot_token: form.value.bot_token,
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
  ElMessage.success(`已把 ${c.id} 填进白名单,记得点「保存收件设置」`)
}

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
  await api.admin.logoutAll().catch(() => {})
  router.push('/login')
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

const SOURCE_LABEL: Record<string, string> = {
  db: '后台已改',
  env: '来自 .env',
  default: '默认值',
}

// ---- 维护类设置(回收站/备份/Bot 收件) ----
const extras = ref<Extras>()
const backupChunkSizeInput = ref('18MB')
const backupLastTs = ref<number | null>(null)
const trashCount = ref(0)
const extrasSaving = ref(false)
const backingUp = ref(false)
const inboxDirPicker = ref<InstanceType<typeof DirPicker>>()
const inboxRuleExtensionDrafts = ref<string[]>([])

type InboxDirKey = 'inbox_dir' | 'inbox_video_dir' | 'inbox_image_dir' | 'inbox_book_dir'
const INBOX_DIR_LABEL: Record<InboxDirKey, string> = {
  inbox_dir: '默认收件目录',
  inbox_video_dir: '视频目录',
  inbox_image_dir: '图片目录',
  inbox_book_dir: '书籍目录',
}

async function chooseInboxDir(key: InboxDirKey) {
  if (!extras.value) return
  const picked = await inboxDirPicker.value?.open(
    `选择${INBOX_DIR_LABEL[key]}`,
    extras.value[key],
  )
  if (!picked) return
  if (picked === '/') {
    ElMessage.warning('收件目录不能是根目录，请选择或新建一个文件夹')
    return
  }
  extras.value[key] = picked
}

async function chooseRuleDir(rule: InboxRouteRule) {
  const picked = await inboxDirPicker.value?.open(
    `选择「${rule.name.trim() || '自定义类型'}」的目录`,
    rule.dir || extras.value?.inbox_dir || '/收件箱',
  )
  if (!picked) return
  if (picked === '/') {
    ElMessage.warning('收件目录不能是根目录，请选择或新建一个文件夹')
    return
  }
  rule.dir = picked
}

function addInboxRule() {
  if (!extras.value) return
  extras.value.inbox_rules.push({
    name: '',
    extensions: [],
    dir: extras.value.inbox_dir,
  })
  inboxRuleExtensionDrafts.value.push('')
}

function removeInboxRule(index: number) {
  extras.value?.inbox_rules.splice(index, 1)
  inboxRuleExtensionDrafts.value.splice(index, 1)
}

function addRuleExtensions(rule: InboxRouteRule, index: number) {
  const draft = inboxRuleExtensionDrafts.value[index] ?? ''
  const parts = draft
    .split(/[,;\s]+/)
    .map((value) => value.trim().replace(/^\.+/, '').toLowerCase())
    .filter(Boolean)
  if (!parts.length) return

  const seen = new Set(rule.extensions.map((value) => value.replace(/^\.+/, '').toLowerCase()))
  let added = 0
  for (const extension of parts) {
    if (seen.has(extension)) continue
    rule.extensions.push(extension)
    seen.add(extension)
    added++
  }
  inboxRuleExtensionDrafts.value[index] = ''
  if (!added) ElMessage.info('这些文件后缀已经添加过了')
}

function removeRuleExtension(rule: InboxRouteRule, extensionIndex: number) {
  rule.extensions.splice(extensionIndex, 1)
}

async function loadExtras() {
  try {
    const { data } = await api.admin.extras()
    extras.value = data.extras
    backupChunkSizeInput.value = sizeInput(data.extras.backup_chunk_size)
    inboxRuleExtensionDrafts.value = data.extras.inbox_rules.map(() => '')
    backupLastTs.value = data.backup_last_ts
    trashCount.value = data.trash_count
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}
onMounted(loadExtras)

async function saveExtras() {
  if (!extras.value) return
  // 用户直接点保存时，也要收下输入框里尚未按回车的后缀，避免静默丢失。
  extras.value.inbox_rules.forEach((rule, index) => addRuleExtensions(rule, index))
  extrasSaving.value = true
  try {
    await api.admin.saveExtras({
      ...extras.value,
      backup_chunk_size: backupChunkSizeInput.value,
    })
    ElMessage.success('维护设置已保存并生效')
    await loadExtras()
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    extrasSaving.value = false
  }
}

async function backupNow() {
  backingUp.value = true
  try {
    const { data } = await api.admin.backupToChannel()
    ElMessage.success(
      data.parts > 1
        ? `已发送到存储频道:${data.name}（${data.parts} 个分块）`
        : `已发送到存储频道:${data.name}`,
    )
    await loadExtras()
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    backingUp.value = false
  }
}

function downloadBackup() {
  window.open(api.admin.backupUrl, '_blank')
}
</script>

<template>
  <div class="page" v-loading="loading">
    <div class="topbar glass">
      <el-tooltip content="返回文件">
        <el-button class="icon-btn" :icon="ArrowLeft" @click="router.push('/files/')" />
      </el-tooltip>
      <div class="logo-badge"><el-icon><Setting /></el-icon></div>
      <div class="brand brand-text">系统设置</div>
      <div class="spacer" />
      <el-tooltip content="导出 .env">
        <el-button class="icon-btn" :icon="Download" @click="exportEnv" />
      </el-tooltip>
      <el-tooltip content="刷新">
        <el-button class="icon-btn" :icon="Refresh" @click="load" />
      </el-tooltip>
      <el-tooltip :content="isDark ? '切换亮色' : '切换暗色'">
        <el-button class="icon-btn" :icon="isDark ? Sunny : Moon" @click="toggleDark" />
      </el-tooltip>
    </div>

    <el-alert
      v-if="cfg && !cfg.configured"
      type="warning"
      :closable="false"
      show-icon
      title="还没配置 Telegram 存储"
      class="alert"
    >
      在下面的「Telegram 存储」里填好 Bot Token 与存储频道并保存,网盘才能上传下载。
      Token 找 @BotFather 申请;存储频道建议新建一个私有频道,把 bot 拉进去设为管理员。
    </el-alert>

    <el-card shadow="never" class="card">
      <template #header><b>系统状态</b></template>
      <el-descriptions :column="3" size="small" border>
        <el-descriptions-item label="Bot">
          <span v-if="status?.bot">@{{ status.bot }}</span>
          <el-text v-else-if="status && !status.configured" type="warning">未配置</el-text>
          <el-text v-else type="danger">连接不上</el-text>
        </el-descriptions-item>
        <el-descriptions-item label="接口端点">
          {{ status?.official_api ? '官方 Bot API' : '自建 Bot API' }}
        </el-descriptions-item>
        <el-descriptions-item label="分块大小">
          {{ status ? fmtSize(status.chunk_size) : '-' }}
        </el-descriptions-item>
        <el-descriptions-item label="文件 / 目录">
          {{ status?.files ?? 0 }} / {{ status?.dirs ?? 0 }}
        </el-descriptions-item>
        <el-descriptions-item label="占用空间">
          {{ status ? fmtSize(status.total_size) : '-' }}
        </el-descriptions-item>
        <el-descriptions-item label="上传并发">{{ status?.upload_concurrency }}</el-descriptions-item>
        <el-descriptions-item label="元数据库">
          {{ status?.db_path }}({{ status ? fmtSize(status.db_size) : '-' }})
        </el-descriptions-item>
        <el-descriptions-item label="监听地址">{{ status?.listen }}</el-descriptions-item>
        <el-descriptions-item label="已运行">
          {{ uptime }}(v{{ status?.version }})
        </el-descriptions-item>
        <el-descriptions-item label="后台任务">
          <span class="status-tags">
            <el-tooltip
              v-for="task in status?.tasks ?? []"
              :key="task.name"
              :content="task.last_error ?? `重启 ${task.restarts} 次`"
            >
              <el-tag size="small" :type="taskHealthy(task) ? 'success' : 'danger'">
                {{ task.name }}
              </el-tag>
            </el-tooltip>
          </span>
        </el-descriptions-item>
        <el-descriptions-item label="待删消息">
          {{ status?.pending_deletes ?? 0 }}
        </el-descriptions-item>
        <el-descriptions-item label="收件死信">
          <el-button
            v-if="status?.inbox_failures"
            link
            type="warning"
            @click="openInboxFailures"
          >
            {{ status.inbox_failures }} 条，查看
          </el-button>
          <span v-else>0</span>
        </el-descriptions-item>
      </el-descriptions>
      <div class="tip">
        元数据库是文件的唯一索引 —— Bot API 读不了频道历史,库丢了文件就找不回来,记得定期备份。
      </div>
    </el-card>

    <el-card shadow="never" class="card">
      <template #header><b>Telegram 存储</b></template>
      <el-alert type="info" :closable="false" show-icon class="alert">
        <div>改「存储频道」只影响之后上传的文件:已存文件的分块仍在原频道,删除时可能留下无法自动清理的消息。</div>
        <div>从自建端点切回官方端点前请确认历史分块不超过 20MB,否则那些文件将下载不了。</div>
      </el-alert>

      <el-form label-width="110px" label-position="left">
        <el-form-item label="接口端点">
          <div class="row">
            <el-radio-group v-model="apiMode">
              <el-radio-button value="official">官方 Bot API</el-radio-button>
              <el-radio-button value="local">自建 Bot API</el-radio-button>
            </el-radio-group>
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.api_base ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.api_base === 'db'"
              link
              type="primary"
              @click="resetKey('api_base', '接口端点')"
              >恢复</el-button
            >
          </div>
          <div v-if="apiMode === 'local'" class="row sub">
            <el-input v-model="form.api_base" placeholder="http://telegram-bot-api:8081" />
          </div>
          <div class="tip">
            官方端点分块 ≤18MB、限流较紧;自建端点(compose 已随项目启动)分块可到 GB 级。切换后点下方「保存并生效」。
          </div>
        </el-form-item>

        <el-form-item label="Bot Token">
          <div class="row">
            <el-input v-model="form.bot_token" placeholder="不修改就保持原样" />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.bot_token ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.bot_token === 'db'"
              link
              type="primary"
              @click="resetKey('bot_token', 'Bot Token')"
              >恢复</el-button
            >
          </div>
        </el-form-item>

        <el-form-item label="存储频道">
          <div class="row">
            <el-input v-model="form.chat_id" placeholder="-100xxxxxxxxxx 或 @channelname" />
            <el-button @click="openDetect">检测 ID</el-button>
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.chat_id ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.chat_id === 'db'"
              link
              type="primary"
              @click="resetKey('chat_id', '存储频道')"
              >恢复</el-button
            >
          </div>
          <div class="tip">不知道频道 ID?填好 Bot Token 后点「检测 ID」,按提示操作即可。</div>
        </el-form-item>

        <el-form-item label="分块大小">
          <div class="row">
            <el-input v-model="form.chunk_size" placeholder="如 18MB / 512MB" />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.chunk_size ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.chunk_size === 'db'"
              link
              type="primary"
              @click="resetKey('chunk_size', '分块大小')"
              >恢复</el-button
            >
          </div>
          <div class="tip">
            {{
              isOfficial
                ? `官方端点的 getFile 下载上限 20MB,分块不能超过 ${fmtSize(cfg?.official_chunk_max ?? 0)}`
                : '自建端点单文件上限 2000MB,分块越大消息越少、速度越快'
            }}。只影响之后上传的文件。
          </div>
        </el-form-item>

        <el-form-item label="上传并发">
          <div class="row">
            <el-input-number v-model="form.upload_concurrency" :min="1" :max="16" />
            <el-tag size="small" type="info">
              {{ SOURCE_LABEL[cfg?.sources.upload_concurrency ?? ''] }}
            </el-tag>
            <el-button
              v-if="cfg?.sources.upload_concurrency === 'db'"
              link
              type="primary"
              @click="resetKey('upload_concurrency', '上传并发')"
              >恢复</el-button
            >
            <span class="tip">同时发往频道的分块数;调太高容易触发限流</span>
          </div>
        </el-form-item>

        <el-form-item label="视频消息模式">
          <div class="row">
            <el-switch v-model="form.send_video" />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.send_video ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.send_video === 'db'"
              link
              type="primary"
              @click="resetKey('send_video', '视频消息模式')"
              >恢复</el-button
            >
          </div>
          <div class="tip">
            开启后,不超过分块大小的 mp4 / mov / m4v 会以「视频消息」发到频道,在
            Telegram 里可直接播放(文件内容不变,下载不受影响)。mkv / webm 仍走文档
            —— Telegram 播放器不支持其常见编码,发成视频消息只有声音没画面。超过分块大小的视频按文档分块存储。自建端点默认开启,官方端点默认关闭。
          </div>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never" class="card">
      <template #header><b>MTProto 流式播放 / 自建 Bot API</b></template>
      <el-alert type="info" :closable="false" show-icon class="alert">
        <div>
          tgdrive 使用 MTProto 把视频按 512KiB 响应浏览器 Range，关闭预览后停止后续请求；
          不改变已有 Telegram 分块大小。自建 telegram-bot-api 仍负责 GB 级分块上传和本地缓存直读。
        </div>
        <div>
          下面两项在
          <el-link type="primary" href="https://my.telegram.org" target="_blank">my.telegram.org</el-link>
          申请。它们同时由 tgdrive 和 telegram-bot-api 读取。保存后还要导出 .env
          覆盖回项目目录，再执行 <code>{{ RESTART_CMD }}</code> 重启/重建两个服务。
        </div>
      </el-alert>

      <el-form label-width="110px" label-position="left">
        <el-form-item label="api_id">
          <div class="row">
            <el-input v-model="form.telegram_api_id" placeholder="纯数字,如 21293566" />
            <el-tag size="small" type="info">
              {{ SOURCE_LABEL[cfg?.sources.telegram_api_id ?? ''] }}
            </el-tag>
            <el-button
              v-if="cfg?.sources.telegram_api_id === 'db'"
              link
              type="primary"
              @click="resetKey('telegram_api_id', 'api_id')"
              >恢复</el-button
            >
          </div>
        </el-form-item>
        <el-form-item label="api_hash">
          <div class="row">
            <el-input v-model="form.telegram_api_hash" placeholder="32 位十六进制;不修改就保持原样" />
            <el-tag size="small" type="info">
              {{ SOURCE_LABEL[cfg?.sources.telegram_api_hash ?? ''] }}
            </el-tag>
            <el-button
              v-if="cfg?.sources.telegram_api_hash === 'db'"
              link
              type="primary"
              @click="resetKey('telegram_api_hash', 'api_hash')"
              >恢复</el-button
            >
          </div>
        </el-form-item>
      </el-form>

      <div class="tip">
        流式播放启用步骤：填好上面两项并保存 → 导出 .env 覆盖回宿主机 → 执行
        <code>{{ RESTART_CMD }}</code>。如需 GB 级分块上传，再回「Telegram 存储」把接口端点
        切到「自建 Bot API」并保存。
      </div>
    </el-card>

    <el-card shadow="never" class="card">
      <template #header><b>账号</b></template>
      <el-form label-width="110px" label-position="left">
        <el-form-item label="用户名">
          <div class="row">
            <el-input v-model="form.username" />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.username ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.username === 'db'"
              link
              type="primary"
              @click="resetKey('username', '用户名')"
              >恢复</el-button
            >
          </div>
        </el-form-item>
        <el-form-item label="密码">
          <div class="row">
            <el-input v-model="form.password" type="password" show-password />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.password ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.password === 'db'"
              link
              type="primary"
              @click="resetKey('password', '密码')"
              >恢复</el-button
            >
          </div>
          <div class="tip">
            改动账号后所有设备的登录状态立即失效,需要重新登录;WebDAV 客户端也要同步改密码。
          </div>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card v-if="extras" shadow="never" class="card">
      <template #header><b>备份与回收站</b></template>
      <el-form label-width="110px" label-position="left">
        <el-form-item label="回收站保留">
          <div class="row">
            <el-input-number v-model="extras.trash_retention_days" :min="0" :max="3650" />
            <span class="tip">天;0 = 永不自动清理。当前回收站 {{ trashCount }} 项,超期自动彻底删除并释放频道消息。</span>
          </div>
        </el-form-item>
        <el-form-item label="每日自动备份">
          <div class="row">
            <el-switch v-model="extras.backup_auto" />
            <span class="tip">
              每天把数据库快照发到存储频道,滚动保留最近 7 份。
              {{ backupLastTs ? `上次备份:${fmtTime(backupLastTs)}` : '还没备份过' }}
            </span>
          </div>
        </el-form-item>
        <el-form-item label="备份分块">
          <div class="row">
            <el-input
              v-model="backupChunkSizeInput"
              placeholder="如 18MB / 512MB"
              style="width: 180px"
            />
            <span class="tip">
              默认 18MB；官方端点不得超过
              {{ fmtSize(cfg?.official_chunk_max ?? 19_500_000) }}，自建端点最大 1900MB。
            </span>
          </div>
        </el-form-item>
      </el-form>
      <div class="row">
        <el-button type="primary" :loading="extrasSaving" @click="saveExtras">保存维护设置</el-button>
        <el-button :loading="backingUp" @click="backupNow">立即备份到频道</el-button>
        <el-button @click="downloadBackup">下载数据库备份</el-button>
      </div>
      <div class="tip" style="margin-top: 8px">
        元数据库是文件的唯一索引,强烈建议开启自动备份；单块 .db 可直接恢复，多块备份需按
        part 编号顺序二进制合并为 .db 后再替换 data/tgdrive.db。
      </div>
    </el-card>

    <el-card v-if="extras" shadow="never" class="card">
      <template #header><b>Bot 收件</b></template>
      <el-alert
        v-if="status?.inbox_failures"
        type="warning"
        :closable="false"
        show-icon
        class="alert"
      >
        有 {{ status.inbox_failures }} 条消息连续处理失败并进入死信，后续收件未被阻塞。
        <el-button link type="warning" @click="openInboxFailures">查看与清理</el-button>
      </el-alert>
      <el-alert type="info" :closable="false" show-icon class="alert">
        <div>开启后:<b>直接往存储频道发媒体</b>会自动登记进网盘(零流量);白名单用户私聊发送<b>文件、视频、图片或音频</b>也会按原类型零流量转存。</div>
        <div>官方 Bot API 下载单文件上限 20MB:更大的文件也会入库,但需要自建端点才能下载。</div>
      </el-alert>
      <el-form label-width="110px" label-position="left">
        <el-form-item label="启用收件">
          <el-switch v-model="extras.inbox_enabled" />
        </el-form-item>
        <el-form-item label="默认目录">
          <div class="row">
            <el-input v-model="extras.inbox_dir" placeholder="/收件箱" style="max-width: 360px" />
            <el-button @click="chooseInboxDir('inbox_dir')">选择目录</el-button>
          </div>
          <div class="tip">没有命中任何类型规则的文件放在这里，目录不存在时自动创建。</div>
        </el-form-item>
        <el-form-item label="视频目录">
          <div class="row">
            <el-input v-model="extras.inbox_video_dir" style="max-width: 360px" />
            <el-button @click="chooseInboxDir('inbox_video_dir')">选择目录</el-button>
          </div>
          <div class="tip">mp4、mkv、mov、avi、webm、flv、wmv、ts、m2ts、mpeg、3gp 等。</div>
        </el-form-item>
        <el-form-item label="图片目录">
          <div class="row">
            <el-input v-model="extras.inbox_image_dir" style="max-width: 360px" />
            <el-button @click="chooseInboxDir('inbox_image_dir')">选择目录</el-button>
          </div>
          <div class="tip">jpg、png、gif、webp、bmp、avif、heic、tiff、svg 等。</div>
        </el-form-item>
        <el-form-item label="书籍目录">
          <div class="row">
            <el-input v-model="extras.inbox_book_dir" style="max-width: 360px" />
            <el-button @click="chooseInboxDir('inbox_book_dir')">选择目录</el-button>
          </div>
          <div class="tip">pdf、epub、mobi、azw3、fb2、djvu、cbz、cbr、txt、rtf 等。</div>
        </el-form-item>
        <el-form-item label="私聊白名单">
          <div class="row">
            <el-input
              v-model="extras.inbox_allowed_chats"
              placeholder="如 123456789,987654321;留空则只收频道直发"
            />
            <el-button @click="openDetect">检测 ID</el-button>
          </div>
          <div class="tip">
            数字为 Telegram 用户 ID:用对方账号私聊 bot 发条消息,点「检测 ID」就能看到;不在名单里的私聊消息一律忽略。
          </div>
        </el-form-item>
      </el-form>

      <el-divider content-position="left">自定义类型路由</el-divider>
      <el-alert type="info" :closable="false" class="route-help">
        自定义规则优先于上面的内置类型；按文件名后缀匹配，忽略大小写，并支持
        <code>tar.gz</code> 这类多段后缀，重叠时更长的后缀优先。同一个后缀不能出现在两条自定义规则中。
      </el-alert>
      <div v-if="extras.inbox_rules.length" class="route-list">
        <div v-for="(rule, index) in extras.inbox_rules" :key="index" class="route-card">
          <div class="route-head">
            <b>规则 {{ index + 1 }}{{ rule.name.trim() ? ` · ${rule.name.trim()}` : '' }}</b>
            <el-button type="danger" link @click="removeInboxRule(index)">删除</el-button>
          </div>
          <div class="route-grid">
            <label class="route-field">
              <span>类型名称</span>
              <el-input
                v-model="rule.name"
                maxlength="40"
                placeholder="例如：音乐、压缩包、项目源码"
              />
            </label>
            <label class="route-field route-extensions">
              <span>文件后缀</span>
              <div class="extension-editor">
                <div v-if="rule.extensions.length" class="extension-tags">
                  <el-tag
                    v-for="(extension, extensionIndex) in rule.extensions"
                    :key="`${extension}-${extensionIndex}`"
                    closable
                    @close="removeRuleExtension(rule, extensionIndex)"
                  >
                    {{ extension.startsWith('.') ? extension : `.${extension}` }}
                  </el-tag>
                </div>
                <div class="row">
                  <el-input
                    v-model="inboxRuleExtensionDrafts[index]"
                    class="route-extension-input"
                    placeholder="输入 mp3、flac 或 tar.gz"
                    @keydown.enter.prevent="addRuleExtensions(rule, index)"
                  />
                  <el-button @click="addRuleExtensions(rule, index)">添加</el-button>
                </div>
                <span class="tip">可用逗号、分号或空格一次添加多个后缀。</span>
              </div>
            </label>
            <label class="route-field route-target">
              <span>存储目录</span>
              <div class="row">
                <el-input v-model="rule.dir" placeholder="/收件箱/自定义" />
                <el-button @click="chooseRuleDir(rule)">选择目录</el-button>
              </div>
            </label>
          </div>
        </div>
      </div>
      <el-empty v-else description="还没有自定义类型，未匹配文件会使用内置类型或默认目录" :image-size="56" />
      <div class="row route-actions">
        <el-button plain @click="addInboxRule">添加自定义类型</el-button>
        <el-button type="primary" :loading="extrasSaving" @click="saveExtras">保存收件设置</el-button>
      </div>
    </el-card>

    <el-card shadow="never" class="card">
      <template #header><b>高级</b></template>
      <el-form label-width="110px" label-position="left">
        <el-form-item label="临时目录">
          <div class="row">
            <el-input v-model="form.tmp_dir" />
            <el-tag size="small" type="info">{{ SOURCE_LABEL[cfg?.sources.tmp_dir ?? ''] }}</el-tag>
            <el-button
              v-if="cfg?.sources.tmp_dir === 'db'"
              link
              type="primary"
              @click="resetKey('tmp_dir', '临时目录')"
              >恢复</el-button
            >
          </div>
          <div class="tip">超过 32MB 的分块会先落到这里再上传,需要有足够剩余空间。</div>
        </el-form-item>
      </el-form>

      <el-divider content-position="left">需重启容器才能改</el-divider>
      <el-descriptions :column="2" size="small" border>
        <el-descriptions-item v-for="(v, k) in cfg?.readonly ?? {}" :key="k" :label="String(k)">
          {{ v }}
        </el-descriptions-item>
      </el-descriptions>
      <div class="tip">这些项只能改 .env 后重启容器(端口在 compose 里映射,数据库路径决定设置存在哪)。</div>

      <el-divider content-position="left">维护</el-divider>
      <div class="row">
        <el-button @click="cleanup">清理残留上传</el-button>
        <el-button type="danger" plain @click="logoutAll">登出所有会话</el-button>
      </div>
    </el-card>

    <div class="actions glass">
      <el-button type="primary" :loading="saving" :disabled="!dirty" @click="save()">
        保存并生效
      </el-button>
      <el-button :loading="testing" @click="test">测试连接</el-button>
      <el-button :disabled="!dirty" @click="load">放弃改动</el-button>
      <span class="tip">保存后立即生效,不需要重启容器。</span>
    </div>

    <el-dialog v-model="inboxFailureDialog" title="Bot 收件死信" width="760px" align-center>
      <el-table :data="inboxFailures" v-loading="inboxFailureLoading" size="small">
        <el-table-column prop="update_id" label="Update ID" width="120" />
        <el-table-column prop="attempts" label="尝试" width="72" />
        <el-table-column label="失败时间" width="170">
          <template #default="{ row }">{{ fmtTime(row.failed_at) }}</template>
        </el-table-column>
        <el-table-column prop="last_error" label="最后错误" show-overflow-tooltip />
        <el-table-column label="操作" width="72" align="right">
          <template #default="{ row }">
            <el-button link type="danger" @click="removeInboxFailure(row.update_id)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty><el-empty description="没有死信" :image-size="56" /></template>
      </el-table>
      <template #footer>
        <el-button :disabled="!inboxFailures.length" type="danger" plain @click="clearInboxFailures">
          清空全部
        </el-button>
        <el-button type="primary" @click="inboxFailureDialog = false">关闭</el-button>
      </template>
    </el-dialog>

    <DirPicker ref="inboxDirPicker" />

    <el-dialog v-model="detectDialog" title="检测频道 / 用户 ID" width="680px" align-center>
      <el-alert type="info" :closable="false" show-icon class="alert">
        <div>
          找<b>频道 ID</b>:把 bot{{ detectBot ? `(@${detectBot})` : '' }}
          拉进目标频道并设为<b>管理员</b>,在频道里随便发一条消息,再点「重新检测」。
        </div>
        <div>找<b>用户 ID</b>(收件白名单用):用该账号<b>私聊 bot</b> 随便发一条消息,再点「重新检测」。</div>
      </el-alert>
      <el-table
        :data="detectedChats"
        size="small"
        v-loading="detecting"
        empty-text="还没检测到会话,按上面的提示给 bot 发条消息后点「重新检测」"
      >
        <el-table-column label="类型" width="76">
          <template #default="{ row }">
            <el-tag size="small" :type="row.kind === 'channel' ? 'primary' : 'info'">
              {{ KIND_LABEL[row.kind] ?? row.kind }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="名称" show-overflow-tooltip>
          <template #default="{ row }">
            {{ row.name }}<span v-if="row.username" class="tip"> @{{ row.username }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="id" label="ID" width="150" />
        <el-table-column label="最近出现" width="90">
          <template #default="{ row }">{{ fmtTime(row.at).slice(5, 16) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="130">
          <template #default="{ row }">
            <el-button
              v-if="!isUserKind(row.kind)"
              link
              type="primary"
              @click="useChatId(row as DetectedChat)"
            >
              填入存储频道
            </el-button>
            <el-button
              v-else-if="extras"
              link
              type="primary"
              @click="addWhitelist(row as DetectedChat)"
            >
              加入白名单
            </el-button>
          </template>
        </el-table-column>
      </el-table>
      <div class="tip" style="margin-top: 8px">
        列表来自 bot 最近收到的消息(开启收件后也会持续记录),不会保存任何配置改动。
      </div>
      <template #footer>
        <el-button @click="detectDialog = false">关闭</el-button>
        <el-button type="primary" :loading="detecting" @click="detect">重新检测</el-button>
      </template>
    </el-dialog>

    <el-dialog v-model="envDialog" title="当前生效配置(.env 格式)" width="640px" align-center>
      <el-alert type="warning" :closable="false" show-icon class="alert">
        包含 Bot Token 与密码,注意不要外传。
      </el-alert>
      <el-input v-model="envText" type="textarea" :rows="16" readonly />
      <template #footer>
        <el-button @click="envDialog = false">关闭</el-button>
        <el-button type="primary" @click="copyEnv">复制</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.page {
  max-width: 1000px;
  margin: 0 auto;
  padding: 16px 20px 40px;
  min-height: 100vh;
  box-sizing: border-box;
}
.topbar {
  position: sticky;
  top: 12px;
  z-index: 20;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 16px;
  margin-bottom: 16px;
  border-radius: 16px;
}
.brand {
  font-size: 20px;
  font-weight: 800;
  letter-spacing: 0.5px;
}
.spacer {
  flex: 1;
}
.card {
  margin-bottom: 16px;
  border-radius: 8px;
}
.row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
}
.row .el-input {
  max-width: 420px;
}
.row.sub {
  margin-top: 8px;
}
.tip {
  color: var(--text-soft);
  font-size: 12px;
  line-height: 1.7;
}
.alert {
  margin-bottom: 12px;
}
.status-tags {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.route-help {
  margin-bottom: 10px;
}
.route-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.route-card {
  padding: 12px;
  border: 1px solid var(--glass-border);
  border-radius: 10px;
  background: var(--tile-bg);
}
.route-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.route-grid {
  display: grid;
  grid-template-columns: minmax(150px, 0.7fr) minmax(260px, 1.3fr);
  gap: 10px;
}
.route-field {
  display: flex;
  flex-direction: column;
  gap: 5px;
  min-width: 0;
  color: var(--text-soft);
  font-size: 12px;
}
.route-field .el-input {
  width: 100%;
  max-width: none;
}
.extension-editor {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.extension-tags {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.route-target {
  grid-column: 1 / -1;
}
.route-actions {
  margin-top: 12px;
}
.actions {
  position: sticky;
  bottom: 12px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 16px;
  border-radius: 16px;
}
@media (max-width: 700px) {
  .route-grid {
    grid-template-columns: 1fr;
  }
  .route-target {
    grid-column: auto;
  }
}
</style>
