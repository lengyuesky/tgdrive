import { flushPromises, mount } from '@vue/test-utils'
import { describe, expect, it, vi } from 'vitest'
import { invalidateSession } from '../session'
import SessionBoundary from './SessionBoundary.vue'

const replace = vi.fn()
vi.mock('vue-router', () => ({ useRouter: () => ({ replace }) }))
vi.mock('../api', () => ({ api: { me: vi.fn().mockResolvedValue({}) }, errMsg: (error: unknown) => String(error) }))
vi.mock('./SessionShell.vue', () => ({ default: { template: '<div class="shell" />' } }))

describe('登录外壳边界', () => {
  it('会话失效时收起外壳并离开到登录页，不停在等待页', async () => {
    const wrapper = mount(SessionBoundary, { global: { stubs: { 'el-button': true } } })
    await flushPromises()
    expect(wrapper.find('.shell').exists()).toBe(true)
    invalidateSession()
    await flushPromises()
    expect(wrapper.find('.shell').exists()).toBe(false)
    expect(replace).toHaveBeenCalledExactlyOnceWith('/login')
    wrapper.unmount()
    invalidateSession()
    expect(replace).toHaveBeenCalledTimes(1)
  })
})
