/**
 * 居中毛玻璃对话框(替代 ElMessageBox):
 * - appConfirm / appPrompt 取消时 reject('cancel'),沿用原有 `e !== 'cancel'` 的错误处理习惯
 * - appPrompt resolve `{ value }`,与 ElMessageBox.prompt 的解构写法兼容
 */
import { createVNode, render, type Component } from 'vue'
import AppDialog from './components/AppDialog.vue'

export interface DialogOptions {
  title: string
  message?: string
  /** 徽章图标,不传按 kind 取默认 */
  icon?: Component
  /** 徽章与确认按钮的配色,默认 primary */
  kind?: 'primary' | 'danger' | 'warning'
  confirmText?: string
  cancelText?: string
  /** 可选的第三操作，显示在取消与主确认按钮之间。 */
  alternateText?: string
  alternateKind?: 'primary' | 'danger' | 'warning'
}

export interface PromptOptions extends DialogOptions {
  value?: string
  placeholder?: string
  pattern?: RegExp
  errorMessage?: string
}

type Mode = 'confirm' | 'prompt' | 'alert'
type DialogResult = { action: 'confirm'; value: string } | { action: 'alternate' }
export type DialogChoice = DialogResult['action']

function show(mode: Mode, opts: PromptOptions): Promise<DialogResult> {
  return new Promise((resolve, reject) => {
    const host = document.createElement('div')
    document.body.appendChild(host)
    render(
      createVNode(AppDialog, {
        mode,
        ...opts,
        onResolve: (v: string) => resolve({ action: 'confirm', value: v }),
        onAlternate: () => resolve({ action: 'alternate' }),
        onReject: () => reject('cancel'),
        // 离场动画结束后再卸载
        onDone: () => {
          render(null, host)
          host.remove()
        },
      }),
      host,
    )
  })
}

/** 确认框:确定 resolve,取消 / Esc / 点遮罩 reject('cancel') */
export function appConfirm(opts: DialogOptions): Promise<void> {
  return show('confirm', opts).then(() => undefined)
}

/** 输入框:确定 resolve({ value }),取消 reject('cancel') */
export function appPrompt(opts: PromptOptions): Promise<{ value: string }> {
  return show('prompt', opts).then((result) => ({
    value: result.action === 'confirm' ? result.value : '',
  }))
}

/** 三选一确认框：主操作、备选操作或取消。 */
export function appChoice(
  opts: DialogOptions & { alternateText: string },
): Promise<DialogChoice> {
  return show('confirm', opts).then((result) => result.action)
}

/** 提示框:只有一个确认按钮,Esc / 点遮罩也算确认,永不 reject */
export function appAlert(opts: DialogOptions): Promise<void> {
  return show('alert', opts).then(() => undefined)
}
