import { describe, expect, it } from 'vitest'
import { keyboardIsOpen } from './viewport'
import { boundedScale, swipeDirection } from './image-gestures'

describe('手机视口与图片手势', () => {
  it('地址栏变化和页面缩放不能误判为软键盘', () => {
    expect(keyboardIsOpen(844, 760, true)).toBe(false)
    expect(keyboardIsOpen(844, 480, false)).toBe(false)
    expect(keyboardIsOpen(844, 480, true, 2)).toBe(false)
    expect(keyboardIsOpen(844, 480, true)).toBe(true)
  })
  it('仅未缩放的单指明显横滑切图，多指与竖向移动不切图', () => {
    expect(swipeDirection(-100, 10, 1, false)).toBe(1)
    expect(swipeDirection(100, 10, 1, false)).toBe(-1)
    expect(swipeDirection(100, 200, 1, false)).toBe(0)
    expect(swipeDirection(100, 0, 2, false)).toBe(0)
    expect(swipeDirection(100, 0, 1, true)).toBe(0)
    expect(boundedScale(10)).toBe(4)
    expect(boundedScale(.5)).toBe(1)
  })
})
