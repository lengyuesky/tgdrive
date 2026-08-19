import axios from 'axios'
import { router } from './router'

export interface Entry {
  name: string
  is_dir: boolean
  size: number
  created_at: number
  modified_at: number
  sha256: string | null
  favorite: boolean
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
  concurrency: number
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
  inbox_enabled: boolean
  inbox_allowed_chats: string
  inbox_dir: string
  inbox_video_dir: string
  inbox_image_dir: string
  inbox_book_dir: string
  inbox_rules: InboxRouteRule[]
}

export type ExtrasPatch = Omit<Partial<Extras>, 'backup_chunk_size'> & {
  /** 字符串形式支持 18MB、1GB 等后台输入。 */
  backup_chunk_size?: string
}

/** 可在后台修改的配置项键名 */
export type CfgKey =
  | 'api_base'
  | 'bot_token'
  | 'chat_id'
  | 'chunk_size'
  | 'upload_concurrency'
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
    bot_token: string
    chat_id: string
    chunk_size: number
    upload_concurrency: number
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
  files: number
  dirs: number
  total_size: number
  db_size: number
  db_path: string
  listen: string
  started_at: number
  now: number
  pending_deletes: number
  inbox_failures: number
  tasks: TaskStatus[]
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
  bot_token: string
  chat_id: string
  chunk_size: string
  upload_concurrency: number
  send_video: boolean
  tmp_dir: string
  username: string
  password: string
  telegram_api_id: string
  telegram_api_hash: string
  force: boolean
}>

const http = axios.create({ baseURL: '/api' })

// 任意接口 401 → 回登录页
http.interceptors.response.use(
  (r) => r,
  (err) => {
    if (err.response?.status === 401 && router.currentRoute.value.path !== '/login') {
      router.push('/login')
    }
    return Promise.reject(err)
  },
)

/** 后端返回的错误信息(JSON {error}) */
export function errMsg(e: unknown): string {
  const any = e as { response?: { data?: { error?: string } }; message?: string }
  return any.response?.data?.error ?? any.message ?? String(e)
}

export const api = {
  login: (username: string, password: string) =>
    http.post('/auth/login', { username, password }),
  logout: () => http.post('/auth/logout'),
  me: () => http.get<{ username: string }>('/auth/me'),
  info: () => http.get<ServerInfo>('/info'),
  list: (path: string) =>
    http.get<{ path: string; entries: Entry[] }>('/fs/list', { params: { path } }),
  properties: (path: string) =>
    http.get<{ properties: EntryProperties }>('/fs/properties', { params: { path } }),
  mkdir: (path: string) => http.post('/fs/mkdir', { path }),
  rename: (from: string, to: string) => http.post('/fs/rename', { from, to }),
  copy: (from: string, to: string) => http.post('/fs/copy', { from, to }),
  del: (paths: string[]) =>
    http.post<{ deleted: number; failed: { path: string; error: string }[] }>('/fs/delete', {
      paths,
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
  exists: (dir: string, names: string[]) =>
    http.post<{ existing: string[] }>('/fs/exists', { dir, names }),

  search: (params: SearchParams | string) =>
    http.get<{ results: SearchHit[] }>('/fs/search', {
      params: typeof params === 'string' ? { q: params, limit: 200 } : params,
    }),
  favorite: (path: string, favorite: boolean) =>
    http.post<{ ok: boolean; favorite: boolean }>('/fs/favorite', { path, favorite }),
  /** 秒传探测:库里有同内容文件则零流量落位 */
  instant: (dir: string, name: string, sha256: string, size: number) =>
    http.post<{ linked: boolean; pending: boolean; entry?: Entry }>('/fs/instant', {
      dir,
      name,
      sha256,
      size,
    }),

  uploads: {
    list: () =>
      http.get<{ sessions: UploadSession[]; concurrency: number }>('/uploads'),
    status: (id: string) => http.get<UploadSessionResponse>(`/uploads/${id}`),
    create: (dir: string, name: string, size: number, sha256: string) =>
      http.post<UploadSessionResponse>('/uploads', { dir, name, size, sha256 }),
    thumbnail: (id: string, image: Blob) =>
      http.post<{ ok: boolean }>(`/uploads/${id}/thumbnail`, image, {
        headers: { 'Content-Type': image.type || 'image/jpeg' },
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
    complete: (id: string) =>
      http.post<{ ok: boolean; entry: Entry }>(`/uploads/${id}/complete`),
    cancel: (id: string) => http.delete<{ ok: boolean }>(`/uploads/${id}`),
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
    test: (probe: { api_base?: string; bot_token?: string; chat_id?: string } = {}) =>
      http.post<{ ok: boolean; bot: string; api_base: string }>('/admin/test', probe),
    /** 根据 Bot Token 检测最近向 bot 发过消息的频道/用户,辅助填 chat_id */
    detectChats: (probe: { api_base?: string; bot_token?: string } = {}) =>
      http.post<{ bot: string; chats: DetectedChat[] }>('/admin/detect-chats', probe),
    status: () => http.get<AdminStatus>('/admin/status'),
    inboxFailures: () =>
      http.get<{ items: InboxFailure[] }>('/admin/inbox-failures'),
    removeInboxFailure: (updateId: number) =>
      http.delete(`/admin/inbox-failures/${updateId}`),
    clearInboxFailures: () =>
      http.delete<{ cleared: number }>('/admin/inbox-failures'),
    cleanup: () => http.post<{ cleaned: number }>('/admin/cleanup'),
    logoutAll: () => http.post('/admin/logout-all'),
    envExport: () => http.get<string>('/admin/env-export', { responseType: 'text' }),
    backupUrl: '/api/admin/backup',
    backupToChannel: () =>
      http.post<{ ok: boolean; name: string; parts: number }>('/admin/backup-to-channel'),
    extras: () =>
      http.get<{ extras: Extras; backup_last_ts: number | null; trash_count: number }>(
        '/admin/extras',
      ),
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
