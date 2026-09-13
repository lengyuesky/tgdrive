export type SettingValue = string | number | boolean
export type SettingValues = Record<string, SettingValue>

export interface AppSetting {
  key: string
  label: string
  description: string
  type: 'string' | 'number' | 'boolean' | 'directory'
  default: SettingValue
}

export interface AppManifest {
  id: string
  name: string
  version: string
  api_version: number
  min_host_version: string
  description: string
  author: string
  entry: string
  permissions: string[]
  settings: AppSetting[]
}

export interface InstalledApp {
  manifest: AppManifest
  digest: string
  revision: string
  enabled: boolean
  pinned: boolean
  source: 'catalog' | 'import'
  installed_at: number
  updated_at: number
}

export interface CatalogStatus {
  source: 'github' | 'local' | 'disabled'
  repository: string | null
  state: 'ready' | 'stale' | 'unavailable' | 'disabled'
  last_success_at: number | null
  error: string | null
}

export interface CatalogApp {
  manifest: AppManifest
  digest: string
  size: number
  source: 'github' | 'local'
  release_url?: string | null
  update_available: boolean
}

export interface AppIndex {
  installed: InstalledApp[]
  available: CatalogApp[]
  catalog?: CatalogStatus
  api_version: number
  supported_api_versions?: number[]
  host_version: string
}

export interface AppPreview {
  manifest: AppManifest
  digest: string
  existing: InstalledApp | null
  added_permissions: string[]
}

export interface AppLaunch {
  app: InstalledApp
  session: string
  frame_url: string
  expires_at: number
}

export const permissionLabels: Record<string, string> = {
  'files.read': '搜索文件与读取文件信息',
  'media.read': '读取文件内容（播放、缩略图和下载）',
  'favorites.write': '添加或取消收藏',
}

export const APPS_CHANGED = 'tgdrive-apps-changed'

/** 通知当前页面和其他标签页刷新应用入口、检查运行状态。 */
export function notifyAppsChanged() {
  window.dispatchEvent(new Event(APPS_CHANGED))
  try { localStorage.setItem(APPS_CHANGED, `${Date.now()}-${Math.random()}`) } catch { /* 隐私模式仍可通知当前页面。 */ }
}
