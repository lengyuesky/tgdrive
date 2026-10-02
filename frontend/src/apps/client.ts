import { http } from '@drive/http'
import type { AppFile, AppHealth, ScopeMode, AppIndex, AppLaunch, AppPreview, CoverStats, InstalledApp, SettingValues } from './types'

export const appApi = {
  list: () => http.get<AppIndex>('/apps'),
  get: (id: string) => http.get<InstalledApp>(`/apps/${encodeURIComponent(id)}`),
  inspect: (file: File) => http.post<AppPreview>('/apps/inspect', file, { headers: { 'Content-Type': 'application/octet-stream' } }),
  install: (file: File, digest: string, expected_revision?: string) =>
    http.post<InstalledApp>('/apps/install', file, {
      params: { digest, expected_revision }, headers: { 'Content-Type': 'application/octet-stream' },
    }),
  installCatalog: (id: string, digest: string, expected_revision?: string) =>
    http.post<InstalledApp>(`/apps/catalog/${encodeURIComponent(id)}/install`, { digest, expected_revision }),
  refreshCatalog: (force = false) => http.post<AppIndex>('/apps/catalog/refresh', { force }),
  packageUrl: (id: string, digest?: string) =>
    `/api/apps/catalog/${encodeURIComponent(id)}/package${digest ? `?digest=${encodeURIComponent(digest)}` : ''}`,
  state: (id: string, patch: { enabled?: boolean; pinned?: boolean }) =>
    http.post<InstalledApp>(`/apps/${encodeURIComponent(id)}/state`, patch),
  uninstall: (id: string, purgeData: boolean) =>
    http.delete(`/apps/${encodeURIComponent(id)}`, { params: { purge_data: purgeData } }),
  settings: (id: string) => http.get<SettingValues>(`/apps/${encodeURIComponent(id)}/settings`),
  saveSettings: (id: string, values: SettingValues) => http.patch<SettingValues>(`/apps/${encodeURIComponent(id)}/settings`, values),
  saveScope: (id: string, paths: string[], mode?: ScopeMode) => http.patch<InstalledApp>(`/apps/${encodeURIComponent(id)}/scope`, { paths, mode }),
  authorize: (id: string, path: string, expected_revision: string) => http.post<InstalledApp>(`/apps/${encodeURIComponent(id)}/authorize`, { path, expected_revision }),
  open: (id: string, node: number | string) => http.post<AppFile>(`/apps/${encodeURIComponent(id)}/open`, typeof node === 'number' ? { id: node } : { path: node }),
  health: (id: string) => http.get<AppHealth>(`/apps/${encodeURIComponent(id)}/health`),
  report: (id: string, revision: string, phase: string, message = '') => http.post(`/apps/${encodeURIComponent(id)}/health`, { revision, phase, message }),
  recover: (id: string, expected_revision: string) => http.post<InstalledApp>(`/apps/${encodeURIComponent(id)}/recovery`, { expected_revision }),
  coverStats: (id: string) => http.get<CoverStats>(`/apps/${encodeURIComponent(id)}/covers`),
  clearCovers: (id: string) => http.delete<{ ok: boolean; removed: number }>(`/apps/${encodeURIComponent(id)}/covers`),
  launch: (id: string, signal?: AbortSignal) => http.post<AppLaunch>(`/apps/${encodeURIComponent(id)}/launch`, {}, { signal }),
  rpc: (id: string, session: string, method: string, params: unknown = {}, signal?: AbortSignal) =>
    http.post(`/apps/${encodeURIComponent(id)}/rpc`, { session, method, params }, { signal }),
}
