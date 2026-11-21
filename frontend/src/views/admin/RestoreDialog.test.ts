import { ref, computed } from 'vue'
import { shallowMount, flushPromises } from '@vue/test-utils'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import RestoreDialog from './RestoreDialog.vue'
import type { RestoreController } from './useRestore'
import type { RestoreStatus } from '../../api'

const mocks = vi.hoisted(() => ({ history: vi.fn(), discover: vi.fn() }))
vi.mock('../../api', () => ({ api: { admin: { restore: mocks } }, errMsg: String, fmtSize: String }))
vi.mock('../../http', () => ({ holdAuthRedirect: vi.fn() }))
let view: ReturnType<typeof shallowMount>
let r: RestoreController
async function render(phase: RestoreStatus['phase'] = 'idle', safe = false) {
  const status = ref<RestoreStatus>({ phase, id: 'job', warnings: ['临时消息清理失败'], safe_mode: safe })
  const active = computed(() => ['preparing', 'ready', 'restarting'].includes(status.value.phase))
  r = {
    status, active, opened: ref(true), busy: ref(false), loaded: ref(true), error: ref(''), waiting: ref(false),
    loginRequired: ref(false), locked: computed(() => active.value), open: vi.fn(), refresh: vi.fn(), prepare: vi.fn(),
    confirm: vi.fn(), cancel: vi.fn(), resume: vi.fn(),
  }
  view = shallowMount(RestoreDialog, { props: { controller: r }, global: {
    plugins: [ElementPlus], renderStubDefaultSlot: true,
    stubs: { ResponsivePanel: { template: '<div><slot /><slot name="footer" /></div>' } },
  } })
  await flushPromises()
}
function input(label: string) {
  return view.findAllComponents({ name: 'ElFormItem' }).find(x => x.attributes('label') === label || x.props('label') === label)!.findComponent({ name: 'ElInput' })
}
async function fill(label: string, value: string) { input(label).vm.$emit('update:modelValue', value); await flushPromises() }
function button(label: string) { return view.findAllComponents({ name: 'ElButton' }).find(x => x.text() === label)! }
async function click(label: string) { button(label).vm.$emit('click'); await flushPromises() }
beforeEach(() => {
  vi.clearAllMocks()
  mocks.discover.mockResolvedValue({ data: { backups: [], scanned: 100, has_more: false } })
  mocks.history.mockResolvedValue({ data: { backups: [
    { id: 'ok', name: 'backup.db.age', parts: 2, available: true },
    { id: 'bad', name: '损坏记录', parts: 0, available: false, error: '存储目标不存在' },
  ] } })
})
afterEach(() => view?.unmount())

