import { flushPromises, shallowMount } from '@vue/test-utils'
import { nextTick } from 'vue'
import ElementPlus from 'element-plus'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import Admin from './Admin.vue'
import SettingField from '../components/SettingField.vue'

const mocks = vi.hoisted(() => ({
  config: vi.fn(), status: vi.fn(), extras: vi.fn(), save: vi.fn(), reset: vi.fn(),
  test: vi.fn(), detectChats: vi.fn(), confirm: vi.fn(),
  route: { query: { s: 'proxy' } },
}))
vi.mock('vue-router', () => ({
  useRoute: () => mocks.route,
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  onBeforeRouteLeave: vi.fn(),
}))
vi.mock('../api', () => ({
  api: { admin: mocks },
  errMsg: (error: { response?: { data?: { error?: string } } }) => error.response?.data?.error ?? String(error),
  fmtSize: String, fmtTime: String, fmtDuration: String,
}))
vi.mock('../dialog', () => ({ appAlert: vi.fn(), appConfirm: mocks.confirm }))
vi.mock('../theme', () => ({ isDark: false, toggleDark: vi.fn() }))

const maskedUrl = 'socks5h://***:***@proxy.example:1080'
let wrapper: ReturnType<typeof shallowMount> | undefined
let config: ReturnType<typeof configFixture>

function configFixture() {
  return {
    values: {
      api_base: 'https://api.telegram.org', bot_token: 'masked-bot-token', upload_bot_tokens: '', chat_id: '-100123',
      proxy_enabled: true, proxy_url: maskedUrl,
      chunk_size: 18 * 1024 ** 2, upload_concurrency: 2, download_concurrency: 4, send_video: false,
      tmp_dir: '/tmp', username: 'test', password: '••••••••', telegram_api_id: '', telegram_api_hash: '',
    },
    sources: { proxy_enabled: 'db', proxy_url: 'db' }, configured: false,
    official_api: true, official_api_base: 'https://api.telegram.org',
    official_chunk_max: 19_500_000, official_chunk_default: 18 * 1024 ** 2, local_chunk_default: 256 * 1024 ** 2,
    readonly: {},
  }
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.useFakeTimers()
  localStorage.clear()
  mocks.route.query.s = 'proxy'
  config = configFixture()
  mocks.config.mockImplementation(async () => ({ data: structuredClone(config) }))
  mocks.status.mockResolvedValue({ data: { configured: false, version: 'test', now: 0, started_at: 0 } })
  mocks.extras.mockResolvedValue({ data: {
    extras: { inbox_rules: [], backup_chunk_size: 18 * 1024 ** 2, backup_telegram_enabled: true },
    backup_last_ts: null, backup_last_by_target: { telegram: null, webdav: null, s3: null }, trash_count: 0,
  } })
  mocks.save.mockImplementation(async (patch) => {
    Object.assign(config.values, patch)
    return { data: { changed: Object.keys(patch), account_changed: false, needs_local_api_restart: false, bot: null } }
  })
  mocks.reset.mockImplementation(async () => {
    config.values.proxy_enabled = false
    config.values.proxy_url = ''
    return { data: { changed: ['proxy_enabled', 'proxy_url'], account_changed: false, needs_local_api_restart: false } }
  })
  mocks.test.mockResolvedValue({ data: { bot: 'test-bot' } })
  mocks.detectChats.mockResolvedValue({ data: { bot: 'test-bot', chats: [] } })
  mocks.confirm.mockResolvedValue(undefined)
})
afterEach(() => { wrapper?.unmount(); vi.useRealTimers() })

