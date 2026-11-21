import { ElMessage } from 'element-plus'
import { appPrompt } from '../dialog'

/** 剪贴板不可用时显示可选文本，而不是让长链接挤在短暂通知里。 */
export async function copyText(text: string) {
  try { await navigator.clipboard.writeText(text); ElMessage.success('已复制') }
  catch {
    try { await appPrompt({ title: '手动复制', message: '长按下面的文本并选择复制。', value: text, confirmText: '完成' }) }
    catch { /* 关闭不影响已创建的分享。 */ }
  }
}
export async function shareLink(url: string, title: string, password?: string) {
  const text = password ? `提取码：${password}` : undefined
  if (navigator.share) {
    try { await navigator.share({ url, title, text }); return }
    catch (error) { if ((error as Error).name === 'AbortError') return }
  }
  await copyText(password ? `${url}  提取码：${password}` : url)
}
