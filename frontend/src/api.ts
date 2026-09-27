import { http } from './http'
import { invalidateSession } from './session'
import { appApi } from '@apps/client'

export interface Entry {
  name: string
  is_dir: boolean
  size: number
  created_at: number
  modified_at: number
  sha256: string | null
  favorite: boolean
}

/** 游标目录专用条目；分享、回收站仍使用原类型。 */
export interface DirectoryEntry extends Entry {
  id: number
  has_thumbnail: boolean
}

export interface EntriesQuery {
  path: string
  limit: number
  cursor?: string
  sort: 'name' | 'size' | 'mtime'
  order: 'asc' | 'desc'
  kind: 'all' | 'dir' | 'file'
  q: string
}

export interface EntriesResponse {
  path: string
  entries: DirectoryEntry[]
  next_cursor: string | null
  has_more: boolean
  total?: number
}

export interface EntryProperties extends Entry {
  path: string
  /** 目录内递归文件数；普通文件为 1。 */
  file_count: number
  /** 目录内递归子目录数；不包含目录自身。 */
  dir_count: number
  /** Telegram 分块记录数；统计只读取 SQLite。 */
  chunk_count: number
}

export interface ServerInfo {
  username: string
  api_base: string
  official_api: boolean
  chunk_size: number
  /** Telegram 存储是否已配置(未配置时上传下载不可用) */
  configured: boolean
  files: number
  dirs: number
  total_size: number
}

export interface SearchHit {
  path: string
  name: string
  is_dir: boolean
  size: number
  created_at: number
  modified_at: number
  favorite: boolean
}

export interface SearchParams {
  q?: string
  kind?: 'all' | 'file' | 'dir'
  /** 逗号分隔且不带前导点的后缀。 */
  extensions?: string
  min_size?: number
  max_size?: number
  modified_after?: number
  modified_before?: number
  favorite?: boolean
  /** 只在该目录子树内搜索(绝对路径);缺省或 '/' 表示整库。 */
  under?: string
  sort?: 'name' | 'size' | 'modified' | 'created'
  order?: 'asc' | 'desc'
  limit?: number
}

export interface UploadSession {
  id: string
  path: string
  name: string
  size: number
  sha256: string
  chunk_size: number
  part_count: number
  uploaded_parts: number[]
  uploaded_bytes: number
  created_at: number
  updated_at: number
}

export interface UploadSessionResponse {
  session: UploadSession
  /** 单块会话为所有 Bot 的总并发；多分块会话固定返回 1。 */
  concurrency: number
  bot_count: number
  /** 当前会话所属 Bot 的 429 冷却剩余秒数。 */
  retry_after: number
}

export interface TrashItem {
  id: number
  name: string
  is_dir: boolean
  size: number
  orig_path: string
  trashed_at: number
}

export interface ShareItem {
  id: number
  token: string
  path: string
  name: string
  is_dir: boolean
  password: string | null
  expires_at: number | null
  max_downloads: number | null
  downloads: number
  created_at: number
}

export interface ShareMeta {
  name: string
  is_dir: boolean
  size: number
  need_password: boolean
  expires_at: number | null
  downloads: number
  max_downloads: number | null
}

export interface InboxRouteRule {
  /** 后台显示的类型名称。 */
  name: string
  /** 不带点的文件后缀；支持 tar.gz 这类多段后缀。 */
  extensions: string[]
  dir: string
}

/** 维护类可调项(后台「维护」「Bot 收件」区块) */
export interface Extras {
  trash_retention_days: number
  backup_auto: boolean
  /** SQLite 快照上传到 Telegram 时的单块字节数。 */
  backup_chunk_size: number
  backup_telegram_enabled: boolean
  backup_webdav_enabled: boolean
  backup_webdav_url: string
  backup_webdav_username: string
  /** 已保存时为固定脱敏占位符。 */
  backup_webdav_password: string
  backup_s3_enabled: boolean
  backup_s3_endpoint: string
  backup_s3_region: string
  backup_s3_bucket: string
  backup_s3_prefix: string
  /** 已保存时为固定脱敏占位符。 */
  backup_s3_access_key: string
  /** 已保存时为固定脱敏占位符。 */
  backup_s3_secret_key: string
  /** 可选临时凭据；已保存时为固定脱敏占位符。 */
  backup_s3_session_token: string
  backup_s3_path_style: boolean
  /** 非空时备份先加密再上传；已保存时为固定脱敏占位符，清空保存即关闭加密。 */
  backup_encryption_password: string
  inbox_bot_enabled: boolean
  inbox_channel_enabled: boolean
  inbox_allowed_chats: string
  inbox_dir: string
  inbox_video_dir: string
  inbox_image_dir: string
  inbox_book_dir: string
  inbox_rules: InboxRouteRule[]
  /** 删除文件后自动清理 Telegram 消息;关掉则只入队,等后台手动执行 */
  auto_delete_message: boolean
}

