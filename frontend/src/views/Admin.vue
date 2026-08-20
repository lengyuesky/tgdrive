<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, type Component } from 'vue'
import { onBeforeRouteLeave, useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import {
  ArrowLeft,
  Brush,
  Coin,
  Cpu,
  Delete,
  Document,
  Download,
  FolderOpened,
  Histogram,
  InfoFilled,
  Link,
  Message,
  Money,
  Monitor,
  Moon,
  Odometer,
  Plus,
  Promotion,
  Refresh,
  RefreshLeft,
  Setting,
  Sunny,
  SwitchButton,
  Tools,
  Upload,
  User,
  VideoPlay,
  WarnTriangleFilled,
} from '@element-plus/icons-vue'
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
  type BackupLastTimes,
  type CfgKey,
  type ConfigPatch,
  type DetectedChat,
  type Extras,
  type InboxRouteRule,
  type InboxFailure,
} from '../api'
import DirPicker from '../components/DirPicker.vue'
import SettingField from '../components/SettingField.vue'

const router = useRouter()
const route = useRoute()
const cfg = ref<AdminConfig>()
const status = ref<AdminStatus>()
const loading = ref(true)
const saving = ref(false)
const testing = ref(false)
const inboxFailureDialog = ref(false)
const inboxFailureLoading = ref(false)
const inboxFailures = ref<InboxFailure[]>([])

// ---- 分区导航:一次只显示一块,避免整页长滚动 ----
type SectionKey = 'overview' | 'telegram' | 'stream' | 'account' | 'backup' | 'inbox' | 'advanced'

const SECTIONS: { key: SectionKey; label: string; desc: string; icon: Component }[] = [
  { key: 'overview', label: '概览', desc: '运行状态与快捷操作', icon: Odometer },
  { key: 'telegram', label: 'Telegram 存储', desc: 'Bot、频道与分块', icon: Promotion },
  { key: 'stream', label: '流式播放', desc: 'MTProto / 自建 Bot API', icon: VideoPlay },
  { key: 'account', label: '账号', desc: '登录用户名与密码', icon: User },
  { key: 'backup', label: '备份与回收站', desc: '数据库快照与保留期', icon: FolderOpened },
  { key: 'inbox', label: 'Bot 收件', desc: '自动入库与分类目录', icon: Message },
  { key: 'advanced', label: '高级', desc: '临时目录、只读项与维护', icon: Tools },
]

const SECTION_STORE = 'tgdrive.adminSection'
const isSectionKey = (k: string): k is SectionKey => SECTIONS.some((s) => s.key === k)

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
const API_MODES = [
  { label: '官方 Bot API', value: 'official' },
  { label: '自建 Bot API', value: 'local' },
]
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

/** api_id/hash 改动后，tgdrive 与自建 Bot API 都要使用宿主机的新 .env。 */
const RESTART_CMD =
  'docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api'

