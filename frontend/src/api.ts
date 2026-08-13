import axios from 'axios'
import { router } from './router'

export interface Entry {
  name: string
  is_dir: boolean
  size: number
  created_at: number
  modified_at: number
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
  modified_at: number
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

/** 维护类可调项(后台「维护」「Bot 收件」区块) */
export interface Extras {
  trash_retention_days: number
  backup_auto: boolean
  inbox_enabled: boolean
  inbox_allowed_chats: string
  inbox_dir: string
}

/** 可在后台修改的配置项键名 */
export type CfgKey =
  | 'api_base'
  | 'bot_token'
  | 'chat_id'
  | 'chunk_size'
  | 'upload_concurrency'
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
  mkdir: (path: string) => http.post('/fs/mkdir', { path }),
  rename: (from: string, to: string) => http.post('/fs/rename', { from, to }),
  copy: (from: string, to: string) => http.post('/fs/copy', { from, to }),
  del: (paths: string[]) =>
    http.post<{ deleted: number; failed: { path: string; error: string }[] }>('/fs/delete', {
      paths,
    }),
  upload: (dir: string, file: File, onProgress: (pct: number) => void, signal?: AbortSignal) => {
    const form = new FormData()
    form.append('file', file, file.name)
    return http.post('/fs/upload', form, {
      params: { path: dir },
      signal,
      onUploadProgress: (ev) => {
        if (ev.total) onProgress(Math.round((ev.loaded * 100) / ev.total))
      },
    })
  },
  downloadUrl: (path: string, inline = false) =>
    `/api/fs/download?path=${encodeURIComponent(path)}${inline ? '&inline=1' : ''}`,

  search: (q: string) => http.get<{ results: SearchHit[] }>('/fs/search', { params: { q } }),
  /** 秒传探测:库里有同内容文件则零流量落位 */
  instant: (dir: string, name: string, sha256: string, size: number) =>
    http.post<{ linked: boolean; entry?: Entry }>('/fs/instant', { dir, name, sha256, size }),

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
    list: (token: string, path: string, pwd?: string) =>
      http.get<{ entries: Entry[] }>(`/share/${token}/list`, { params: { path, pwd } }),
    downloadUrl: (token: string, path = '', pwd = '', inline = false) => {
      const p = new URLSearchParams()
      if (path) p.set('path', path)
      if (pwd) p.set('pwd', pwd)
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
    status: () => http.get<AdminStatus>('/admin/status'),
    cleanup: () => http.post<{ cleaned: number }>('/admin/cleanup'),
    logoutAll: () => http.post('/admin/logout-all'),
    envExport: () => http.get<string>('/admin/env-export', { responseType: 'text' }),
    backupUrl: '/api/admin/backup',
    backupToChannel: () =>
      http.post<{ ok: boolean; name: string }>('/admin/backup-to-channel'),
    extras: () =>
      http.get<{ extras: Extras; backup_last_ts: number | null; trash_count: number }>(
        '/admin/extras',
      ),
    saveExtras: (patch: Partial<Extras>) => http.post('/admin/extras', patch),
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
