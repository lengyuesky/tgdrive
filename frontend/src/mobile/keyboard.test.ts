import { describe, expect, it } from 'vitest'
import { isComposingKey } from './keyboard'

describe('输入法按键保护', () => {
  it('中文候选确认与 Safari 229 按键不触发表单提交', () => {
    expect(isComposingKey(new KeyboardEvent('keydown', { key: 'Enter', isComposing: true }))).toBe(true)
    expect(isComposingKey(new KeyboardEvent('keydown', { key: 'Enter', keyCode: 229 }))).toBe(true)
  })

  it('普通回车和组件普通事件可安全处理', () => {
    expect(isComposingKey(new KeyboardEvent('keydown', { key: 'Enter' }))).toBe(false)
    expect(isComposingKey(new Event('keydown'))).toBe(false)
  })
})
