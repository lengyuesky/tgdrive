import { describe, expect, it } from 'vitest'
import { appIconStyle } from './icon'

describe('应用图标配色', () => {
  it('同一 ID 生成确定的渐变背景', () => {
    expect(appIconStyle('sample')).toEqual({ background: 'linear-gradient(140deg, hsl(94 85% 64%), hsl(139 78% 52%))' })
    expect(appIconStyle('sample')).toEqual(appIconStyle('sample'))
  })
  it('不同 ID 色相不同', () => {
    expect(appIconStyle('sample').background).not.toBe(appIconStyle('viewer').background)
    expect(appIconStyle('viewer')).toEqual({ background: 'linear-gradient(140deg, hsl(78 85% 64%), hsl(123 78% 52%))' })
  })
  it('输出始终是两段 hsl 渐变', () => {
    for (const id of ['books', 'sample-viewer', '', '带中文的标识']) {
      expect(appIconStyle(id).background).toMatch(/^linear-gradient\(140deg, hsl\(\d+ 85% 64%\), hsl\(\d+ 78% 52%\)\)$/)
    }
  })
})
