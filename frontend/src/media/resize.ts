const EDGE = 512

/** 只读取有界头部；未知格式交给串行 Image 兼容路径，不猜测宽高。 */
async function dimensions(blob: Blob): Promise<[number, number] | undefined> {
  const data = new DataView(await blob.slice(0, 256 * 1024).arrayBuffer())
  const n = data.byteLength
  if (n < 12) return
  const u32 = (i: number) => data.getUint32(i)
  if (n >= 24 && u32(0) === 0x89504e47 && u32(12) === 0x49484452) return [u32(16), u32(20)]
  if (u32(0) === 0x47494638) return [data.getUint16(6, true), data.getUint16(8, true)]
  if (n >= 26 && data.getUint16(0) === 0x424d) return [Math.abs(data.getInt32(18, true)), Math.abs(data.getInt32(22, true))]
  if (n >= 30 && u32(0) === 0x52494646 && u32(8) === 0x57454250) {
    if (u32(12) === 0x56503858) {
      const u24 = (i: number) => data.getUint8(i) + (data.getUint8(i + 1) << 8) + (data.getUint8(i + 2) << 16)
      return [u24(24) + 1, u24(27) + 1]
    }
    if (u32(12) === 0x56503820 && data.getUint8(23) === 0x9d && data.getUint16(24) === 0x012a) return [data.getUint16(26, true) & 0x3fff, data.getUint16(28, true) & 0x3fff]
    if (u32(12) === 0x5650384c && data.getUint8(20) === 0x2f) {
      const bits = data.getUint32(21, true)
      return [(bits & 0x3fff) + 1, ((bits >>> 14) & 0x3fff) + 1]
    }
  }
  if (data.getUint16(0) === 0xffd8) {
    let i = 2
    while (i + 8 < n) {
      if (data.getUint8(i++) !== 0xff) return
      while (i < n && data.getUint8(i) === 0xff) i++
      if (i + 7 >= n) return
      const marker = data.getUint8(i++)
      if (marker === 0xda || marker === 0xd9) return
      const length = data.getUint16(i)
      if (length < 2) return
      if ([0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker)) return [data.getUint16(i + 5), data.getUint16(i + 3)]
      i += length
    }
  }
}

function targetSize(width: number, height: number): [number, number] {
  if (!Number.isFinite(width) || !Number.isFinite(height) || width < 1 || height < 1) throw new Error('图片尺寸无效')
  const ratio = Math.min(1, EDGE / Math.max(width, height))
  return [Math.max(1, Math.round(width * ratio)), Math.max(1, Math.round(height * ratio))]
}

function abort(signal: AbortSignal) { signal.throwIfAborted() }

async function canvasBlob(source: CanvasImageSource, width: number, height: number, signal: AbortSignal) {
  abort(signal)
  const canvas = document.createElement('canvas')
  ;[canvas.width, canvas.height] = targetSize(width, height)
  try {
    const context = canvas.getContext('2d')
    if (!context) throw new Error('浏览器不支持图片缩小')
    context.drawImage(source, 0, 0, canvas.width, canvas.height)
    const blob = await new Promise<Blob>((resolve, reject) => canvas.toBlob(
      (result) => result ? resolve(result) : reject(new Error('图片编码失败')), 'image/webp', 0.82,
    ))
    abort(signal)
    return blob
  } finally {
    canvas.width = canvas.height = 0
  }
}

/** 调用方持有唯一解码槽，包含兼容解码、canvas 编码及资源清理全过程。 */
export async function resizeMedia(blob: Blob, signal: AbortSignal): Promise<Blob> {
  abort(signal)
  const size = await dimensions(blob)
  abort(signal)
  if (size && typeof createImageBitmap === 'function') {
    const [resizeWidth, resizeHeight] = targetSize(...size)
    let bitmap: ImageBitmap | undefined
    try {
      bitmap = await createImageBitmap(blob, { resizeWidth, resizeHeight, resizeQuality: 'high' })
      return await canvasBlob(bitmap, bitmap.width, bitmap.height, signal)
    } catch (error) {
      if (signal.aborted) throw error
      // 部分浏览器不支持特定编码或 resize 选项，仍在同一个串行槽内回退。
    } finally { bitmap?.close() }
  }
  const url = URL.createObjectURL(blob)
  const image = new Image()
  const cancel = () => { image.src = '' }
  try {
    await new Promise<void>((resolve, reject) => {
      image.onload = () => resolve()
      image.onerror = () => reject(new Error('图片解码失败'))
      signal.addEventListener('abort', onAbort, { once: true })
      function onAbort() { cancel(); reject(signal.reason) }
      const cleanup = () => signal.removeEventListener('abort', onAbort)
      image.onload = () => { cleanup(); resolve() }
      image.onerror = () => { cleanup(); reject(new Error('图片解码失败')) }
      image.src = url
      if (signal.aborted) { cleanup(); onAbort() }
    })
    return await canvasBlob(image, image.naturalWidth, image.naturalHeight, signal)
  } finally {
    image.onload = image.onerror = null
    cancel()
    URL.revokeObjectURL(url)
  }
}
