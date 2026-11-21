import { Blob as NodeBlob } from 'node:buffer'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resizeMedia } from './resize'

function png(width: number, height: number) {
  const bytes = new Uint8Array(24); const view = new DataView(bytes.buffer)
  view.setUint32(0, 0x89504e47); view.setUint32(12, 0x49484452)
  view.setUint32(16, width); view.setUint32(20, height)
  return new Blob([bytes], { type: 'image/png' })
}
let draw: ReturnType<typeof vi.fn>
let createURL: ReturnType<typeof vi.fn>
let revokeURL: ReturnType<typeof vi.fn>
beforeEach(() => {
  vi.stubGlobal('Blob', NodeBlob)
  draw = vi.fn()
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({ drawImage: draw } as unknown as CanvasRenderingContext2D)
  vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation(function (callback) { callback(new Blob(['small'])) })
  createURL = vi.fn(() => 'blob:temporary'); revokeURL = vi.fn()
  vi.stubGlobal('URL', { createObjectURL: createURL, revokeObjectURL: revokeURL })
})
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks() })

describe('512px串行解码实现与释放', () => {
  it.each([[4000, 2000, 512, 256], [2000, 4000, 256, 512], [40, 20, 40, 20]])('bitmap保持比例 %i×%i → %i×%i，关闭位图并清空画布', async (w, h, tw, th) => {
    const bitmap = { width: tw, height: th, close: vi.fn() }
    const decode = vi.fn().mockResolvedValue(bitmap)
    vi.stubGlobal('createImageBitmap', decode)
    const blob = png(w, h)
    expect((await resizeMedia(blob, new AbortController().signal)).size).toBe(5)
    expect(decode).toHaveBeenCalledWith(blob, { resizeWidth: tw, resizeHeight: th, resizeQuality: 'high' })
    expect(draw).toHaveBeenCalledWith(bitmap, 0, 0, tw, th)
    expect(bitmap.close).toHaveBeenCalledTimes(1)
    const canvas = (HTMLCanvasElement.prototype.toBlob as ReturnType<typeof vi.fn>).mock.instances[0]!
    expect(canvas.width).toBe(0); expect(canvas.height).toBe(0)
    expect(createURL).not.toHaveBeenCalled()
  })

  it('不可中断的bitmap晚到仍close，不产生可复用结果', async () => {
    const controller = new AbortController()
    const bitmap = { width: 512, height: 256, close: vi.fn() }
    vi.stubGlobal('createImageBitmap', vi.fn(async () => { controller.abort(); return bitmap }))
    await expect(resizeMedia(png(1000, 500), controller.signal)).rejects.toBeDefined()
    expect(bitmap.close).toHaveBeenCalledTimes(1)
    expect(draw).not.toHaveBeenCalled()
  })

  it('无bitmap时Image兼容路径等比例缩小，清空src并revoke临时URL', async () => {
    const images: FakeImage[] = []
    class FakeImage {
      onload: (() => void) | null = null
      onerror: (() => void) | null = null
      naturalWidth = 1000; naturalHeight = 2000
      source = ''
      constructor() { images.push(this) }
      set src(value: string) { this.source = value; if (value) queueMicrotask(() => this.onload?.()) }
    }
    vi.stubGlobal('Image', FakeImage); vi.stubGlobal('createImageBitmap', undefined)
    await resizeMedia(new Blob(['unknown']), new AbortController().signal)
    expect(draw).toHaveBeenCalledWith(images[0], 0, 0, 256, 512)
    expect(images[0]!.source).toBe('')
    expect(revokeURL).toHaveBeenCalledWith('blob:temporary')
    expect(images[0]!.onload).toBeNull()
  })

  it('兼容Image等待时取消，revoke并断开事件和src', async () => {
    let image!: { onload: (() => void) | null; onerror: (() => void) | null; src: string }
    vi.stubGlobal('Image', class { onload = null; onerror = null; src = ''; constructor() { image = this } })
    vi.stubGlobal('createImageBitmap', undefined)
    const controller = new AbortController()
    const pending = resizeMedia(new Blob(['unknown']), controller.signal)
    await vi.waitFor(() => expect(createURL).toHaveBeenCalled())
    controller.abort()
    await expect(pending).rejects.toBeDefined()
    expect(image.src).toBe(''); expect(image.onload).toBeNull()
    expect(revokeURL).toHaveBeenCalledTimes(1)
  })
})