export type ExtrasPatch = Omit<Partial<Extras>, 'backup_chunk_size'> & {
  /** 字符串形式支持 18MB、1GB 等后台输入。 */
  backup_chunk_size?: string
}

export interface BackupLastTimes {
  telegram: number | null
  webdav: number | null
  s3: number | null
}

export interface BackupDestinationOutcome {
  kind: 'telegram' | 'webdav' | 's3'
  label: string
  ok: boolean
  parts?: number
  error?: string
}

export interface BackupOutcome {
  ok: boolean
  name: string
  parts: number
  succeeded: number
  failed: number
  destinations: BackupDestinationOutcome[]
}

/** 可在后台修改的配置项键名 */
export type CfgKey =
  | 'api_base'
  | 'proxy_enabled'
  | 'proxy_url'
  | 'bot_token'
  | 'upload_bot_tokens'
  | 'chat_id'
  | 'chunk_size'
  | 'upload_concurrency'
  | 'download_concurrency'
  | 'send_video'
  | 'tmp_dir'
  | 'username'
  | 'password'
  | 'telegram_api_id'
  | 'telegram_api_hash'

/** 每项的来源:后台改过 / 来自环境变量 / 内置默认 */
export type CfgSource = 'db' | 'env' | 'default'

export interface AdminConfig {
  values: {
    api_base: string
    proxy_enabled: boolean
    /** 代理 URL 中的用户名和密码均已脱敏。 */
    proxy_url: string
    bot_token: string
    /** 每行一个额外上传 Bot Token；返回值已逐项脱敏。 */
    upload_bot_tokens: string
    chat_id: string
    chunk_size: number
    upload_concurrency: number
    download_concurrency: number
    /** 单块以内的视频以「视频消息」发送,Telegram 里可直接播放 */
    send_video: boolean
    tmp_dir: string
    username: string
    password: string
    telegram_api_id: string
    telegram_api_hash: string
  }
  sources: Record<CfgKey, CfgSource>
  configured: boolean
  official_api: boolean
  official_api_base: string
  official_chunk_max: number
  official_chunk_default: number
  local_chunk_default: number
  readonly: Record<string, string>
}

export interface AdminStatus {
  version: string
  bot: string | null
  configured: boolean
  api_base: string
  official_api: boolean
  chunk_size: number
  upload_concurrency: number
  upload_bot_count: number
  download_concurrency: number
  files: number
  dirs: number
  total_size: number
  db_size: number
  db_path: string
  listen: string
  started_at: number
  now: number
  pending_deletes: number
  delete_queue: QueueOverview
  auto_delete_message: boolean
  inbox_failures: number
  tasks: TaskStatus[]
}

/** 后台队列概况:待处理量与卡住的原因 */
export interface QueueOverview {
  pending: number
  /** 已失败重试过的任务数 */
  retrying: number
  /** 重试次数最多那条试了几次 */
  max_attempts: number
  /** 重试次数最多那条的错误,通常就是卡住整个队列的根因 */
  last_error: string | null
}

/** 手动执行一轮队列的结果 */
export interface QueueRunSummary {
  completed: number
  failed: number
  /** Telegram 已确认不可恢复，已退出队列的任务数 */
  discarded: number
  pending: number
  last_error: string | null
  /** 因超时提前收工;为真时再点一次可以接着处理 */
  stopped_early: boolean
}

export interface StorageMigrationSource {
  storage_target_id: number
  bot_id: string
  chat_id: string
  messages: number
  chunks: number
  bytes: number
}

export interface StorageMigrationStatus {
  status: 'idle' | 'running' | 'paused' | 'failed' | 'completed'
  running: boolean
  main_target_id: number
  main_bot_id: string
  total_messages: number
  migrated_messages: number
  remaining_messages: number
  total_chunks: number
  migrated_chunks: number
  remaining_chunks: number
  last_error: string | null
  started_at: number | null
  updated_at: number | null
  sources: StorageMigrationSource[]
}

export interface TaskStatus {
  name: string
  running: boolean
  started_at: number
  last_heartbeat: number
  restarts: number
  last_error: string | null
}

