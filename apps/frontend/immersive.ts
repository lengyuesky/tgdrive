/** 沉浸只改变宿主布局；请求仍须经过会话检查，不授予浏览器全屏或同源权限。 */
export class HostImmersion {
  private sequence = 0
  private restore?: () => void
  constructor(private changed: (active: boolean, background?: string) => void) {}

  async request(params: Record<string, unknown>, apiVersion: number, verify: () => Promise<unknown>, signal: AbortSignal, painted: () => Promise<void>) {
    if (apiVersion < 2) throw new Error('此能力需要应用 API v2')
    if (Object.keys(params).some((key) => key !== 'active' && key !== 'background') || typeof params.active !== 'boolean') throw new Error('沉浸参数必须包含布尔值 active，只能附加 background')
    if ('background' in params && (typeof params.background !== 'string' || !/^#[0-9a-f]{6}$/i.test(params.background))) throw new Error('沉浸背景必须是六位十六进制颜色')
    const sequence = ++this.sequence
    try {
      await verify()
      signal.throwIfAborted()
      if (sequence !== this.sequence) return null
      this.apply(params.active, params.background as string | undefined)
      await painted()
      signal.throwIfAborted()
      return null
    } catch (error) {
      if (sequence === this.sequence) this.reset()
      throw error
    }
  }

  private apply(active: boolean, background?: string) {
    if (active && !this.restore) {
      let viewport = document.querySelector<HTMLMetaElement>('meta[name="viewport"]')
      const created = !viewport
      if (!viewport) {
        viewport = document.createElement('meta'); viewport.name = 'viewport'
        document.head.append(viewport)
      }
      const original = viewport.getAttribute('content')
      const content = (original ?? 'width=device-width, initial-scale=1').split(',').map((part) => part.trim()).filter((part) => part && !/^viewport-fit\s*=/i.test(part))
      const applied = [...content, 'viewport-fit=cover'].join(', ')
      // iOS 主屏幕窗口不应在每次阅读时重写相同 viewport，避免重新计算安全区。
      if (viewport.content !== applied) viewport.content = applied
      const styles = [document.documentElement.style, document.body.style].flatMap((style) => ['overflow-x', 'overflow-y'].map((key) => {
        const value = style.getPropertyValue(key), priority = style.getPropertyPriority(key)
        style.setProperty(key, 'hidden')
        return () => {
          if (style.getPropertyValue(key) !== 'hidden') return
          if (value) style.setProperty(key, value, priority)
          else style.removeProperty(key)
        }
      }))
      this.restore = () => {
        if (viewport!.content === applied) {
          if (created) viewport!.remove()
          else if (original === null) viewport!.removeAttribute('content')
          else if (viewport!.content !== original) viewport!.content = original
        }
        styles.forEach((restore) => restore())
      }
    } else if (!active) {
      this.restore?.(); this.restore = undefined
    }
    if (active && background !== undefined) this.changed(active, background)
    else this.changed(active)
  }

  reset() { this.sequence++; this.apply(false) }
}
