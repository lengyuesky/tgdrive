import { describe, expect, it } from 'vitest'
import { filesChangeAffectsScope, pathInScope } from './types'

describe('目录范围判定', () => {
  it('路径落在根之下才算在范围内，空范围全盘', () => {
    expect(pathInScope([], '/任意')).toBe(true)
    expect(pathInScope(undefined, '/任意')).toBe(true)
    expect(pathInScope(['/书'], '/书')).toBe(true)
    expect(pathInScope(['/书/'], '/书/子')).toBe(true)
    expect(pathInScope(['/书'], '/书籍')).toBe(false)
    expect(pathInScope(['/'], '/别处')).toBe(true)
  })
  it('文件事件按变更目录与范围的包含或祖先关系过滤；范围未知一律相关', () => {
    expect(filesChangeAffectsScope(['/书'], undefined)).toBe(true)
    expect(filesChangeAffectsScope([], ['/别处'])).toBe(true)
    expect(filesChangeAffectsScope(['/书'], ['/书'])).toBe(true)
    expect(filesChangeAffectsScope(['/书'], ['/书/子目录'])).toBe(true)
    // 根的祖先列表变化（根本身可能被改名或删除）
    expect(filesChangeAffectsScope(['/书/子'], ['/书'])).toBe(true)
    expect(filesChangeAffectsScope(['/书'], ['/'])).toBe(true)
    // 兄弟目录与前缀相同的其他目录无关
    expect(filesChangeAffectsScope(['/书'], ['/影片', '/书籍'])).toBe(false)
    expect(filesChangeAffectsScope(['/书/a'], ['/书/b'])).toBe(false)
    // 空列表表示可见树无变化
    expect(filesChangeAffectsScope(['/书'], [])).toBe(false)
    expect(filesChangeAffectsScope(['/'], [])).toBe(false)
    expect(filesChangeAffectsScope([], [])).toBe(false)
  })
})
