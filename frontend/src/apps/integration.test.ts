import { describe, expect, it } from 'vitest'
import { acceptsFile, safeDirectory } from './integration'
import type { AppManifest } from './types'
describe('文件应用入口', () => {
  const manifest = { permissions: ['files.read'], integration: { file_types: ['epub'], directories: true } } as AppManifest
  it('只匹配声明的类型，目录独立处理，权限仍是接入前提', () => {
    expect(acceptsFile(manifest, { name: '书.EPUB', is_dir: false })).toBe(true)
    expect(acceptsFile(manifest, { name: '书.pdf', is_dir: false })).toBe(false)
    expect(acceptsFile(manifest, { name: '漫画', is_dir: true })).toBe(true)
    expect(acceptsFile({ ...manifest, permissions: [] }, { name: '书.epub', is_dir: false })).toBe(false)
  })
  it('导航不接受协议相对地址和控制字符', () => {
    expect(safeDirectory('//example.com')).toBe('/')
    expect(safeDirectory('/书\n')).toBe('/')
    expect(safeDirectory('/书/小说')).toBe('/书/小说')
  })
})
