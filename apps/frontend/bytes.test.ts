import { describe, expect, it } from 'vitest'
import { assetPath, boundedBytes, rangeInput } from './bytes'

describe('有界文件读取', () => {
  const signal = new AbortController().signal
  const response = (body: string, range = 'bytes 2-4/10', status = 206) => new Response(body, { status, headers: { 'Content-Length': String(body.length), 'Content-Range': range } })
  it('验证精确范围、实际长度和 EOF 裁剪', async () => {
    expect([...await boundedBytes(response('234'), 1024, signal, { offset: 2, length: 3 })]).toEqual([50, 51, 52])
    expect((await boundedBytes(response('89', 'bytes 8-9/10'), 1024, signal, { offset: 8, length: 10 })).length).toBe(2)
    await expect(boundedBytes(response('234', 'bytes 2-4/10', 200), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow('整个文件')
    await expect(boundedBytes(response('234', 'bytes 3-5/10'), 1024, signal, { offset: 2, length: 3 })).rejects.toThrow()
    await expect(boundedBytes(new Response('abcd', { headers: { 'Content-Length': '3' } }), 1024, signal)).rejects.toThrow('超过声明')
    await expect(boundedBytes(new Response('ab', { headers: { 'Content-Length': '3' } }), 1024, signal)).rejects.toThrow('提前结束')
  })
  it('拒绝任意 URL、过大范围与资源路径穿越', () => {
    const ref = { id: 2, content_version: 'a'.repeat(64) }
    expect(rangeInput({ ref, offset: 0, length: 1024 }).ref).toEqual(ref)
    for (const length of [0, -1, 1048577, Infinity]) expect(() => rangeInput({ ref, offset: 0, length })).toThrow()
    expect(() => rangeInput({ ref: { path: 'https://example.invalid' }, offset: 0, length: 2 })).toThrow()
    for (const path of ['../api/admin', '/api/admin', 'https://bad', 'a/%2e%2e/b', 'a\\b', '.hidden']) expect(() => assetPath(path)).toThrow()
    expect(assetPath('fonts/FoxitSans.pfb')).toBe('fonts/FoxitSans.pfb')
  })
})