async function render() {
  wrapper = shallowMount(Admin, { global: {
    plugins: [ElementPlus], renderStubDefaultSlot: true,
    // 不展开无关弹窗的表格作用域插槽，只检查设置页本身。
    stubs: { ResponsivePanel: { template: '<div />' } },
  } })
  await flushPromises()
  return wrapper
}
function field(label: string) {
  return wrapper!.findAllComponents(SettingField).find((item) => item.props('label') === label)!
}
async function button(label: string) {
  const button = wrapper!.findAllComponents({ name: 'ElButton' }).find((item) => item.text().trim() === label)
  expect(button, `应显示按钮：${label}`).toBeDefined()
  button!.vm.$emit('click')
  await flushPromises()
}
async function editProxy(enabled: boolean, url?: string) {
  field('启用代理').findComponent({ name: 'ElSwitch' }).vm.$emit('update:modelValue', enabled)
  if (url !== undefined) field('代理地址').findComponent({ name: 'ElInput' }).vm.$emit('update:modelValue', url)
  await nextTick()
}
async function section(label: string) {
  const item = wrapper!.findAll('.nav-item').find((item) => item.text().includes(label))!
  await item.trigger('click')
  await flushPromises()
}

describe('系统设置网络代理', () => {
  it('回填脱敏地址，关闭时只提交开关并保留原凭据', async () => {
    const view = await render()
    expect(view.find('.pane-title h2').text()).toBe('网络代理')
    const input = field('代理地址').findComponent({ name: 'ElInput' })
    expect(input.props('modelValue')).toBe(maskedUrl)
    expect(input.props('type')).toBe('password')
    expect(view.text()).toContain('HTTP CONNECT')
    await editProxy(false)
    expect(view.find('.nav-item.on .nav-dot').exists()).toBe(true)
    expect(view.text()).toContain('1 项改动待保存')
    await button('保存并生效')
    expect(mocks.save).toHaveBeenCalledWith({ proxy_enabled: false })
    expect(field('代理地址').findComponent({ name: 'ElInput' }).props('modelValue')).toBe(maskedUrl)
    expect(view.find('.savebar').exists()).toBe(false)
  })

  it('测试与检测 ID 使用未保存代理，跨分区编辑不会丢失', async () => {
    config.values.proxy_enabled = false
    config.values.proxy_url = ''
    await render()
    const url = 'socks5h://user:password@proxy.example:1080'
    await editProxy(true, url)
    await button('测试连接')
    expect(mocks.test).toHaveBeenCalledWith({
      api_base: config.values.api_base, bot_token: config.values.bot_token, chat_id: config.values.chat_id,
      proxy_enabled: true, proxy_url: url,
    })
    expect(mocks.save).not.toHaveBeenCalled()
    await section('Telegram 存储')
    await button('检测 ID')
    expect(mocks.detectChats).toHaveBeenCalledWith({
      api_base: config.values.api_base, bot_token: config.values.bot_token,
      proxy_enabled: true, proxy_url: url,
    })
    await section('网络代理')
    expect(field('代理地址').findComponent({ name: 'ElInput' }).props('modelValue')).toBe(url)
    await button('保存并生效')
    expect(mocks.save).toHaveBeenCalledWith({ proxy_enabled: true, proxy_url: url })
  })

  it('恢复代理地址时同时恢复开关，避免空地址仍启用', async () => {
    await render()
    field('代理地址').vm.$emit('reset')
    await flushPromises()
    expect(mocks.confirm).toHaveBeenCalled()
    expect(mocks.reset).toHaveBeenCalledWith(['proxy_enabled', 'proxy_url'])
    expect(field('启用代理').findComponent({ name: 'ElSwitch' }).props('modelValue')).toBe(false)
    expect(field('代理地址').findComponent({ name: 'ElInput' }).props('modelValue')).toBe('')
  })

  it('连接验证失败时确认后强制保存，代理编辑值不会被丢弃', async () => {
    await render()
    const url = 'http://proxy.example:7890'
    await editProxy(true, url)
    mocks.save.mockRejectedValueOnce({ response: { data: { error: '连接验证未通过，可选择强制保存' } } })
    await button('保存并生效')
    expect(mocks.confirm).toHaveBeenCalledWith(expect.objectContaining({ title: '连接验证未通过' }))
    expect(mocks.save).toHaveBeenNthCalledWith(1, { proxy_url: url })
    expect(mocks.save).toHaveBeenNthCalledWith(2, { proxy_url: url, force: true })
  })
})