describe('恢复向导表单', () => {
  it('本机备份显示不可用记录，必须明确确认临时转发，解密密码提交即清空', async () => {
    await render()
    view.findComponent({ name: 'ElRadioGroup' }).vm.$emit('update:modelValue', 'history')
    await flushPromises()
    expect(view.text()).toContain('存储目标不存在')
    expect(view.text()).toContain('临时消息清理失败')
    const bad = view.findAllComponents({ name: 'ElOption' }).find(x => x.props('value') === 'bad')!
    expect(bad.props('disabled')).toBe(true)
    view.findComponent({ name: 'ElSelect' }).vm.$emit('update:modelValue', 'ok')
    await fill('备份解密密码（可选，仅用于本次预检）', 'decrypt-secret')
    await click('下载并预检')
    expect(r.prepare).not.toHaveBeenCalled()
    view.findComponent({ name: 'ElCheckbox' }).vm.$emit('update:modelValue', true)
    await click('下载并预检')
    expect(r.prepare).toHaveBeenCalledWith({ backup_id: 'ok', password: 'decrypt-secret', acknowledge_forward: true })
    expect(input('备份解密密码（可选，仅用于本次预检）').props('modelValue')).toBe('')
    expect(localStorage.getItem('decrypt-secret')).toBeNull()
  })
  it('保留手动输入完整非连续分块，提交频道和多消息 ID', async () => {
    mocks.history.mockResolvedValue({ data: { backups: [] } })
    await render()
    expect(view.text()).toContain('全新部署')
    view.findComponent({ name: 'ElRadioGroup' }).vm.$emit('update:modelValue', 'manual')
    await flushPromises()
    expect(view.text()).toContain('手动来源无需配置')
    expect(view.text()).toContain('必须包含全部分块')
    expect(view.text()).toContain('绝不删除原备份')
    await fill('频道 ID（使用消息 ID 时必填；完整链接可留空）', '-100123')
    await fill('完整一组消息链接，或多个消息 ID', '42\n57')
    view.findComponent({ name: 'ElCheckbox' }).vm.$emit('update:modelValue', true)
    await click('下载并预检')
    expect(r.prepare).toHaveBeenCalledWith({ messages: '42\n57', channel: '-100123', acknowledge_forward: true })
  })
  it('仅点击探测后跨页查找五份完整备份，缺块不算完成，达到五份后停止', async () => {
    mocks.history.mockResolvedValue({ data: { backups: [] } })
    const complete = Array.from({ length: 5 }, (_, i) => ({ id: i ? `channel:${i}` : 'channel:old', name: `tgdrive-backup-${i}.db`, parts: 2, available: true }))
    mocks.discover.mockResolvedValueOnce({ data: { backups: complete.map((b, i) => ({ ...b, available: i < 4 })), scanned: 100, has_more: true } })
      .mockResolvedValueOnce({ data: { backups: complete, scanned: 200, has_more: false } })
    await render()
    expect(mocks.discover).not.toHaveBeenCalled()
    view.findComponent({ name: 'ElRadioGroup' }).vm.$emit('update:modelValue', 'history')
    await flushPromises()
    view.findComponent({ name: 'ElRadioGroup' }).vm.$emit('update:modelValue', 'channel')
    r.loaded.value = false
    await flushPromises()
    r.loaded.value = true
    await flushPromises()
    expect(mocks.discover).not.toHaveBeenCalled()
    await click('探测频道备份')
    expect(mocks.discover.mock.calls).toEqual([[true], [false]])
    expect(view.text()).toContain('已找到最近 5 份完整备份，探测已停止')
    expect(view.text()).not.toContain('已探测到频道起点')
    expect(button('继续探测')).toBeUndefined()
    view.findComponent({ name: 'ElSelect' }).vm.$emit('update:modelValue', 'channel:old')
    view.findComponent({ name: 'ElCheckbox' }).vm.$emit('update:modelValue', true)
    await click('下载并预检')
    expect(r.prepare).toHaveBeenCalledWith({ backup_id: 'channel:old', acknowledge_forward: true })
  })
  it('探测失败时显示配置错误并允许重试，不会假报没有备份', async () => {
    mocks.discover.mockRejectedValueOnce(new Error('需要 TELEGRAM_API_ID'))
    await render()
    await click('探测频道备份')
    expect(view.findAllComponents({ name: 'ElAlert' }).some(x => String(x.props('title')).includes('需要 TELEGRAM_API_ID'))).toBe(true)
    expect(view.text()).not.toContain('频道中未发现')
    await click('重新探测频道')
    expect(mocks.discover).toHaveBeenCalledTimes(2)
  })
  it('完整消息链接可不填频道，纯 ID 缺少频道时不能提交', async () => {
    await render()
    view.findComponent({ name: 'ElRadioGroup' }).vm.$emit('update:modelValue', 'manual')
    await flushPromises()
    await fill('完整一组消息链接，或多个消息 ID', '42\n57')
    view.findComponent({ name: 'ElCheckbox' }).vm.$emit('update:modelValue', true)
    await click('下载并预检')
    expect(r.prepare).not.toHaveBeenCalled()
    await fill('完整一组消息链接，或多个消息 ID', 'https://t.me/c/123/42\nhttps://t.me/c/123/57')
    await click('下载并预检')
    expect(r.prepare).toHaveBeenCalledWith({ messages: 'https://t.me/c/123/42\nhttps://t.me/c/123/57', acknowledge_forward: true })
  })
  it('最终确认须精确输入恢复及登录密码，可选 2FA，发送后清空凭据', async () => {
    await render('ready')
    await fill('当前登录密码', 'login-secret')
    await fill('两步验证码（已启用 2FA 时必填）', '123456')
    await fill('输入“恢复”以确认覆盖当前数据库', '恢复 ')
    await click('确认恢复并重启')
    expect(r.confirm).not.toHaveBeenCalled()
    await fill('输入“恢复”以确认覆盖当前数据库', '恢复')
    await click('确认恢复并重启')
    expect(r.confirm).toHaveBeenCalledWith({ id: 'job', password: 'login-secret', totp: '123456', confirmation: '恢复' })
    expect(input('当前登录密码').props('modelValue')).toBe('')
    expect(input('两步验证码（已启用 2FA 时必填）').props('modelValue')).toBe('')
  })
  it('解除安全模式须检查旧任务风险并再次认证，不自动调用恢复', async () => {
    await render('succeeded', true)
    expect(view.text()).toContain('删除真实频道文件')
    expect(r.resume).not.toHaveBeenCalled()
    await fill('当前登录密码', 'current')
    await click('解除安全模式并重启')
    expect(r.resume).not.toHaveBeenCalled()
    view.findComponent({ name: 'ElCheckbox' }).vm.$emit('update:modelValue', true)
    await click('解除安全模式并重启')
    expect(r.resume).toHaveBeenCalledWith({ password: 'current', totp: undefined })
    expect(r.prepare).not.toHaveBeenCalled()
    expect(r.confirm).not.toHaveBeenCalled()
    expect(input('当前登录密码').props('modelValue')).toBe('')
  })
})
