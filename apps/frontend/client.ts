import { http } from '@drive/http'
import type { AppIndex, AppLaunch, AppPreview, InstalledApp, SettingValues } from './types'

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
  packageUrl: (id: string) => `/api/apps/catalog/${encodeURIComponent(id)}/package`,
  state: (id: string, patch: { enabled?: boolean; pinned?: boolean }) =>
    http.post<InstalledApp>(`/apps/${encodeURIComponent(id)}/state`, patch),
  uninstall: (id: string, purgeData: boolean) =>
    http.delete(`/apps/${encodeURIComponent(id)}`, { params: { purge_data: purgeData } }),
  settings: (id: string) => http.get<SettingValues>(`/apps/${encodeURIComponent(id)}/settings`),
  saveSettings: (id: string, values: SettingValues) => http.patch<SettingValues>(`/apps/${encodeURIComponent(id)}/settings`, values),
  launch: (id: string, signal?: AbortSignal) => http.post<AppLaunch>(`/apps/${encodeURIComponent(id)}/launch`, {}, { signal }),
  rpc: (id: string, session: string, method: string, params: unknown = {}, signal?: AbortSignal) =>
    http.post(`/apps/${encodeURIComponent(id)}/rpc`, { session, method, params }, { signal }),
}
