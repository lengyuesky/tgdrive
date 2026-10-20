import type { InternalAxiosRequestConfig } from 'axios'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { http } from '@drive/http'
import { appApi } from './client'

vi.mock('@drive/router', () => ({ router: { currentRoute: { value: { path: '/apps' } }, replace: vi.fn() } }))

const originalAdapter = http.defaults.adapter
let requests: InternalAxiosRequestConfig[]
beforeEach(() => {
  requests = []
  http.defaults.adapter = async (config) => {
    requests.push(config)
    return { config, data: {}, status: 200, statusText: 'OK', headers: {} }
  }
})
afterEach(() => { http.defaults.adapter = originalAdapter })

describe('安装预览与替换的 HTTP 契约', () => {
  it('目录预览通过 POST JSON 发送确认摘要，不放入查询参数', async () => {
    await appApi.inspectCatalog('reader', 'a'.repeat(64))
    const request = requests[0]!
    expect(request.method).toBe('post')
    expect(request.url).toBe('/apps/catalog/reader/inspect')
    expect(request.params).toBeUndefined()
    expect(request.headers.get('Content-Type')).toBe('application/json')
    expect(JSON.parse(request.data)).toEqual({ digest: 'a'.repeat(64) })
  })

  it.each([false, true])('目录安装在 JSON 内携带 replace_legacy=%s、摘要和旧修订', async (replaceLegacy) => {
    await appApi.installCatalog('reader', 'digest', 'old-revision', replaceLegacy, 'server-preview-token')
    expect(requests[0]!.url).toBe('/apps/catalog/reader/install')
    expect(JSON.parse(requests[0]!.data)).toEqual({ digest: 'digest', expected_revision: 'old-revision', replace_legacy: replaceLegacy, replacement_revision: 'server-preview-token' })
  })

  it.each([false, true])('本地安装保留原包字节，在查询参数内携带 replace_legacy=%s', async (replaceLegacy) => {
    const file = new File(['original-package'], 'reader.tgapp')
    await appApi.inspect(file)
    await appApi.install(file, 'digest', 'old-revision', replaceLegacy, 'server-preview-token')
    expect(requests[0]!.url).toBe('/apps/inspect')
    expect(requests[0]!.data).toBe(file)
    expect(requests[0]!.headers.get('Content-Type')).toBe('application/octet-stream')
    expect(requests[1]!.url).toBe('/apps/install')
    expect(requests[1]!.data).toBe(file)
    expect(requests[1]!.params).toEqual({ digest: 'digest', expected_revision: 'old-revision', replace_legacy: replaceLegacy, replacement_revision: 'server-preview-token' })
  })

  it('所有安装入口默认不授权旧应用替换', async () => {
    await appApi.installCatalog('reader', 'digest')
    await appApi.install(new File(['package'], 'reader.tgapp'), 'digest')
    expect(JSON.parse(requests[0]!.data)).toEqual({ digest: 'digest', replace_legacy: false })
    expect(requests[1]!.params.replace_legacy).toBe(false)
  })

  it.each([undefined, null])('预览指纹为 %s 时保留可选字段的兼容行为', async (revision) => {
    await appApi.installCatalog('sample', 'digest', undefined, false, revision)
    await appApi.install(new File(['package'], 'sample.tgapp'), 'digest', undefined, false, revision)
    const body = JSON.parse(requests[0]!.data)
    if (revision === null) expect(body.replacement_revision).toBeNull()
    else expect(body).not.toHaveProperty('replacement_revision')
    expect(requests[1]!.params.replacement_revision).toBe(revision)
  })
})
