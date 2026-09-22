import { describe, expect, it } from 'vitest'
import { BATCH_LIMIT, batchInput } from './batch'

describe('批量信封校验', () => {
  it('只接受白名单能力、1～16 项、对象参数；缺省参数补空对象', () => {
    const batch = batchInput({ calls: [{ method: 'files.stat', params: { id: 1 } }, { method: 'app.ping' }] })
    expect(batch).toEqual({ calls: [{ method: 'files.stat', params: { id: 1 } }, { method: 'app.ping', params: {} }] })
    expect(() => batchInput({ calls: [] })).toThrow('1 到 16')
    expect(() => batchInput({ calls: Array.from({ length: BATCH_LIMIT + 1 }, () => ({ method: 'app.ping' })) })).toThrow('1 到 16')
    expect(() => batchInput({})).toThrow()
    for (const method of ['ui.close', 'files.readRange', 'files.readRanges', 'assets.read', 'rpc.batch', 'ui.setImmersive', '/admin']) {
      expect(() => batchInput({ calls: [{ method }] })).toThrow('不支持')
    }
    expect(() => batchInput({ calls: [{ method: 'files.stat', params: [1] }] })).toThrow('参数无效')
    expect(() => batchInput({ calls: [null] })).toThrow('不支持')
  })
})
