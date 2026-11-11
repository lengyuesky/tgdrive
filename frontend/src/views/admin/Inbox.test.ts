import { flushPromises, mount } from '@vue/test-utils'
import { ref } from 'vue'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Extras, InboxRequest, InboxTypeDirectories } from '../../api'
import { useAdminExtras } from './useAdminExtras'
import InboxDirectoryList from './InboxDirectoryList.vue'
import InboxRequests from './InboxRequests.vue'

const mocks = vi.hoisted(() => ({
  extras: vi.fn(), saveExtras: vi.fn(), inboxRequests: vi.fn(),
  inboxRequestCancel: vi.fn(), inboxRequestRetry: vi.fn(), inboxRequestResend: vi.fn(),
  confirm: vi.fn(), context: { current: null as Record<string, unknown> | null },
}))
vi.mock('../../api', () => ({
  api: { admin: mocks }, errMsg: String, fmtTime: String, fmtSize: String,
}))
vi.mock('../../dialog', () => ({ appConfirm: mocks.confirm }))
vi.mock('./useAdmin', () => ({ useAdminContext: () => mocks.context.current }))

function fixture(): Extras {
  return {
    inbox_bot_enabled: true, inbox_channel_enabled: true, inbox_allowed_chats: '42',
    inbox_dir: '/收件', inbox_video_dir: '/视频', inbox_image_dir: '/图片', inbox_book_dir: '/书籍',
    inbox_rules: [{ name: '音乐', extensions: ['mp3'], dir: '/音乐' }],
    backup_chunk_size: 18 * 1024 ** 2, backup_telegram_enabled: true,
  } as Extras
}
function boundRoute(key: string, path: string, id: number): InboxTypeDirectories {
  return { key, directories: [{ node_id: id, path, automatic: true, missing: false }] }
}
function job(id: string, status: string): InboxRequest {
  return {
    id, status, bot_id: '100', chat_id: 42, source_message_id: 10,
    original_name: 'book.pdf', name: '书籍.pdf', selected_path: '/书籍', node_id: null,
    result_path: null, last_error: status === 'uncertain' ? '转存结果待核实' : null,
    created_at: 100, updated_at: 100, expires_at: 9999999999, ended_at: null,
    can_cancel: true, can_retry: status === 'failed',
    target_channel: null, stored_message_id: null, copy_started_at: null,
  }
}

let wrapper: ReturnType<typeof mount> | undefined
let controller: ReturnType<typeof useAdminExtras>
let saved: Extras

beforeEach(async () => {
  vi.clearAllMocks()
  vi.useFakeTimers()
  saved = fixture()
  mocks.extras.mockImplementation(async () => ({ data: { extras: structuredClone(saved), backup_last_ts: null, backup_last_by_target: { telegram: null, webdav: null, s3: null }, trash_count: 0 } }))
  mocks.saveExtras.mockResolvedValue({ data: { ok: true } })
  mocks.inboxRequests.mockResolvedValue({ data: { items: [], counts: {}, next_cursor: null, current_bot_id: '100' } })
  mocks.inboxRequestCancel.mockResolvedValue({ data: { ok: true } })
  mocks.inboxRequestRetry.mockResolvedValue({ data: { ok: true } })
  mocks.inboxRequestResend.mockResolvedValue({ data: { ok: true } })
  mocks.confirm.mockResolvedValue(undefined)
  controller = useAdminExtras()
  await controller.loadExtras()
  mocks.context.current = { ...controller, restoreLocked: ref(false), restore: { status: ref({ safe_mode: false }) } }
})
afterEach(() => { wrapper?.unmount(); wrapper = undefined; vi.useRealTimers() })

