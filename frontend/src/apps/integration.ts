import type { AppManifest } from './types'
export function acceptsFile(manifest: AppManifest, file: { name: string; is_dir: boolean }): boolean {
  const info = manifest.integration
  if (!info || !manifest.permissions.includes('files.read')) return false
  return file.is_dir ? info.directories : info.file_types.includes(file.name.split('.').length > 1 ? file.name.split('.').at(-1)!.toLowerCase() : '')
}
/** 仅接受站内文件目录，拒绝协议 URL 与控制字符。 */
export function parentPath(path: string): string { return path.slice(0, path.lastIndexOf('/')) || '/' }
export function safeDirectory(value: unknown): string { return typeof value === 'string' && value.startsWith('/') && !value.startsWith('//') && !/[\u0000-\u001f\\]/.test(value) ? value : '/' }
