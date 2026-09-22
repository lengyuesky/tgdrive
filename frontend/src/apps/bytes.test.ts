import { describe, expect, it } from 'vitest'
import { assetPath, boundedBytes, mapLimit, rangeInput, rangesInput } from './bytes'

describe('有界文件读取', () => {
  const signal = new AbortController().signal
  const response = (body: string, range = 'bytes 2-4/10', status = 206) => new Response(body, { status, headers: { 'Content-Length': String(body.length), 'Content-Range': range } })
  it('验证精确范围、实际长度和 EOF 裁剪', async () => {
    expect([...await boundedBytes(response('234'), 1024, signal, { offset: 2, length: 3 })]).toEqual([50, 51, 52])
    expect((await boundedBytes(response('89', 'bytes 8-9/10'), 1024, signal, { offset: 8, length: 10 })).length).toBe(2)
    await expect(boundedBytes(response('234', 'bytes 2-4/10', 200), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow('整个文件')
    await expect(boundedBytes(response('234', 'bytes 3-5/10'), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow()
    await expect(boundedBytes(new Response('2345', { status: 206, headers: { 'Content-Length': '3', 'Content-Range': 'bytes 2-4/10' } }), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow('超过声明')
    await expect(boundedBytes(new Response('23', { status: 206, headers: { 'Content-Length': '3', 'Content-Range': 'bytes 2-4/10' } }), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow('提前结束')
    await expect(boundedBytes(new Response('234', { status: 206, headers: { 'Content-Range': 'bytes 2-4/10' } }), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow('整个文件')
  })
  it('包内资源按上限流式读取：压缩变体的 Content-Length 小于解码后长度，分块响应也可接受', async () => {
    // 浏览器透明解码 brotli/gzip 后暴露的 Content-Length 仍是压缩后大小。
    expect([...await boundedBytes(new Response('abcdef', { headers: { 'Content-Length': '3', 'Content-Encoding': 'br' } }), 1024, signal)]).toEqual([97, 98, 99, 100, 101, 102])
    expect((await boundedBytes(new Response('chunked'), 1024, signal)).length).toBe(7)
    const stream = new ReadableStream<Uint8Array>({ start(controller) { controller.enqueue(new Uint8Array([1, 2])); controller.enqueue(new Uint8Array([3])); controller.close() } })
    expect([...await boundedBytes(new Response(stream), 1024, signal)]).toEqual([1, 2, 3])
    await expect(boundedBytes(new Response('abcdef', { headers: { 'Content-Length': '2' } }), 4, signal)).rejects.toThrow('超过大小限制')
    await expect(boundedBytes(new Response('ab', { headers: { 'Content-Length': '9' } }), 4, signal)).rejects.toThrow('超过限制')
  })
  it('拒绝任意 URL、过大范围与资源路径穿越', () => {
    const ref = { id: 2, content_version: 'a'.repeat(64) }
    expect(rangeInput({ ref, offset: 0, length: 1024 }).ref).toEqual(ref)
    for (const length of [0, -1, 1048577, Infinity]) expect(() => rangeInput({ ref, offset: 0, length })).toThrow()
    expect(() => rangeInput({ ref: { path: 'https://example.invalid' }, offset: 0, length: 2 })).toThrow()
    for (const path of ['../api/admin', '/api/admin', 'https://bad', 'a/%2e%2e/b', 'a\\b', '.hidden']) expect(() => assetPath(path)).toThrow()
    expect(assetPath('fonts/FoxitSans.pfb')).toBe('fonts/FoxitSans.pfb')
  })
  it('有限并发映射保持顺序、不超并发上限，失败即整体拒绝', async () => {
    const finish: Array<() => void> = []
    let active = 0, peak = 0
    const worker = (item: number) => new Promise<number>((resolve) => {
      active++; peak = Math.max(peak, active)
      finish.push(() => { active--; resolve(item * 10) })
    })
    const pending = mapLimit([1, 2, 3, 4, 5, 6], 4, worker)
    await Promise.resolve()
    expect(finish).toHaveLength(4)
    // 后完成的先释放：结果仍按输入顺序。
    finish[3]!(); finish[0]!()
    await Promise.resolve(); await Promise.resolve()
    expect(finish).toHaveLength(6)
    finish.slice(1).forEach((done) => done())
    expect(await pending).toEqual([10, 20, 30, 40, 50, 60])
    expect(peak).toBe(4)
    await expect(mapLimit([1, 2], 4, async (item) => { if (item === 2) throw new Error('坏段'); return item })).rejects.toThrow('坏段')
    expect(await mapLimit([], 4, async (item) => item)).toEqual([])
  })
  it('批量段读限制条数、单段与总量', () => {
    const ref = { id: 3, content_version: 'b'.repeat(64) }
    const batch = rangesInput({ ref, ranges: [{ offset: 0, length: 512 }, { offset: 1024, length: 512 }] })
    expect(batch.ranges).toEqual([{ offset: 0, length: 512 }, { offset: 1024, length: 512 }])
    expect(() => rangesInput({ ref, ranges: [] })).toThrow('批量读取参数无效')
    expect(() => rangesInput({ ref, ranges: Array.from({ length: 9 }, () => ({ offset: 0, length: 1 })) })).toThrow('批量读取参数无效')
    expect(() => rangesInput({ ref, ranges: [{ offset: 0, length: 0 }] })).toThrow()
    expect(() => rangesInput({ ref, ranges: [{ offset: -1, length: 1 }] })).toThrow()
    expect(() => rangesInput({ ref, ranges: Array.from({ length: 8 }, () => ({ offset: 0, length: 1024 * 1024 })) })).toThrow('总量')
    expect(() => rangesInput({ ref: { id: 3 }, ranges: [{ offset: 0, length: 1 }] })).toThrow('批量读取参数无效')
  })
})
