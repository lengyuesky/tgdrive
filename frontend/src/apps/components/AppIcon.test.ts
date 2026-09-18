import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import type { AppManifest } from '../types'
import AppIcon from './AppIcon.vue'

const manifest: AppManifest = {
  id: 'sample', name: '测试应用', version: '1.0.0', api_version: 2, min_host_version: '0.1.0',
  description: '合成清单', author: '测试作者', entry: 'index.html', permissions: [], settings: [],
}
const iconed: AppManifest = { ...manifest, icon: 'icon.svg' }

describe('应用图标', () => {
  it('安装后的应用渲染包内 SVG 资源地址', () => {
    const wrapper = mount(AppIcon, { props: { manifest: iconed, revision: 'rev1' } })
    const img = wrapper.find('img.app-icon-img')
    expect(img.attributes('src')).toBe('/api/apps/runtime/sample/rev1/icon.svg')
    expect(img.attributes('alt')).toBe('测试应用')
  })

  it('目录中未安装应用与未声明图标的应用回退字母渐变', () => {
    const noRevision = mount(AppIcon, { props: { manifest: iconed } })
    expect(noRevision.find('img').exists()).toBe(false)
    expect(noRevision.find('.app-icon-letter').text()).toBe('测')
    expect(noRevision.find('.app-icon-letter').attributes('style')).toContain('linear-gradient')
    const noIcon = mount(AppIcon, { props: { manifest, revision: 'rev1' } })
    expect(noIcon.find('img').exists()).toBe(false)
    expect(noIcon.find('.app-icon-letter').text()).toBe('测')
  })

  it('图标加载失败回退字母，修订变化后重新尝试加载', async () => {
    const wrapper = mount(AppIcon, { props: { manifest: iconed, revision: 'rev1' } })
    await wrapper.find('img.app-icon-img').trigger('error')
    expect(wrapper.find('.app-icon-letter').exists()).toBe(true)
    await wrapper.setProps({ revision: 'rev2' })
    const img = wrapper.find('img.app-icon-img')
    expect(img.attributes('src')).toBe('/api/apps/runtime/sample/rev2/icon.svg')
  })
})