export interface InboxFailure {
  update_id: number
  attempts: number
  last_error: string
  failed_at: number
  update: unknown
}

export interface BackendLog {
  id: number
  level: 'debug' | 'info' | 'warn' | 'error' | string
  category: string
  message: string
  created_at: number
}

/** 日志切面:按全库(而非当前页)统计,用于筛选下拉与级别条数 */
export interface BackendLogFacets {
  categories: string[]
  level_counts: Record<string, number>
}

export interface BackendLogPage {
  items: BackendLog[]
  has_more: boolean
  facets: BackendLogFacets
}

/** 后台日志筛选;空串表示该维度不限制,发请求前会被剔除 */
export interface BackendLogQuery {
  limit?: number
  before?: number
  level?: string
  category?: string
  q?: string
}

/** 检测到的会话(最近向 bot 发过消息的频道/用户/群) */
export interface DetectedChat {
  id: number
  /** channel / private / group / supergroup / user */
  kind: string
  name: string
  username: string | null
  at: number
}

/** 保存配置的结果 */
export interface ApplyOutcome {
  changed: CfgKey[]
  account_changed: boolean
  /** 改了 telegram-bot-api 容器的凭据,需要重建那个容器 */
  needs_local_api_restart: boolean
  bot: string | null
}

/** 提交的改动;只带用户实际改过的字段 */
export type ConfigPatch = Partial<{
  api_base: string
  proxy_enabled: boolean
  proxy_url: string
  bot_token: string
  upload_bot_tokens: string
  chat_id: string
  chunk_size: string
  upload_concurrency: number
  send_video: boolean
  download_concurrency: number
  tmp_dir: string
  username: string
  password: string
  telegram_api_id: string
  telegram_api_hash: string
  force: boolean
}>

/** 后台「账号安全」状态 */
export interface SecurityStatus {
  totp_enabled: boolean
  recovery_remaining: number
  webdav_password_set: boolean
  /** 登录密码是否以 argon2 哈希保存;来自 .env 明文时为 false */
  password_hashed: boolean
}

export interface TotpSetup {
  secret: string
  uri: string
  /** 后端渲染的二维码 SVG,密钥不经过第三方服务 */
  qr_svg: string
}

/** 后端错误的机器码(JSON {code}) */
export function errCode(e: unknown): string | undefined {
  const any = e as { response?: { data?: { code?: string | null } } }
  return any.response?.data?.code ?? undefined
}

/** 后端返回的错误信息(JSON {error}) */
export function errMsg(e: unknown): string {
  const any = e as { response?: { data?: { error?: string } }; message?: string }
  return any.response?.data?.error ?? any.message ?? String(e)
}

/** 注销响应成功后立即清理本地工作，不等待登录页懒加载及路由动画。 */
async function endSession(path: string) {
  const response = await http.post(path)
  invalidateSession()
  return response
}

