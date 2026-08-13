import { ref } from 'vue'

const KEY = 'tgdrive.dark'

/** 首次进入:跟随系统;之后记住用户的选择 */
function initial(): boolean {
  const saved = localStorage.getItem(KEY)
  if (saved !== null) return saved === '1'
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false
}

export const isDark = ref(initial())

function apply() {
  // element-plus 的暗色变量挂在 html.dark 上
  document.documentElement.classList.toggle('dark', isDark.value)
}
apply()

export function toggleDark() {
  isDark.value = !isDark.value
  localStorage.setItem(KEY, isDark.value ? '1' : '0')
  apply()
}
