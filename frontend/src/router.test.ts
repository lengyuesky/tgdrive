import { describe, expect, it } from 'vitest'
import { router } from './router'
import manifestSource from '../public/manifest.webmanifest?raw'

describe('宿主地址边界', () => {
  it.each(['/files/', '/login', '/settings', '/transfers', '/me', '/s/token'])('%s 的浏览器地址位于 /admin', (path) => {
    expect(router.options.history.base).toBe('/admin')
    expect(router.resolve(path).href).toBe(`/admin${path}`)
    expect(router.resolve(path).matched.at(-1)?.path).not.toBe('/:catchall(.*)*')
  })

  it('系统设置使用独立路径，不保留旧设置与应用路由', () => {
    const paths = router.getRoutes().map((route) => route.path)
    expect(paths).toContain('/settings')
    expect(paths).not.toContain('/admin')
    expect(paths.some((path) => path.startsWith('/apps') || path === '/shorts')).toBe(false)
  })

  it('主屏幕入口、范围与图标不会请求根地址', () => {
    const manifest = JSON.parse(manifestSource)
    expect(manifest.id).toBe('/admin/')
    expect(manifest.scope).toBe('/admin/')
    expect(manifest.start_url).toBe('/admin/files/')
    expect(manifest.icons.every((icon: { src: string }) => icon.src.startsWith('/admin/'))).toBe(true)
  })
})