describe('收件目录编辑', () => {
  it('旧单目录配置加载为每种类型的唯一自动目录', async () => {
    const route = controller.inboxRouteForType('book')
    expect(route.directories).toEqual([{ node_id: null, path: '/书籍', automatic: true, missing: false }])
    expect(controller.inboxRouteForType('custom:音乐').directories[0]?.path).toBe('/音乐')
    await controller.saveExtras()
    expect(mocks.saveExtras).not.toHaveBeenCalled()
    controller.extras.value!.inbox_bot_enabled = false
    await controller.saveExtras()
    const patch = mocks.saveExtras.mock.calls[0]![0]
    expect(patch).toEqual({ inbox_bot_enabled: false })
  })

  it('重排目录保留自动目录，修改路径解除旧身份绑定', async () => {
    saved.inbox_routes = [boundRoute('default', '/收件', 10), boundRoute('video', '/视频', 11), boundRoute('image', '/图片', 12), boundRoute('book', '/书籍', 13), boundRoute('custom:音乐', '/音乐', 14)]
    await controller.loadExtras()
    const route = controller.inboxRouteForType('video')
    wrapper = mount(InboxDirectoryList, { props: { route }, global: { plugins: [ElementPlus] } })
    await wrapper.findAll('button').find(button => button.text() === '添加目录')!.trigger('click')
    const inputs = wrapper.findAllComponents({ name: 'ElInput' })
    inputs[1]!.vm.$emit('update:modelValue', '/电视剧')
    const radios = wrapper.findAllComponents({ name: 'ElRadio' })
    radios[1]!.vm.$emit('update:modelValue', 1)
    await flushPromises()
    await wrapper.find('button[aria-label="上移目录 2"]').trigger('click')
    expect(route.directories[0]?.path).toBe('/电视剧')
    expect(route.directories[0]?.automatic).toBe(true)
    expect(route.directories[1]?.node_id).toBe(11)
    wrapper.findAllComponents({ name: 'ElInput' })[1]!.vm.$emit('update:modelValue', '/电影')
    await controller.saveExtras()
    const patch = mocks.saveExtras.mock.calls[0]![0]
    const video = patch.inbox_routes.find((route: InboxTypeDirectories) => route.key === 'video')
    expect(video.directories.map((directory: { path: string }) => directory.path)).toEqual(['/电视剧', '/电影'])
    expect(video.directories[1].node_id).toBeNull()
    expect(patch.inbox_video_dir).toBe('/电视剧')
  })

  it('自定义类型改名保留目录身份，删除类型同时移除对应配置', async () => {
    saved.inbox_routes = [boundRoute('default', '/收件', 10), boundRoute('video', '/视频', 11), boundRoute('image', '/图片', 12), boundRoute('book', '/书籍', 13), boundRoute('custom:音乐', '/音乐', 14)]
    await controller.loadExtras()
    controller.extras.value!.inbox_rules[0]!.name = '音频'
    await controller.saveExtras()
    const patch = mocks.saveExtras.mock.calls[0]![0]
    expect(patch.inbox_routes.find((route: InboxTypeDirectories) => route.key === 'custom:音频').directories[0].node_id).toBe(14)
    expect(patch.inbox_routes.some((route: InboxTypeDirectories) => route.key === 'custom:音乐')).toBe(false)
    controller.removeInboxRule(0)
    expect(controller.extras.value!.inbox_routes?.some(route => route.key.startsWith('custom:'))).toBe(false)
  })

  it('同时新增的未命名类型各自保留目录，填好名称后分别保存', async () => {
    controller.addInboxRule()
    controller.addInboxRule()
    const rules = controller.extras.value!.inbox_rules
    const first = controller.inboxRouteForRule(rules[1]!)
    const second = controller.inboxRouteForRule(rules[2]!)
    expect(first).not.toBe(second)
    rules[1]!.name = '压缩包'; rules[1]!.extensions = ['zip']
    rules[2]!.name = '源码'; rules[2]!.extensions = ['rs']
    controller.setInboxDirectoryPath(first.directories[0]!, '/归档')
    controller.setInboxDirectoryPath(second.directories[0]!, '/代码')
    await controller.saveExtras()
    const patch = mocks.saveExtras.mock.calls[0]![0]
    expect(patch.inbox_routes.find((route: InboxTypeDirectories) => route.key === 'custom:压缩包').directories[0].path).toBe('/归档')
    expect(patch.inbox_routes.find((route: InboxTypeDirectories) => route.key === 'custom:源码').directories[0].path).toBe('/代码')
  })
})

describe('收件待办管理', () => {
  it('明确失败提供重试，结果未知只允许核实后结束', async () => {
    mocks.inboxRequests.mockResolvedValue({ data: { items: [job('failed', 'failed'), job('unknown', 'uncertain')], counts: { failed: 1, uncertain: 1 }, next_cursor: null, current_bot_id: '100' } })
    wrapper = mount(InboxRequests, { global: { plugins: [ElementPlus] } })
    await flushPromises()
    const unknown = wrapper.findAll('.request-card').find(card => card.text().includes('待核实'))!
    expect(unknown.text()).not.toContain('重试')
    expect(unknown.text()).toContain('原文件：book.pdf')
    await unknown.findAll('button').find(button => button.text() === '结束待办')!.trigger('click')
    await flushPromises()
    expect(mocks.confirm).toHaveBeenCalledWith(expect.objectContaining({ message: expect.stringContaining('不会删除无法确认的频道副本') }))
    expect(mocks.inboxRequestCancel).toHaveBeenCalledWith('unknown')
    expect(mocks.inboxRequestRetry).not.toHaveBeenCalled()
  })

  it('恢复安全模式阻止重试与重发，分页使用上次返回的游标', async () => {
    mocks.inboxRequests.mockResolvedValue({ data: { items: [job('first', 'pending')], counts: { pending: 1 }, next_cursor: '100:cursor', current_bot_id: '100' } })
    mocks.context.current!.restore = { status: ref({ safe_mode: true }) }
    wrapper = mount(InboxRequests, { global: { plugins: [ElementPlus] } })
    await flushPromises()
    const resend = wrapper.findAll('button').find(button => button.text() === '重发操作消息')!
    expect(resend.attributes('disabled')).toBeDefined()
    await wrapper.find('button.request-more').trigger('click')
    await flushPromises()
    expect(mocks.inboxRequests).toHaveBeenLastCalledWith({ active: true, limit: 50, cursor: '100:cursor' })
    expect(mocks.inboxRequestResend).not.toHaveBeenCalled()
  })
})