export const api = {
  apps: appApi,
  login: (username: string, password: string, totp?: string) =>
    http.post('/auth/login', { username, password, totp }),
  logout: () => endSession('/auth/logout'),
  me: () => http.get<{ username: string }>('/auth/me'),
  info: () => http.get<ServerInfo>('/info'),
  list: (path: string) =>
    http.get<{ path: string; entries: Entry[] }>('/fs/list', { params: { path } }),
  entries: (params: EntriesQuery, signal?: AbortSignal) =>
    http.get<EntriesResponse>('/fs/entries', { params, signal }),
  properties: (path: string) =>
    http.get<{ properties: EntryProperties }>('/fs/properties', { params: { path } }),
  mkdir: (path: string, signal?: AbortSignal) => http.post('/fs/mkdir', { path }, { signal }),
  rename: (from: string, to: string) => http.post('/fs/rename', { from, to }),
  copy: (from: string, to: string) => http.post('/fs/copy', { from, to }),
  del: (paths: string[], permanent = false) =>
    http.post<{ deleted: number; failed: { path: string; error: string }[] }>('/fs/delete', {
      paths,
      permanent,
    }),
  upload: (
    dir: string,
    file: File,
    thumbnail: Blob | null | undefined,
    onProgress: (pct: number) => void,
    signal?: AbortSignal,
  ) => {
    const form = new FormData()
    // 缩略图必须先到达后端，才能和第一个视频分块放进同一条 Telegram 消息。
    if (thumbnail) form.append('thumbnail', thumbnail, 'thumbnail.jpg')
    form.append('file', file, file.name)
    return http.post<{ saved: Entry[] }>('/fs/upload', form, {
      params: { path: dir },
      signal,
      onUploadProgress: (ev) => {
        if (ev.total) onProgress(Math.round((ev.loaded * 100) / ev.total))
      },
    })
  },
  downloadUrl: (path: string, inline = false) =>
    `/api/fs/download?path=${encodeURIComponent(path)}${inline ? '&inline=1' : ''}`,
  /** 只请求 Telegram 小图，不会读取原视频。 */
  thumbnailUrl: (path: string) => `/api/fs/thumbnail?path=${encodeURIComponent(path)}`,
  /** 上传前批量查重:返回目录下已存在的名字 */
  exists: (dir: string, names: string[], signal?: AbortSignal) =>
    http.post<{ existing: string[] }>('/fs/exists', { dir, names }, { signal }),

  search: (params: SearchParams | string) =>
    http.get<{ results: SearchHit[] }>('/fs/search', {
      params: typeof params === 'string' ? { q: params, limit: 200 } : params,
    }),
  favorite: (path: string, favorite: boolean) =>
    http.post<{ ok: boolean; favorite: boolean }>('/fs/favorite', { path, favorite }),
  /** 秒传探测:库里有同内容文件则零流量落位 */
  instant: (dir: string, name: string, sha256: string, size: number, signal?: AbortSignal) =>
    http.post<{ linked: boolean; pending: boolean; entry?: Entry }>('/fs/instant', {
      dir,
      name,
      sha256,
      size,
    }, { signal }),

  uploads: {
    list: (signal?: AbortSignal) =>
      http.get<{
        sessions: UploadSession[]
        concurrency: number
        bot_count: number
        retry_after: number
      }>('/uploads', { signal }),
    status: (id: string, signal?: AbortSignal) => http.get<UploadSessionResponse>(`/uploads/${id}`, { signal }),
    create: (dir: string, name: string, size: number, sha256: string, signal?: AbortSignal) =>
      http.post<UploadSessionResponse>('/uploads', { dir, name, size, sha256 }, { signal }),
    thumbnail: (id: string, image: Blob, signal?: AbortSignal) =>
      http.post<{ ok: boolean }>(`/uploads/${id}/thumbnail`, image, {
        headers: { 'Content-Type': image.type || 'image/jpeg' },
        signal,
      }),
    part: (
      id: string,
      idx: number,
      data: Blob,
      onProgress: (loaded: number) => void,
      signal?: AbortSignal,
    ) =>
      http.put<{ already_uploaded: boolean; session: UploadSession }>(
        `/uploads/${id}/parts/${idx}`,
        data,
        {
          headers: { 'Content-Type': 'application/octet-stream' },
          signal,
          onUploadProgress: (event) => onProgress(event.loaded),
        },
      ),
    complete: (id: string, signal?: AbortSignal) =>
      http.post<{ ok: boolean; entry: Entry }>(`/uploads/${id}/complete`, undefined, { signal }),
    cancel: (id: string, signal?: AbortSignal) => http.delete<{ ok: boolean }>(`/uploads/${id}`, { signal }),
  },

  trash: {
    list: () => http.get<{ items: TrashItem[] }>('/trash'),
    restore: (ids: number[]) =>
      http.post<{ restored: { id: number; path: string }[]; failed: { id: number; error: string }[] }>(
        '/trash/restore',
        { ids },
      ),
    purge: (ids: number[], all = false) =>
      http.post<{ purged: number }>('/trash/purge', { ids, all }),
  },

  shares: {
    list: () => http.get<{ items: ShareItem[] }>('/shares'),
    create: (body: {
      path: string
      password?: string
      expires_hours?: number
      max_downloads?: number
    }) => http.post<{ share: { token: string }; name: string; is_dir: boolean }>('/shares', body),
    remove: (id: number) => http.delete('/shares/' + id),
  },

  /** 分享的公开访问(免登录) */
  share: {
    meta: (token: string) => http.get<ShareMeta>(`/share/${token}`),
    authorize: (token: string, password?: string) =>
      http.post<{ expires_at: number }>(`/share/${token}/authorize`, {
        password,
      }),
    list: (token: string, path: string) =>
      http.get<{ entries: Entry[] }>(`/share/${token}/list`, { params: { path } }),
    downloadUrl: (token: string, path = '', inline = false) => {
      const p = new URLSearchParams()
      if (path) p.set('path', path)
      if (inline) p.set('inline', '1')
      const qs = p.toString()
      return `/api/share/${token}/download${qs ? '?' + qs : ''}`
    },
  },

  admin: {
    config: () => http.get<AdminConfig>('/admin/config'),
    save: (patch: ConfigPatch) => http.post<ApplyOutcome>('/admin/config', patch),
    reset: (keys: CfgKey[]) => http.post<ApplyOutcome>('/admin/config/reset', { keys }),
    test: (probe: { api_base?: string; bot_token?: string; chat_id?: string; proxy_enabled?: boolean; proxy_url?: string } = {}) =>
      http.post<{ ok: boolean; bot: string; api_base: string }>('/admin/test', probe),
    /** 根据 Bot Token 检测最近向 bot 发过消息的频道/用户,辅助填 chat_id */
    detectChats: (probe: { api_base?: string; bot_token?: string; proxy_enabled?: boolean; proxy_url?: string } = {}) =>
      http.post<{ bot: string; chats: DetectedChat[] }>('/admin/detect-chats', probe),
    status: () => http.get<AdminStatus>('/admin/status'),
    logs: (params: BackendLogQuery = {}) =>
      http.get<BackendLogPage>('/admin/logs', {
        // 空串会被后端当成有效筛选,这里先剔掉再发
        params: Object.fromEntries(
          Object.entries(params).filter(([, v]) => v !== '' && v !== undefined),
        ),
      }),
    inboxFailures: () =>
      http.get<{ items: InboxFailure[] }>('/admin/inbox-failures'),
    removeInboxFailure: (updateId: number) =>
      http.delete(`/admin/inbox-failures/${updateId}`),
    clearInboxFailures: () =>
      http.delete<{ cleared: number }>('/admin/inbox-failures'),
    cleanup: () => http.post<{ cleaned: number }>('/admin/cleanup'),
    runDeleteQueue: () => http.post<QueueRunSummary>('/admin/queues/delete/run'),
    clearDeleteQueue: () => http.delete<{ cleared: number }>('/admin/queues/delete'),
    storageMigration: () => http.get<StorageMigrationStatus>('/admin/storage-migration'),
    startStorageMigration: () =>
      http.post<StorageMigrationStatus>('/admin/storage-migration/start'),
    pauseStorageMigration: () =>
      http.post<StorageMigrationStatus>('/admin/storage-migration/pause'),
    logoutAll: () => endSession('/admin/logout-all'),
    security: {
      status: () => http.get<SecurityStatus>('/admin/security'),
      totpSetup: () => http.post<TotpSetup>('/admin/security/totp/setup'),
      totpEnable: (code: string) =>
        http.post<{ recovery_codes: string[] }>('/admin/security/totp/enable', { code }),
      totpDisable: (code: string) => http.post('/admin/security/totp/disable', { code }),
      regenerateRecovery: (code: string) =>
        http.post<{ recovery_codes: string[] }>('/admin/security/totp/recovery', { code }),
      generateWebdavPassword: () =>
        http.post<{ password: string }>('/admin/security/webdav-password'),
      revokeWebdavPassword: () => http.delete('/admin/security/webdav-password'),
    },
    envExport: () => http.get<string>('/admin/env-export', { responseType: 'text' }),
    backupUrl: '/api/admin/backup',
    backupNow: () => http.post<BackupOutcome>('/admin/backup-now'),
    extras: () =>
      http.get<{
        extras: Extras
        backup_last_ts: number | null
        backup_last_by_target: BackupLastTimes
        trash_count: number
      }>('/admin/extras'),
    saveExtras: (patch: ExtrasPatch) => http.post('/admin/extras', patch),
  },
}

/** 秒数 → "3 天 4 小时" */
export function fmtDuration(sec: number): string {
  const d = Math.floor(sec / 86400)
  const h = Math.floor((sec % 86400) / 3600)
  const m = Math.floor((sec % 3600) / 60)
  if (d > 0) return `${d} 天 ${h} 小时`
  if (h > 0) return `${h} 小时 ${m} 分`
  return `${m} 分 ${sec % 60} 秒`
}

export function fmtSize(n: number): string {
  if (n === 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log2(Math.max(n, 1)) / 10), units.length - 1)
  const v = n / 2 ** (10 * i)
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`
}

export function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString('zh-CN', { hour12: false })
}

/** 浏览器 <video> 大概率能解码的容器(mkv 在 Chrome/Edge 可播 H.264/VP9 轨) */
export const VIDEO_EXT = ['mp4', 'webm', 'mov', 'm4v', 'mkv', 'ogv']

export function isVideo(name: string): boolean {
  return VIDEO_EXT.includes(name.split('.').pop()?.toLowerCase() ?? '')
}
