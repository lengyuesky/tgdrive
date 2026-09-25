import { computed, onMounted, onUnmounted, ref, watch, type Ref } from 'vue'
import { ElMessage } from 'element-plus'
import {
  api,
  errMsg,
  type AdminStatus,
  type BackendLog,
  type BackendLogFacets,
  type BackendLogQuery,
} from '../../api'
import type { SectionKey } from './sections'

export const LOG_LEVELS = [
  { value: 'error', label: '错误' },
  { value: 'warn', label: '警告' },
  { value: 'info', label: '信息' },
  { value: 'debug', label: '调试' },
]

export function logLevelLabel(level: string): string {
  return LOG_LEVELS.find((l) => l.value === level)?.label ?? level
}

export function logLevelType(level: string): 'success' | 'warning' | 'danger' | 'info' {
  if (level === 'error') return 'danger'
  if (level === 'warn') return 'warning'
  if (level === 'info') return 'success'
  return 'info'
}

/** 后台日志:分页、筛选、5 秒静默轮询(顺带刷新运行状态) */
export function useBackendLogs(status: Ref<AdminStatus | undefined>, section: Ref<SectionKey>) {
  const backendLogs = ref<BackendLog[]>([])
  /** 请求进行中(含 5 秒静默轮询),只用于防重入 */
  const backendLogsLoading = ref(false)
  /** 仅手动操作时置位:轮询不该让按钮和遮罩每 5 秒闪一下 */
  const backendLogsBusy = ref(false)
  const backendLogsHasMore = ref(false)
  const backendLogsBefore = ref<number>()
  const backendLogsAutoRefresh = ref(true)
  const backendLogFacets = ref<BackendLogFacets>({ categories: [], level_counts: {} })
  const backendLogLevel = ref('')
  const backendLogCategory = ref('')
  const backendLogKeyword = ref('')
  let backendLogsTimer: ReturnType<typeof setInterval> | undefined

  /** 是否有筛选生效;决定空列表该说“没有日志”还是“没有匹配项” */
  const backendLogFiltered = computed(
    () => !!(backendLogLevel.value || backendLogCategory.value || backendLogKeyword.value.trim()),
  )

  function resetBackendLogFilters() {
    backendLogLevel.value = ''
    backendLogCategory.value = ''
    backendLogKeyword.value = ''
  }

  /** 请求代次:筛选变化后,旧条件的响应必须作废,不能拿它覆盖新列表 */
  let backendLogsSeq = 0

  async function loadBackendLogs(reset = true, silent = false, force = false) {
    // force 用于筛选变化:此时哪怕轮询正在飞,也要让新条件的请求发出去
    if (backendLogsLoading.value && !force) return
    if (!reset && !backendLogsHasMore.value) return
    const seq = ++backendLogsSeq
    backendLogsLoading.value = true
    if (!silent) backendLogsBusy.value = true
    try {
      const params: BackendLogQuery = {
        limit: 50,
        level: backendLogLevel.value,
        category: backendLogCategory.value,
        q: backendLogKeyword.value.trim(),
      }
      if (!reset && backendLogsBefore.value) params.before = backendLogsBefore.value
      const [logsResult, statusResult] = await Promise.all([
        api.admin.logs(params),
        api.admin.status(),
      ])
      if (seq !== backendLogsSeq) return
      const items = logsResult.data.items
      backendLogFacets.value = logsResult.data.facets ?? { categories: [], level_counts: {} }
      if (reset) {
        // 轮询若直接覆盖列表,用户点开的“更早日志”会每 5 秒被吞掉一次。
        // 新旧两页有重叠(说明期间新增不超过一页)时只把增量补到最前面,否则才整页替换。
        const known = new Set(backendLogs.value.map((l) => l.id))
        const overlaps = items.some((l) => known.has(l.id))
        if (overlaps && backendLogs.value.length > items.length) {
          backendLogs.value = [...items.filter((l) => !known.has(l.id)), ...backendLogs.value]
        } else {
          backendLogs.value = items
          backendLogsHasMore.value = logsResult.data.has_more
        }
      } else {
        backendLogs.value = [...backendLogs.value, ...items]
        backendLogsHasMore.value = logsResult.data.has_more
      }
      backendLogsBefore.value = backendLogs.value.at(-1)?.id
      status.value = statusResult.data
    } catch (e) {
      if (!silent && seq === backendLogsSeq) ElMessage.error(errMsg(e))
    } finally {
      // 已被更新的请求接管时不要复位状态,否则新请求的 loading 会被旧请求清掉
      if (seq === backendLogsSeq) {
        backendLogsLoading.value = false
        backendLogsBusy.value = false
      }
    }
  }

  async function loadMoreBackendLogs() {
    await loadBackendLogs(false)
  }

  /** 换筛选条件等于换结果集,分页游标必须一起清掉 */
  async function reloadBackendLogs() {
    backendLogs.value = []
    backendLogsBefore.value = undefined
    backendLogsHasMore.value = false
    await loadBackendLogs(true, false, true)
  }

  // 下拉即时生效;关键字防抖 300ms,免得每敲一个字打一次请求
  watch([backendLogLevel, backendLogCategory], () => void reloadBackendLogs())
  let keywordTimer: ReturnType<typeof setTimeout> | undefined
  watch(backendLogKeyword, () => {
    clearTimeout(keywordTimer)
    keywordTimer = setTimeout(() => void reloadBackendLogs(), 300)
  })

  onMounted(() => {
    if (section.value === 'logs') void loadBackendLogs()
    backendLogsTimer = setInterval(() => {
      if (section.value === 'logs' && backendLogsAutoRefresh.value) {
        void loadBackendLogs(true, true)
      }
    }, 5000)
  })

  onUnmounted(() => {
    if (backendLogsTimer) clearInterval(backendLogsTimer)
    clearTimeout(keywordTimer)
  })

  return {
    backendLogs,
    backendLogsBusy,
    backendLogsHasMore,
    backendLogsAutoRefresh,
    backendLogFacets,
    backendLogLevel,
    backendLogCategory,
    backendLogKeyword,
    backendLogFiltered,
    resetBackendLogFilters,
    loadBackendLogs,
    loadMoreBackendLogs,
  }
}
