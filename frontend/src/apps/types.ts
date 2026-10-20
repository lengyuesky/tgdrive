export type SettingValue = string | number | boolean
export type SettingValues = Record<string, SettingValue>

export interface AppSetting {
  key: string
  label: string
  description: string
  type: 'string' | 'number' | 'boolean' | 'directory'
  default: SettingValue
}

export interface AppIntegration {
  file_types: string[]; directories: boolean; changelog: string; formats: string[]; limitations: string[]
  homepage: string; support: string; screenshots: string[]; data_schema: number
}
export interface AppFile { id: number; content_version: string; path: string; name: string; is_dir: boolean; size: number; created_at: number; modified_at: number; favorite: boolean }
export interface AppHealth { lifecycle: { schema_version: number; phase: string; message: string; updated_at: number } | null; recovery: { version: string; created_at: number; bytes: number } | null }
export type ScopeMode = 'none' | 'all' | 'selected'
export interface AppManifest {
  id: string
  name: string
  version: string
  api_version: number
  min_host_version: string
  description: string
  author: string
  entry: string
  /** 包内可选 SVG 图标路径；安装后由宿主展示。 */
  icon?: string
  permissions: string[]
  integration?: AppIntegration
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
  /** 目录范围；空数组表示全盘。 */
  scope: string[]
  scope_mode?: ScopeMode
}

/** 应用封面缓存占用；封面存于独立封面库，可随时清理后重新生成。 */
export interface CoverStats {
  entries: number
  bytes: number
  limit_bytes: number
  limit_entries: number
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
  source_url?: string | null
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
  /** 服务端判定的旧应用及卸载后残留数据；需用户明确同意替换。 */
  replacements: { id: string; name: string; installed: boolean; has_data: boolean }[]
  /** 服务端预览范围指纹，安装时原样回传；前端不自行构造。 */
  replacement_revision?: string | null
}

export interface AppLaunch {
  files_cursor?: string
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

/** 目录范围判定：空范围即全盘；否则路径需等于或位于某个根之下。 */
export function pathInScope(scope: string[] | undefined, path: string): boolean {
  if (!scope?.length) return true
  return scope.some((root) => {
    const trimmed = root.endsWith('/') && root !== '/' ? root.slice(0, -1) : root
    return trimmed === '/' || path === trimmed || path.startsWith(`${trimmed}/`)
  })
}

/**
 * 文件事件是否与目录范围相关：无范围或范围未知时一律相关；否则任一变更目录落在某个根之下，
 * 或者是某个根的祖先（根本身可能被新增、改名或删除）。
 */
export function filesChangeAffectsScope(scope: string[] | undefined, paths: string[] | undefined): boolean {
  if (!paths) return true
  if (!paths.length) return false // 空列表表示可见树没有变化
  if (!scope?.length) return true
  return paths.some((path) => scope.some((root) => {
    const trimmed = root.endsWith('/') && root !== '/' ? root.slice(0, -1) : root
    return trimmed === '/' || path === '/' || path === trimmed || path.startsWith(`${trimmed}/`) || trimmed.startsWith(`${path}/`)
  }))
}

/** 通知当前页面和其他标签页刷新应用入口、检查运行状态。 */
export function notifyAppsChanged() {
  window.dispatchEvent(new Event(APPS_CHANGED))
  try { localStorage.setItem(APPS_CHANGED, `${Date.now()}-${Math.random()}`) } catch { /* 隐私模式仍可通知当前页面。 */ }
}
