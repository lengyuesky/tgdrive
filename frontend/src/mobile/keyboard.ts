/** Element 输入组件也可能传入普通事件；兼容 Safari 用 229 标记输入法确认键。 */
export function isComposingKey(event: Event): boolean {
  return ('isComposing' in event && event.isComposing === true)
    || ('keyCode' in event && event.keyCode === 229)
}
