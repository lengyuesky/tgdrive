import { shallowRef } from 'vue'
import { invalidateSession } from '../session'
import { ENDPOINTS, APP_CENTER_BASE, ASSET_BASE, type ApiError, type ErrorResponse, type InstalledApp, type InstalledAppsResponse,
  type InstallPreview, type InstallSource, type InstallRequest, type ScopeInfo, type ScopeUpdateRequest,
  type AppStatePatch, type CatalogResponse, type PackageUploadReceipt, type LaunchPageRequest,
  type LaunchPageResponse, type NodeReference, type FileNode, type DataUsage, type BackendStatus, type RootAppSelection, type RootAppUpdateRequest,
  type AppResourceLimits, type AppResourceLimitsUpdateRequest } from './contract'

export interface BackendLogs { stdout: string; stderr: string; truncated: boolean }

// 只有宿主自己的前端调用；不向应用 SDK 宣告，也不混入契约注册表完整性检查。
const INTERNAL_ENDPOINTS = [
  { name: 'center.nodes.stat', methods: ['POST'], path: `${APP_CENTER_BASE}/nodes/stat` },
  { name: 'center.app.usage', methods: ['GET'], path: `${APP_CENTER_BASE}/apps/{app_id}/data/usage` },
  { name: 'center.app.logs', methods: ['GET'], path: `${APP_CENTER_BASE}/apps/{app_id}/backend/logs` },
] as const
const definitionOf = (name: string) => ENDPOINTS.find((entry) => entry.name === name) ?? INTERNAL_ENDPOINTS.find((entry) => entry.name === name)

export class AppApiError extends Error {
  constructor(readonly error: ApiError) { super(error.message) }
  get code() { return this.error.code }
}
export function endpoint(name: string, parameters: Record<string, string> = {}) {
  const definition = definitionOf(name)
  if (!definition) throw new Error('宿主契约未提供所需接口')
  return definition.path.replace(/\{([^}]+)\}/g, (_, key: string) => {
    const value = parameters[key]
    if (value === undefined) throw new Error('接口缺少路径参数')
    return key === 'path' ? value.split('/').map(encodeURIComponent).join('/') : encodeURIComponent(value)
  })
}
async function owner<T>(name: string, parameters: Record<string, string> = {}, body?: unknown, init: RequestInit = {}, query = ''): Promise<T> {
  const definition = definitionOf(name)
  if (!definition) throw new Error('宿主契约未提供所需接口')
  const headers = new Headers(init.headers)
  if (body !== undefined && !(body instanceof Blob)) headers.set('Content-Type', 'application/json')
  const response = await fetch(endpoint(name, parameters) + query, {
    ...init, mode: 'same-origin', method: definition.methods[0], headers,
    body: body === undefined ? undefined : body instanceof Blob ? body : JSON.stringify(body),
    credentials: 'same-origin', redirect: 'error',
  })
  if (!response.ok) {
    if (response.status === 401) invalidateSession()
    const error = await response.json().catch(() => null) as ErrorResponse | null
    throw new AppApiError(error?.error ?? { code: 'internal_error', message: '应用中心请求失败', request_id: '' })
  }
  return (response.status === 204 ? undefined : await response.json()) as T
}

export const installedApps = shallowRef<InstalledApp[]>([])
export const rootSelection = shallowRef<RootAppSelection>()
let lastLoad = 0
let loading: Promise<InstalledApp[]> | undefined
export function loadInstalled(force = false) {
  if (!force && Date.now() - lastLoad < 5000) return Promise.resolve(installedApps.value)
  if (loading) return loading
  loading = owner<InstalledAppsResponse>('center.apps').then((response) => {
    if (!Array.isArray(response.apps)) throw new Error('安装列表响应无效')
    installedApps.value = response.apps; rootSelection.value = response.root_app; lastLoad = Date.now(); return response.apps
  }).finally(() => { loading = undefined })
  return loading
}
export function openWith(name: string, directory: boolean) {
  const folded = name.replace(/[A-Z]/g, (value) => value.toLowerCase())
  return installedApps.value.filter((app) => app.enabled && app.manifest.page && app.manifest.files.read &&
    (directory ? app.manifest.open_with.directories : app.manifest.open_with.extensions.some((extension) => folded.endsWith(`.${extension}`))))
}
export function iconUrl(app: InstalledApp) {
  return app.manifest.icon ? `${ASSET_BASE}/${encodeURIComponent(app.manifest.id)}/${app.package.sha256}/${app.manifest.icon}` : undefined
}
export function formatBytes(value: string) {
  const bytes = BigInt(value)
  for (const [unit, factor] of [['GiB', 1024n ** 3n], ['MiB', 1024n ** 2n], ['KiB', 1024n]] as const) {
    if (bytes >= factor) return `${bytes / factor}.${(bytes % factor) * 10n / factor} ${unit}`
  }
  return `${bytes} B`
}
export const center = {
  app: (id: string) => owner<InstalledApp>('center.app', { app_id: id }),
  catalog: () => owner<CatalogResponse>('center.catalog'),
  refresh: () => owner<CatalogResponse>('center.catalog.refresh', {}, { force: true }),
  upload: (file: File) => owner<PackageUploadReceipt>('center.package.upload', {}, file, { headers: { 'Content-Type': 'application/zip' } }),
  preview: (source: InstallSource) => owner<InstallPreview>('center.install.preview', {}, { source }),
  install: (body: InstallRequest) => owner<InstalledApp>('center.install', {}, body),
  state: (id: string, body: AppStatePatch) => owner<InstalledApp>('center.app.state', { app_id: id }, body),
  scope: (id: string, body: ScopeUpdateRequest) => owner<ScopeInfo>('center.app.scope', { app_id: id }, body),
  uninstall: (app: InstalledApp, purge: boolean) => owner<void>('center.app.uninstall', { app_id: app.manifest.id }, undefined,
    { headers: { 'If-Match': `"${app.state_revision}"` } }, purge ? '?purge_data=true' : ''),
  clearCache: (id: string) => owner<void>('center.app.cache.clear', { app_id: id }),
  restart: (id: string) => owner<BackendStatus>('center.app.restart', { app_id: id }),
  root: () => owner<RootAppSelection>('center.root.read'),
  setRoot: (body: RootAppUpdateRequest) => owner<RootAppSelection>('center.root.update', {}, body),
  limits: () => owner<AppResourceLimits>('center.limits.read'),
  setLimits: (body: AppResourceLimitsUpdateRequest) => owner<AppResourceLimits>('center.limits.update', {}, body),
  logs: (id: string) => owner<BackendLogs>('center.app.logs', { app_id: id }),
  launch: (id: string, body: LaunchPageRequest) => owner<LaunchPageResponse>('center.app.launch', { app_id: id }, body),
  node: (reference: NodeReference) => owner<FileNode>('center.nodes.stat', {}, reference),
}

// 宿主仅检查页面生命周期或关闭会话，不经此函数转发页面的数据请求。
export async function pageLifecycle<T>(token: string, name: 'runtime.info' | 'session.close') {
  const definition = definitionOf(name)!
  const response = await fetch(endpoint(name), { method: definition.methods[0], credentials: 'omit', redirect: 'error', headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) { const error = await response.json() as ErrorResponse; throw new AppApiError(error.error) }
  return (response.status === 204 ? undefined : await response.json()) as T
}
export function usage(app: InstalledApp): Promise<DataUsage> {
  return owner<DataUsage>('center.app.usage', { app_id: app.manifest.id })
}