async function save(force = false) {
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
    const { data } = await api.admin.reset([key])
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
  ElMessage.success(`已把 ${c.id} 填进白名单,记得点「保存并生效」`)
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
  leaving.value = true
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

// ---- 维护类设置(回收站/备份/Bot 收件) ----
const extras = ref<Extras>()
/** 加载时的深拷贝快照,用于统计维护类设置改了几项 */
const extrasOrigin = ref<Extras>()
const backupChunkSizeInput = ref('18MB')
const backupChunkOrigin = ref('18MB')
const backupLastTs = ref<number | null>(null)
const backupLastByTarget = ref<BackupLastTimes>({ telegram: null, webdav: null, s3: null })
const trashCount = ref(0)
const extrasSaving = ref(false)
const backingUp = ref(false)
const inboxDirPicker = ref<InstanceType<typeof DirPicker>>()
const inboxRuleExtensionDrafts = ref<string[]>([])

type InboxDirKey = 'inbox_dir' | 'inbox_video_dir' | 'inbox_image_dir' | 'inbox_book_dir'
const INBOX_DIRS: { key: InboxDirKey; label: string; help: string }[] = [
  {
    key: 'inbox_dir',
    label: '默认收件目录',
    help: '没有命中任何类型规则的文件放这里,目录不存在时自动创建。',
  },
  {
    key: 'inbox_video_dir',
    label: '视频目录',
    help: 'mp4、mkv、mov、avi、webm、flv、wmv、ts、m2ts、mpeg、3gp 等。',
  },
  {
    key: 'inbox_image_dir',
    label: '图片目录',
    help: 'jpg、png、gif、webp、bmp、avif、heic、tiff、svg 等。',
  },
  {
    key: 'inbox_book_dir',
    label: '书籍目录',
    help: 'pdf、epub、mobi、azw3、fb2、djvu、cbz、cbr、txt、rtf 等。',
  },
]

async function chooseInboxDir(key: InboxDirKey) {
  if (!extras.value) return
  const label = INBOX_DIRS.find((d) => d.key === key)?.label ?? '收件目录'
  const picked = await inboxDirPicker.value?.open(`选择${label}`, extras.value[key])
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
    extrasOrigin.value = JSON.parse(JSON.stringify(data.extras))
    backupChunkSizeInput.value = sizeInput(data.extras.backup_chunk_size)
    backupChunkOrigin.value = backupChunkSizeInput.value
    inboxRuleExtensionDrafts.value = data.extras.inbox_rules.map(() => '')
    backupLastTs.value = data.backup_last_ts
    backupLastByTarget.value = data.backup_last_by_target
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

// ---- 改动追踪:哪些分区有未保存的编辑,一共几项 ----
/** 后缀输入框里还有没敲回车的内容,保存时会一并收下 */
const hasRuleDraft = () => inboxRuleExtensionDrafts.value.some((d) => d.trim().length > 0)

function extrasPatchKeys(): string[] {
  const cur = extras.value
  const org = extrasOrigin.value
  if (!cur || !org) return []
  const keys = (Object.keys(cur) as (keyof Extras)[])
    .filter((k) => JSON.stringify(cur[k]) !== JSON.stringify(org[k]))
    .map(String)
  if (backupChunkSizeInput.value !== backupChunkOrigin.value) keys.push('backup_chunk_size')
  return keys
}

const changed = computed(() => {
  const cfgKeys = Object.keys(buildPatch())
  const extKeys = extrasPatchKeys()
  if (hasRuleDraft() && !extKeys.includes('inbox_rules')) extKeys.push('inbox_rules')
  return { cfgKeys, extKeys, total: cfgKeys.length + extKeys.length }
})

/** 配置项 → 所属分区,用于在导航上标出哪块有改动 */
const CFG_SECTION: Record<string, SectionKey> = {
  api_base: 'telegram',
  bot_token: 'telegram',
  chat_id: 'telegram',
  chunk_size: 'telegram',
  upload_concurrency: 'telegram',
  send_video: 'telegram',
  telegram_api_id: 'stream',
  telegram_api_hash: 'stream',
  username: 'account',
  password: 'account',
  tmp_dir: 'advanced',
}
const dirtySections = computed(() => {
  const set = new Set<SectionKey>()
  for (const k of changed.value.cfgKeys) {
    const sec = CFG_SECTION[k]
    if (sec) set.add(sec)
  }
  for (const k of changed.value.extKeys) set.add(k.startsWith('inbox') ? 'inbox' : 'backup')
  return set
})

/** 导航右侧的提示角标:待配置 / 死信条数 / 备份没开 */
const badges = computed(() => {
  const out: Partial<Record<SectionKey, { text: string; tone: 'danger' | 'warn' }>> = {}
  if (cfg.value && !cfg.value.configured) out.telegram = { text: '待配置', tone: 'danger' }
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
  if (!changed.value.total) {
    ElMessage.info('没有改动')
    return
  }
  // 先存维护设置:保存账号可能立刻跳登录页
  if (changed.value.extKeys.length) await saveExtras()
  if (changed.value.cfgKeys.length) await save()
}

async function discardAll() {
  await Promise.all([load(), loadExtras()])
  ElMessage.info('已放弃未保存的改动')
}

async function reloadAll() {
  await Promise.all([load(), loadExtras()])
}

// ---- 离开前确认,避免切分区后忘了保存 ----
/** 保存账号 / 登出后主动跳转,此时不该再拦截 */
const leaving = ref(false)
onBeforeRouteLeave(async () => {
  if (leaving.value || !changed.value.total) return true
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
interface Tile {
  label: string
  value: string
  icon: Component
  tone?: 'warn'
  action?: () => void
}
const statTiles = computed<Tile[]>(() => {
  const s = status.value
  return [
    { label: '文件 / 目录', value: `${s?.files ?? 0} / ${s?.dirs ?? 0}`, icon: Document },
    { label: '占用空间', value: s ? fmtSize(s.total_size) : '-', icon: Coin },
    { label: '分块大小', value: s ? fmtSize(s.chunk_size) : '-', icon: Histogram },
    { label: '上传并发', value: String(s?.upload_concurrency ?? '-'), icon: Cpu },
    {
      label: '元数据库',
      value: s ? fmtSize(s.db_size) : '-',
      icon: Monitor,
      action: () => go('backup'),
    },
    { label: '回收站', value: `${trashCount.value} 项`, icon: Delete, action: () => go('backup') },
    { label: '待删消息', value: String(s?.pending_deletes ?? 0), icon: Brush },
    { label: '待同步路径', value: String(s?.pending_caption_syncs ?? 0), icon: Refresh },
    {
      label: '收件死信',
      value: String(s?.inbox_failures ?? 0),
      icon: WarnTriangleFilled,
      tone: s?.inbox_failures ? 'warn' : undefined,
      action: s?.inbox_failures ? openInboxFailures : undefined,
    },
  ]
})

interface QuickAction {
  label: string
  desc: string
  icon: Component
  action: () => void
  danger?: boolean
}
const quickActions = computed<QuickAction[]>(() => [
  {
    label: backingUp.value ? '备份中…' : '立即备份',
    desc: '生成快照并上传到已启用渠道',
    icon: Upload,
    action: backupNow,
  },
  { label: '下载数据库', desc: '把当前元数据库存到本机', icon: Download, action: downloadBackup },
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

async function backupNow() {
  backingUp.value = true
  try {
    const { data } = await api.admin.backupNow()
    const succeeded = data.destinations.filter((item) => item.ok).map((item) => item.label)
    const failed = data.destinations
      .filter((item) => !item.ok)
      .map((item) => `${item.label}：${item.error ?? '失败'}`)
    if (data.ok) {
      ElMessage.success(`备份完成：${data.name}（${succeeded.join('、')}）`)
    } else if (data.succeeded > 0) {
      ElMessage.warning(`部分完成：${succeeded.join('、')}；失败：${failed.join('；')}`)
    } else {
      ElMessage.error(`备份失败：${failed.join('；')}`)
    }
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
      <el-tag v-if="status" size="small" round type="info" class="ver">v{{ status.version }}</el-tag>
      <div class="spacer" />
      <el-tooltip content="导出 .env">
        <el-button class="icon-btn" :icon="Download" @click="exportEnv" />
      </el-tooltip>
      <el-tooltip content="刷新">
        <el-button class="icon-btn" :icon="Refresh" @click="reloadAll" />
      </el-tooltip>
      <el-tooltip :content="isDark ? '切换亮色' : '切换暗色'">
        <el-button class="icon-btn" :icon="isDark ? Sunny : Moon" @click="toggleDark" />
      </el-tooltip>
    </div>

    <div v-if="cfg && !cfg.configured" class="note warn setup-note">
      <el-icon><WarnTriangleFilled /></el-icon>
      <div>
        <b>还没配置 Telegram 存储</b>,网盘暂时不能上传下载。Token 找 @BotFather
        申请;存储频道建议新建一个私有频道,把 bot 拉进去设为管理员。
      </div>
      <el-button type="primary" size="small" @click="go('telegram')">去配置</el-button>
    </div>

    <div class="layout">
      <nav class="nav glass">
        <button
          v-for="s in SECTIONS"
          :key="s.key"
          class="nav-item"
          :class="{ on: section === s.key }"
          @click="go(s.key)"
        >
          <span class="nav-ico"><el-icon><component :is="s.icon" /></el-icon></span>
          <span class="nav-text">
            <span class="nav-label">{{ s.label }}</span>
            <span class="nav-desc">{{ s.desc }}</span>
          </span>
          <span class="nav-tail">
            <span v-if="dirtySections.has(s.key)" class="nav-dot" title="有未保存的改动" />
            <span v-if="badges[s.key]" class="nav-badge" :class="badges[s.key]!.tone">
              {{ badges[s.key]!.text }}
            </span>
          </span>
        </button>
      </nav>

      <div class="pane glass">
        <div class="pane-head">
          <div class="pane-ico">
            <el-icon><component :is="activeSection.icon" /></el-icon>
          </div>
          <div class="pane-title">
            <h2>{{ activeSection.label }}</h2>
            <div class="pane-desc">{{ activeSection.desc }}</div>
          </div>
          <div class="pane-actions">
            <el-button v-if="section === 'telegram'" :loading="testing" @click="test">
              测试连接
            </el-button>
            <el-button v-if="section === 'backup'" :loading="backingUp" @click="backupNow">
              立即备份
            </el-button>
            <el-button v-if="section === 'inbox'" plain @click="addInboxRule">
              添加自定义类型
            </el-button>
          </div>
        </div>

        <transition name="pane" mode="out-in">
          <div :key="section" class="pane-body">
            <!-- ========== 概览 ========== -->
            <template v-if="section === 'overview'">
              <div class="hero">
                <span class="status-dot" :class="status?.bot ? 'ok' : 'bad'"><i /></span>
                <div class="hero-text">
                  <div class="hero-title">
                    {{
                      status?.bot
                        ? `@${status.bot}`
                        : status?.configured
                          ? '连接不上 Telegram'
                          : '尚未配置 Telegram'
                    }}
                  </div>
                  <div class="hero-sub">
                    {{ status?.official_api ? '官方 Bot API' : '自建 Bot API' }} · 已运行
                    {{ uptime }} · 监听 {{ status?.listen }}
                  </div>
                </div>
                <el-button :loading="testing" @click="test">测试连接</el-button>
              </div>

              <div class="tiles">
                <div
                  v-for="t in statTiles"
                  :key="t.label"
                  class="tile"
                  :class="[t.tone, { clickable: !!t.action }]"
                  @click="t.action?.()"
                >
                  <span class="tile-ico"><el-icon><component :is="t.icon" /></el-icon></span>
                  <div class="tile-body">
                    <div class="tile-val">{{ t.value }}</div>
                    <div class="tile-label">{{ t.label }}</div>
                  </div>
                </div>
              </div>

              <div class="block">
                <div class="block-head">后台任务</div>
                <div class="tag-row">
                  <el-tooltip
                    v-for="task in status?.tasks ?? []"
                    :key="task.name"
                    :content="task.last_error ?? `重启 ${task.restarts} 次`"
                  >
                    <el-tag size="small" round :type="taskHealthy(task) ? 'success' : 'danger'">
                      {{ task.name }}
                    </el-tag>
                  </el-tooltip>
                  <span v-if="!status?.tasks?.length" class="muted">暂无任务</span>
                </div>
              </div>

              <div class="block">
                <div class="block-head">快捷操作</div>
                <div class="quick">
                  <button
                    v-for="q in quickActions"
                    :key="q.desc"
                    class="quick-item"
                    :class="{ danger: q.danger }"
                    @click="q.action()"
                  >
                    <el-icon><component :is="q.icon" /></el-icon>
                    <span class="quick-text">
                      <b>{{ q.label }}</b>
                      <em>{{ q.desc }}</em>
                    </span>
                  </button>
                </div>
              </div>

              <div class="note">
                <el-icon><InfoFilled /></el-icon>
                <div>
                  元数据库是文件的唯一索引 —— Bot API 读不了频道历史,库丢了文件就找不回来,
                  记得在「备份与回收站」里至少启用一个远端渠道。数据库
                  {{ status?.db_path }}。
                </div>
              </div>
            </template>

            <!-- ========== Telegram 存储 ========== -->
            <template v-else-if="section === 'telegram'">
              <div class="note">
                <el-icon><InfoFilled /></el-icon>
                <div>
                  <div>
                    改「存储频道」只影响之后上传的文件:已存文件的分块仍在原频道,
                    删除时可能留下无法自动清理的消息。
                  </div>
                  <div>从自建端点切回官方端点前请确认历史分块不超过 20MB,否则那些文件将下载不了。</div>
                </div>
              </div>

              <div class="fields">
                <SettingField
                  label="接口端点"
                  :source="cfg?.sources.api_base"
                  wide
                  @reset="resetKey('api_base', '接口端点')"
                >
                  <div class="ctl-col">
                    <el-segmented v-model="apiMode" :options="API_MODES" />
                    <el-input
                      v-if="apiMode === 'local'"
                      v-model="form.api_base"
                      placeholder="http://telegram-bot-api:8081"
                    />
                  </div>
                  <template #help>
                    官方端点分块 ≤18MB、限流较紧;自建端点(compose 已随项目启动)分块可到 GB 级。
                  </template>
                </SettingField>

                <SettingField
                  label="Bot Token"
                  :source="cfg?.sources.bot_token"
                  help="找 @BotFather 申请;显示的是脱敏值,不修改就保持原样。"
                  @reset="resetKey('bot_token', 'Bot Token')"
                >
                  <el-input v-model="form.bot_token" placeholder="不修改就保持原样" />
                </SettingField>

                <SettingField
                  label="存储频道"
                  :source="cfg?.sources.chat_id"
                  help="不知道频道 ID?填好 Bot Token 后点「检测 ID」,按提示操作即可。"
                  @reset="resetKey('chat_id', '存储频道')"
                >
                  <div class="ctl-row">
                    <el-input v-model="form.chat_id" placeholder="-100xxxxxxxxxx 或 @channelname" />
                    <el-button @click="openDetect">检测 ID</el-button>
                  </div>
                </SettingField>

                <SettingField
                  label="分块大小"
                  :source="cfg?.sources.chunk_size"
                  @reset="resetKey('chunk_size', '分块大小')"
                >
                  <el-input v-model="form.chunk_size" placeholder="如 18MB / 512MB" />
                  <template #help>
                    {{
                      isOfficial
                        ? `官方端点的 getFile 下载上限 20MB,分块不能超过 ${fmtSize(cfg?.official_chunk_max ?? 0)}`
                        : '自建端点单文件上限 2000MB,分块越大消息越少、速度越快'
                    }}。只影响之后上传的文件。
                  </template>
                </SettingField>

                <SettingField
                  label="上传并发"
                  :source="cfg?.sources.upload_concurrency"
                  inline
                  help="同时发往频道的分块数;调太高容易触发限流。"
                  @reset="resetKey('upload_concurrency', '上传并发')"
                >
                  <el-input-number
                    v-model="form.upload_concurrency"
                    :min="1"
                    :max="16"
                    size="small"
                    controls-position="right"
                  />
                </SettingField>

                <SettingField
                  label="视频消息模式"
                  :source="cfg?.sources.send_video"
                  inline
                  wide
                  @reset="resetKey('send_video', '视频消息模式')"
                >
                  <el-switch v-model="form.send_video" />
                  <template #help>
                    开启后,不超过分块大小的 mp4 / mov / m4v 会以「视频消息」发到频道,在 Telegram
                    里可直接播放(文件内容不变,下载不受影响)。mkv / webm 仍走文档 —— Telegram
                    播放器不支持其常见编码,发成视频消息只有声音没画面。超过分块大小的视频按文档分块存储。
                    自建端点默认开启,官方端点默认关闭。
                  </template>
                </SettingField>
              </div>
            </template>

            <!-- ========== 流式播放 ========== -->
            <template v-else-if="section === 'stream'">
              <div class="note">
                <el-icon><InfoFilled /></el-icon>
                <div>
                  <div>
                    tgdrive 使用 MTProto 把视频按 512KiB 响应浏览器 Range,关闭预览后停止后续请求;
                    不改变已有 Telegram 分块大小。自建 telegram-bot-api 仍负责 GB
                    级分块上传和本地缓存直读。
                  </div>
                  <div>
                    下面两项在
                    <el-link type="primary" href="https://my.telegram.org" target="_blank"
                      >my.telegram.org</el-link
                    >
                    申请,由 tgdrive 和 telegram-bot-api 共同读取。
                  </div>
                </div>
              </div>

              <div class="fields">
                <SettingField
                  label="api_id"
                  :source="cfg?.sources.telegram_api_id"
                  @reset="resetKey('telegram_api_id', 'api_id')"
                >
                  <el-input v-model="form.telegram_api_id" placeholder="纯数字,如 21293566" />
                </SettingField>
                <SettingField
                  label="api_hash"
                  :source="cfg?.sources.telegram_api_hash"
                  @reset="resetKey('telegram_api_hash', 'api_hash')"
                >
                  <el-input
                    v-model="form.telegram_api_hash"
                    placeholder="32 位十六进制;不修改就保持原样"
                  />
                </SettingField>
              </div>

              <div class="block">
                <div class="block-head">启用步骤</div>
                <div class="steps">
                  <div class="step"><span>1</span><div>填好上面两项,点右下角「保存并生效」</div></div>
                  <div class="step">
                    <span>2</span>
                    <div>点顶栏的<b>导出 .env</b>,把内容覆盖到项目目录的 <code>.env</code></div>
                  </div>
                  <div class="step">
                    <span>3</span>
                    <div>在宿主机执行 <code>{{ RESTART_CMD }}</code>,重启这两个服务</div>
                  </div>
                </div>
              </div>

              <div class="note">
                <el-icon><InfoFilled /></el-icon>
                <div>
                  如需 GB 级分块上传,再回「Telegram 存储」把接口端点切到「自建 Bot API」并保存。
                </div>
              </div>
            </template>

            <!-- ========== 账号 ========== -->
            <template v-else-if="section === 'account'">
              <div class="fields">
                <SettingField
                  label="用户名"
                  :source="cfg?.sources.username"
                  @reset="resetKey('username', '用户名')"
                >
                  <el-input v-model="form.username" autocomplete="username" />
                </SettingField>
                <SettingField
                  label="密码"
                  :source="cfg?.sources.password"
                  @reset="resetKey('password', '密码')"
                >
                  <el-input
                    v-model="form.password"
                    type="password"
                    show-password
                    autocomplete="new-password"
                  />
                </SettingField>
              </div>
              <div class="note warn">
                <el-icon><WarnTriangleFilled /></el-icon>
                <div>
                  改动账号后所有设备的登录状态立即失效,需要重新登录;WebDAV 客户端也要同步改密码。
                </div>
              </div>
            </template>

            <!-- ========== 备份与回收站 ========== -->
            <template v-else-if="section === 'backup'">
              <template v-if="extras">
                <div class="note">
                  <el-icon><InfoFilled /></el-icon>
                  <div>
                    一次只生成一份 SQLite 一致性快照,并同时上传到所有已启用渠道;
                    每个渠道独立滚动保留最近 7 份。
                  </div>
                </div>

                <div class="fields">
                  <SettingField
                    label="回收站保留"
                    inline
                    :help="`单位天,0 = 永不自动清理。当前回收站 ${trashCount} 项,超期自动彻底删除并释放频道消息。`"
                  >
                    <el-input-number
                      v-model="extras.trash_retention_days"
                      :min="0"
                      :max="3650"
                      size="small"
                      controls-position="right"
                    />
                  </SettingField>
                  <SettingField
                    label="每日自动备份"
                    inline
                    :help="`每 24 小时备份到所有启用渠道,失败渠道每 30 分钟单独重试。${backupLastTs ? `最近成功:${fmtTime(backupLastTs)}` : '还没备份过。'}`"
                  >
                    <el-switch v-model="extras.backup_auto" />
                  </SettingField>
                </div>

                <div class="block">
                  <div class="block-head">备份渠道</div>
                  <div class="channels">
                    <div class="channel" :class="{ on: extras.backup_telegram_enabled }">
                      <div class="channel-head">
                        <span class="channel-ico"><el-icon><Promotion /></el-icon></span>
                        <div class="channel-name">
                          <b>Telegram 存储频道</b>
                          <span>
                            {{
                              backupLastByTarget.telegram
                                ? `上次成功 ${fmtTime(backupLastByTarget.telegram)}`
                                : '尚未成功备份'
                            }}
                          </span>
                        </div>
                        <el-switch v-model="extras.backup_telegram_enabled" />
                      </div>
                      <div v-if="extras.backup_telegram_enabled" class="fields channel-fields">
                        <SettingField label="备份分块大小" wide>
                          <el-input v-model="backupChunkSizeInput" placeholder="如 18MB / 512MB" />
                          <template #help>
                            使用上方 Telegram 存储配置。官方端点不得超过
                            {{ fmtSize(cfg?.official_chunk_max ?? 19_500_000) }},自建端点最大
                            1900MB;多块备份按文件名顺序合并后恢复。
                          </template>
                        </SettingField>
                      </div>
                    </div>

                    <div class="channel" :class="{ on: extras.backup_webdav_enabled }">
                      <div class="channel-head">
                        <span class="channel-ico"><el-icon><Link /></el-icon></span>
                        <div class="channel-name">
                          <b>WebDAV</b>
                          <span>
                            {{
                              backupLastByTarget.webdav
                                ? `上次成功 ${fmtTime(backupLastByTarget.webdav)}`
                                : '尚未成功备份'
                            }}
                          </span>
                        </div>
                        <el-switch v-model="extras.backup_webdav_enabled" />
                      </div>
                      <div v-if="extras.backup_webdav_enabled" class="fields channel-fields">
                        <SettingField
                          label="备份目录 URL"
                          wide
                          help="目录需预先存在并允许 PUT、DELETE;修改 URL 后旧目录中的备份不会被自动删除。"
                        >
                          <el-input
                            v-model="extras.backup_webdav_url"
                            placeholder="https://dav.example.com/remote.php/dav/files/user/tgdrive"
                          />
                        </SettingField>
                        <SettingField label="用户名(可选)">
                          <el-input v-model="extras.backup_webdav_username" autocomplete="username" />
                        </SettingField>
                        <SettingField label="密码(可选)" help="已保存的密码只显示固定占位符。">
                          <el-input
                            v-model="extras.backup_webdav_password"
                            type="password"
                            show-password
                            autocomplete="new-password"
                            placeholder="未修改则保留原密码"
                          />
                        </SettingField>
                      </div>
                    </div>

                    <div class="channel" :class="{ on: extras.backup_s3_enabled }">
                      <div class="channel-head">
                        <span class="channel-ico"><el-icon><Money /></el-icon></span>
                        <div class="channel-name">
                          <b>S3 兼容对象存储</b>
                          <span>
                            {{
                              backupLastByTarget.s3
                                ? `上次成功 ${fmtTime(backupLastByTarget.s3)}`
                                : '尚未成功备份'
                            }}
                          </span>
                        </div>
                        <el-switch v-model="extras.backup_s3_enabled" />
                      </div>
                      <div v-if="extras.backup_s3_enabled" class="fields channel-fields">
                        <SettingField label="服务端点" wide>
                          <el-input
                            v-model="extras.backup_s3_endpoint"
                            placeholder="https://s3.amazonaws.com 或 http://minio:9000"
                          />
                        </SettingField>
                        <SettingField label="区域">
                          <el-input v-model="extras.backup_s3_region" placeholder="us-east-1 / auto" />
                        </SettingField>
                        <SettingField label="存储桶">
                          <el-input v-model="extras.backup_s3_bucket" placeholder="my-backups" />
                        </SettingField>
                        <SettingField label="对象前缀(可选)" wide>
                          <el-input v-model="extras.backup_s3_prefix" placeholder="tgdrive" />
                        </SettingField>
                        <SettingField label="Access Key">
                          <el-input
                            v-model="extras.backup_s3_access_key"
                            type="password"
                            show-password
                            autocomplete="new-password"
                            placeholder="未修改则保留原密钥"
                          />
                        </SettingField>
                        <SettingField label="Secret Key">
                          <el-input
                            v-model="extras.backup_s3_secret_key"
                            type="password"
                            show-password
                            autocomplete="new-password"
                            placeholder="未修改则保留原密钥"
                          />
                        </SettingField>
                        <SettingField label="Session Token(临时凭据可选)" wide>
                          <el-input
                            v-model="extras.backup_s3_session_token"
                            type="password"
                            show-password
                            autocomplete="new-password"
                          />
                        </SettingField>
                        <SettingField
                          label="路径样式"
                          inline
                          wide
                          help="endpoint/bucket/key 形式,MinIO、IP 端点通常需要开启。使用 AWS Signature V4;修改端点、桶、前缀或 Access Key 后,旧目标中的备份不会被自动删除。"
                        >
                          <el-switch v-model="extras.backup_s3_path_style" />
                        </SettingField>
                      </div>
                    </div>
                  </div>
                </div>

                <div class="row">
                  <el-button :loading="backingUp" :icon="Upload" @click="backupNow">
                    立即备份到已启用渠道
                  </el-button>
                  <el-button :icon="Download" @click="downloadBackup">下载数据库备份</el-button>
                </div>
                <div class="note">
                  <el-icon><InfoFilled /></el-icon>
                  <div>
                    WebDAV 与 S3 保存完整 .db;Telegram 单块 .db 可直接恢复,多块需按 part
                    编号顺序二进制合并后再替换 data/tgdrive.db。
                  </div>
                </div>
              </template>
            </template>

            <!-- ========== Bot 收件 ========== -->
            <template v-else-if="section === 'inbox'">
              <template v-if="extras">
                <div v-if="status?.inbox_failures" class="note warn">
                  <el-icon><WarnTriangleFilled /></el-icon>
                  <div>
                    有 {{ status.inbox_failures }} 条消息连续处理失败并进入死信,后续收件未被阻塞。
                  </div>
                  <el-button size="small" @click="openInboxFailures">查看与清理</el-button>
                </div>
                <div class="note">
                  <el-icon><InfoFilled /></el-icon>
                  <div>
                    <div>
                      开启后:<b>直接往存储频道发媒体</b>会自动登记进网盘(零流量);
                      白名单用户私聊发送<b>文件、视频、图片或音频</b>也会按原类型零流量转存。
                    </div>
                    <div>官方 Bot API 下载单文件上限 20MB:更大的文件也会入库,但需要自建端点才能下载。</div>
                  </div>
                </div>

                <div class="fields">
                  <SettingField label="启用收件" inline wide>
                    <el-switch v-model="extras.inbox_enabled" />
                  </SettingField>

                  <SettingField
                    v-for="d in INBOX_DIRS"
                    :key="d.key"
                    :label="d.label"
                    :help="d.help"
                  >
                    <div class="ctl-row">
                      <el-input v-model="extras[d.key]" placeholder="/收件箱" />
                      <el-button @click="chooseInboxDir(d.key)">选择</el-button>
                    </div>
                  </SettingField>

                  <SettingField
                    label="私聊白名单"
                    wide
                    help="数字为 Telegram 用户 ID:用对方账号私聊 bot 发条消息,点「检测 ID」就能看到;不在名单里的私聊消息一律忽略。"
                  >
                    <div class="ctl-row">
                      <el-input
                        v-model="extras.inbox_allowed_chats"
                        placeholder="如 123456789,987654321;留空则只收频道直发"
                      />
                      <el-button @click="openDetect">检测 ID</el-button>
                    </div>
                  </SettingField>
                </div>

                <div class="block">
                  <div class="block-head">
                    自定义类型路由
                    <el-button link type="primary" @click="addInboxRule">添加</el-button>
                  </div>
                  <div class="note">
                    <el-icon><InfoFilled /></el-icon>
                    <div>
                      自定义规则优先于内置类型;按文件名后缀匹配,忽略大小写,并支持
                      <code>tar.gz</code> 这类多段后缀,重叠时更长的后缀优先。
                      同一个后缀不能出现在两条自定义规则中。
                    </div>
                  </div>
                  <div v-if="extras.inbox_rules.length" class="rules">
                    <div v-for="(rule, index) in extras.inbox_rules" :key="index" class="rule">
                      <div class="rule-head">
                        <b>规则 {{ index + 1 }}{{ rule.name.trim() ? ` · ${rule.name.trim()}` : '' }}</b>
                        <el-button type="danger" link @click="removeInboxRule(index)">删除</el-button>
                      </div>
                      <div class="fields channel-fields">
                        <SettingField label="类型名称">
                          <el-input
                            v-model="rule.name"
                            maxlength="40"
                            placeholder="例如：音乐、压缩包、项目源码"
                          />
                        </SettingField>
                        <SettingField label="存储目录">
                          <div class="ctl-row">
                            <el-input v-model="rule.dir" placeholder="/收件箱/自定义" />
                            <el-button @click="chooseRuleDir(rule)">选择</el-button>
                          </div>
                        </SettingField>
                        <SettingField
                          label="文件后缀"
                          wide
                          help="可用逗号、分号或空格一次添加多个后缀,回车即添加。"
                        >
                          <div class="ext-editor">
                            <div v-if="rule.extensions.length" class="tag-row">
                              <el-tag
                                v-for="(extension, extensionIndex) in rule.extensions"
                                :key="`${extension}-${extensionIndex}`"
                                closable
                                round
                                @close="removeRuleExtension(rule, extensionIndex)"
                              >
                                {{ extension.startsWith('.') ? extension : `.${extension}` }}
                              </el-tag>
                            </div>
                            <div class="ctl-row">
                              <el-input
                                v-model="inboxRuleExtensionDrafts[index]"
                                placeholder="输入 mp3、flac 或 tar.gz"
                                @keydown.enter.prevent="addRuleExtensions(rule, index)"
                              />
                              <el-button @click="addRuleExtensions(rule, index)">添加</el-button>
                            </div>
                          </div>
                        </SettingField>
                      </div>
                    </div>
                  </div>
                  <button v-else class="rule-empty" @click="addInboxRule">
                    <el-icon><Plus /></el-icon>
                    <span class="quick-text">
                      <b>添加自定义类型</b>
                      <em>未匹配的文件会使用内置类型或默认目录</em>
                    </span>
                  </button>
                </div>
              </template>
            </template>

            <!-- ========== 高级 ========== -->
            <template v-else>
              <div class="fields">
                <SettingField
                  label="临时目录"
                  :source="cfg?.sources.tmp_dir"
                  wide
                  help="超过 32MB 的分块会先落到这里再上传,需要有足够剩余空间。"
                  @reset="resetKey('tmp_dir', '临时目录')"
                >
                  <el-input v-model="form.tmp_dir" />
                </SettingField>
              </div>

              <div class="block">
                <div class="block-head">
                  只读项 <span class="muted">改 .env 后重启容器才生效</span>
                </div>
                <div class="ro-grid">
                  <div v-for="(v, k) in cfg?.readonly ?? {}" :key="k" class="ro-item">
                    <span class="ro-key">{{ k }}</span>
                    <span class="ro-val">{{ v }}</span>
                  </div>
                </div>
                <div class="note">
                  <el-icon><InfoFilled /></el-icon>
                  <div>端口在 compose 里映射,数据库路径决定后台设置存在哪。</div>
                </div>
              </div>

              <div class="block">
                <div class="block-head">维护</div>
                <div class="row">
                  <el-button :icon="Brush" @click="cleanup">清理残留上传</el-button>
                  <el-button type="danger" plain :icon="SwitchButton" @click="logoutAll">
                    登出所有会话
                  </el-button>
                </div>
              </div>
            </template>
          </div>
        </transition>
      </div>
    </div>

    <transition name="savebar">
      <div v-if="changed.total" class="savebar glass">
        <span class="savebar-dot" />
        <span class="savebar-text">{{ changed.total }} 项改动待保存</span>
        <el-button text @click="discardAll">放弃</el-button>
        <el-button type="primary" :loading="saving || extrasSaving" @click="saveAll">
          保存并生效
        </el-button>
      </div>
    </transition>

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
      <div class="note">
        <el-icon><InfoFilled /></el-icon>
        <div>
          <div>
            找<b>频道 ID</b>:把 bot{{ detectBot ? `(@${detectBot})` : '' }}
            拉进目标频道并设为<b>管理员</b>,在频道里随便发一条消息,再点「重新检测」。
          </div>
          <div>找<b>用户 ID</b>(收件白名单用):用该账号<b>私聊 bot</b> 随便发一条消息,再点「重新检测」。</div>
        </div>
      </div>
      <el-table
        :data="detectedChats"
        size="small"
        v-loading="detecting"
        empty-text="还没检测到会话,按上面的提示给 bot 发条消息后点「重新检测」"
      >
        <el-table-column label="类型" width="76">
          <template #default="{ row }">
            <el-tag size="small" round :type="row.kind === 'channel' ? 'primary' : 'info'">
              {{ KIND_LABEL[row.kind] ?? row.kind }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="名称" show-overflow-tooltip>
          <template #default="{ row }">
            {{ row.name }}<span v-if="row.username" class="muted"> @{{ row.username }}</span>
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
      <div class="muted detect-foot">
        列表来自 bot 最近收到的消息(开启收件后也会持续记录),不会保存任何配置改动。
      </div>
      <template #footer>
        <el-button @click="detectDialog = false">关闭</el-button>
        <el-button type="primary" :loading="detecting" @click="detect">重新检测</el-button>
      </template>
    </el-dialog>

    <el-dialog v-model="envDialog" title="当前生效配置(.env 格式)" width="640px" align-center>
      <div class="note warn">
        <el-icon><WarnTriangleFilled /></el-icon>
        <div>包含 Bot Token 与密码,注意不要外传。</div>
      </div>
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
  max-width: 1180px;
  margin: 0 auto;
  padding: 16px 20px 96px;
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
  margin-bottom: 14px;
  border-radius: 16px;
}
.brand {
  font-size: 20px;
  font-weight: 800;
  letter-spacing: 0.5px;
}
.ver {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.spacer {
  flex: 1;
}
.muted {
  color: var(--text-soft);
  font-size: 12px;
  font-weight: 400;
}

/* ---- 分区导航 ---- */
.layout {
  display: flex;
  gap: 14px;
  align-items: flex-start;
}
.nav {
  width: 236px;
  flex-shrink: 0;
  box-sizing: border-box;
  padding: 8px;
  border-radius: 16px;
  position: sticky;
  top: 76px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.nav-item {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 9px 10px;
  border: none;
  border-radius: 12px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
  transition: background 0.16s ease;
}
.nav-item:hover {
  background: var(--tile-bg-hover);
}
.nav-item.on {
  background: linear-gradient(120deg, rgba(99, 102, 241, 0.18), rgba(14, 165, 233, 0.14));
  box-shadow: inset 0 0 0 1px rgba(99, 102, 241, 0.3);
}
.nav-ico {
  width: 30px;
  height: 30px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 9px;
  font-size: 15px;
  color: var(--text-soft);
  background: var(--tile-bg);
  border: 1px solid var(--glass-border);
}
.nav-item.on .nav-ico {
  color: #fff;
  border-color: transparent;
  background: linear-gradient(135deg, #6366f1, #0ea5e9);
  box-shadow: 0 4px 10px rgba(99, 102, 241, 0.35);
}
.nav-text {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.nav-label {
  font-size: 13.5px;
  font-weight: 600;
}
.nav-desc {
  font-size: 11px;
  color: var(--text-soft);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.nav-tail {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-left: auto;
  flex-shrink: 0;
}
.nav-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--el-color-primary);
  box-shadow: 0 0 6px rgba(64, 158, 255, 0.85);
}
.nav-badge {
  padding: 1px 7px;
  border-radius: 999px;
  font-size: 11px;
  font-weight: 600;
}
.nav-badge.danger {
  color: #ef4444;
  background: rgba(239, 68, 68, 0.16);
}
.nav-badge.warn {
  color: #d97706;
  background: rgba(245, 158, 11, 0.18);
}

/* ---- 内容面板 ---- */
.pane {
  flex: 1;
  min-width: 0;
  border-radius: 16px;
  padding: 16px 18px 20px;
}
.pane-head {
  display: flex;
  align-items: center;
  gap: 12px;
  padding-bottom: 14px;
  margin-bottom: 16px;
  border-bottom: 1px solid var(--glass-border);
}
.pane-ico {
  width: 40px;
  height: 40px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 12px;
  font-size: 19px;
  color: #fff;
  background: linear-gradient(135deg, #6366f1, #0ea5e9);
  box-shadow: 0 6px 16px rgba(99, 102, 241, 0.32);
}
.pane-title h2 {
  margin: 0;
  font-size: 17px;
  font-weight: 700;
}
.pane-desc {
  margin-top: 2px;
  font-size: 12px;
  color: var(--text-soft);
}
.pane-actions {
  margin-left: auto;
  display: flex;
  gap: 8px;
}
.pane-enter-active,
.pane-leave-active {
  transition: opacity 0.16s ease, transform 0.16s ease;
}
.pane-enter-from {
  opacity: 0;
  transform: translateY(6px);
}
.pane-leave-to {
  opacity: 0;
  transform: translateY(-4px);
}

/* ---- 通用块 ---- */
.fields {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 12px;
}
.ctl-row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
}
.ctl-row :deep(.el-input) {
  flex: 1;
  min-width: 0;
}
.ctl-col {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.block {
  margin-top: 18px;
}
.block-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 10px;
  font-size: 13.5px;
  font-weight: 700;
}
.tag-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
}
.note {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 11px 14px;
  margin-bottom: 14px;
  border: 1px solid var(--glass-border);
  border-left: 3px solid var(--el-color-primary);
  border-radius: 12px;
  background: var(--tile-bg);
  font-size: 12.5px;
  line-height: 1.75;
  color: var(--text-soft);
}
.note > .el-icon {
  flex-shrink: 0;
  margin-top: 3px;
  font-size: 15px;
  color: var(--el-color-primary);
}
.note > .el-button {
  flex-shrink: 0;
  align-self: center;
  margin-left: auto;
}
.note.warn {
  border-left-color: #f59e0b;
}
.note.warn > .el-icon {
  color: #f59e0b;
}
.note :deep(code),
.block :deep(code) {
  padding: 1px 5px;
  border-radius: 5px;
  background: var(--tile-media-bg);
  word-break: break-all;
}
.setup-note {
  align-items: center;
}

/* ---- 概览 ---- */
.hero {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 16px;
  margin-bottom: 14px;
  border-radius: 14px;
  border: 1px solid var(--glass-border);
  background: linear-gradient(120deg, rgba(99, 102, 241, 0.14), rgba(14, 165, 233, 0.1));
}
.hero-text {
  min-width: 0;
}
.hero-title {
  font-size: 16px;
  font-weight: 700;
}
.hero-sub {
  margin-top: 3px;
  font-size: 12px;
  color: var(--text-soft);
}
.hero .el-button {
  margin-left: auto;
}
.tiles {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(168px, 1fr));
  gap: 10px;
}
.tile {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 12px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
  transition: background 0.16s ease, transform 0.16s ease;
}
.tile.clickable:hover {
  background: var(--tile-bg-hover);
  transform: translateY(-1px);
}
.tile-ico {
  width: 32px;
  height: 32px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 10px;
  font-size: 16px;
  color: var(--el-color-primary);
  background: rgba(99, 102, 241, 0.13);
}
.tile.warn .tile-ico {
  color: #d97706;
  background: rgba(245, 158, 11, 0.18);
}
.tile-body {
  min-width: 0;
}
.tile-val {
  font-size: 17px;
  font-weight: 700;
  line-height: 1.2;
}
.tile-label {
  margin-top: 2px;
  font-size: 11.5px;
  color: var(--text-soft);
}
.quick {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(226px, 1fr));
  gap: 10px;
}
.quick-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 11px 12px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
  transition: background 0.16s ease;
}
.quick-item:hover {
  background: var(--tile-bg-hover);
}
.quick-item > .el-icon {
  flex-shrink: 0;
  font-size: 17px;
  color: var(--el-color-primary);
}
.quick-item.danger > .el-icon {
  color: var(--el-color-danger);
}
.quick-text {
  min-width: 0;
}
.quick-text b {
  display: block;
  font-size: 13px;
  font-weight: 600;
}
.quick-text em {
  display: block;
  margin-top: 1px;
  font-style: normal;
  font-size: 11.5px;
  color: var(--text-soft);
}

/* ---- 步骤条 ---- */
.steps {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.step {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
  font-size: 13px;
}
.step > span {
  width: 22px;
  height: 22px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 50%;
  font-size: 12px;
  font-weight: 700;
  color: #fff;
  background: var(--el-color-primary);
}

/* ---- 备份渠道 / 收件规则:内嵌的设置项去掉外框,避免套娃 ---- */
.channels,
.rules {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.channel,
.rule {
  padding: 14px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.channel.on {
  border-color: rgba(99, 102, 241, 0.32);
}
.channel-head,
.rule-head {
  display: flex;
  align-items: center;
  gap: 10px;
}
.channel-ico {
  width: 32px;
  height: 32px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 10px;
  font-size: 16px;
  color: var(--text-soft);
  background: var(--tile-media-bg);
}
.channel.on .channel-ico {
  color: #fff;
  background: linear-gradient(135deg, #6366f1, #0ea5e9);
}
.channel-name {
  min-width: 0;
}
.channel-name b {
  display: block;
  font-size: 13.5px;
}
.channel-name span {
  display: block;
  margin-top: 2px;
  font-size: 11.5px;
  color: var(--text-soft);
}
.channel-head > .el-switch,
.rule-head > .el-button {
  margin-left: auto;
}
.channel-fields {
  padding-top: 12px;
  margin-top: 12px;
  border-top: 1px solid var(--glass-border);
  gap: 10px 14px;
}
.channel-fields :deep(.field) {
  padding: 0;
  border: none;
  background: transparent;
}
.ext-editor {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.rule-empty {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 14px;
  border: 1px dashed var(--glass-border);
  border-radius: 12px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
  transition: background 0.16s ease, border-color 0.16s ease;
}
.rule-empty:hover {
  border-color: var(--el-color-primary);
  background: var(--tile-bg);
}
.rule-empty > .el-icon {
  flex-shrink: 0;
  font-size: 17px;
  color: var(--el-color-primary);
}

/* ---- 只读项 ---- */
.ro-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
  gap: 8px;
  margin-bottom: 12px;
}
.ro-item {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  padding: 8px 11px;
  border: 1px solid var(--glass-border);
  border-radius: 10px;
  background: var(--tile-bg);
  font-size: 12.5px;
}
.ro-key {
  color: var(--text-soft);
}
.ro-val {
  font-weight: 600;
  text-align: right;
  word-break: break-all;
}

/* ---- 浮动保存条 ---- */
.savebar {
  position: fixed;
  left: 50%;
  bottom: 20px;
  z-index: 40;
  transform: translateX(-50%);
  display: flex;
  align-items: center;
  gap: 10px;
  max-width: calc(100vw - 32px);
  padding: 9px 10px 9px 16px;
  border-radius: 999px;
}
.savebar-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--el-color-primary);
  box-shadow: 0 0 8px rgba(64, 158, 255, 0.9);
}
.savebar-text {
  font-size: 13px;
  font-weight: 600;
  white-space: nowrap;
}
.savebar-enter-active,
.savebar-leave-active {
  transition: opacity 0.2s ease, transform 0.2s ease;
}
.savebar-enter-from,
.savebar-leave-to {
  opacity: 0;
  transform: translate(-50%, 18px);
}
.detect-foot {
  margin-top: 8px;
}

@media (max-width: 1020px) {
  .fields {
    grid-template-columns: 1fr;
  }
}
@media (max-width: 900px) {
  .layout {
    flex-direction: column;
  }
  .nav {
    position: static;
    width: 100%;
    flex-direction: row;
    overflow-x: auto;
    gap: 6px;
  }
  .nav-item {
    width: auto;
    flex-shrink: 0;
  }
  .nav-desc {
    display: none;
  }
  .pane {
    width: 100%;
    box-sizing: border-box;
  }
}
</style>
