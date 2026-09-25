import { computed, onMounted, onUnmounted, ref, type Component, type Ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Delete } from '@element-plus/icons-vue'
import { appConfirm } from '../../dialog'
import {
  api,
  errMsg,
  type AdminConfig,
  type AdminStatus,
  type Extras,
  type QueueRunSummary,
  type StorageMigrationStatus,
} from '../../api'
import type { SectionKey } from './sections'

// ---- 后台任务:Telegram 消息删除队列的自动/手动与手动执行 ----
export type QueueKey = 'delete'

export const QUEUES: {
  key: QueueKey
  label: string
  desc: string
  /** 关掉自动之后会怎样,说清楚代价 */
  cost: string
  icon: Component
}[] = [
  {
    key: 'delete',
    label: 'Telegram 消息清理',
    desc: '文件彻底删除、或上传失败回滚后,删掉存储频道里对应的消息。',
    cost: '不执行的话,这些分块会一直留在频道里占空间,且无法再从网盘找到。',
    icon: Delete,
  },
]

interface Deps {
  cfg: Ref<AdminConfig | undefined>
  status: Ref<AdminStatus | undefined>
  extras: Ref<Extras | undefined>
  section: Ref<SectionKey>
  reloadStatus: () => Promise<void>
  /** 维护类设置里尚未保存的键(用于判断开关是否改了没存) */
  extKeys: () => string[]
}

/** 消息清理队列 + 历史存储迁移 */
export function useTaskQueues({ cfg, status, extras, section, reloadStatus, extKeys }: Deps) {
  /** 手动执行完的结果,只在页面上留着给用户看,不落库 */
  const queueLastRun = ref<Partial<Record<QueueKey, QueueRunSummary>>>({})
  const queueBusy = ref<QueueKey | ''>('')
  const storageMigration = ref<StorageMigrationStatus>()
  const storageMigrationBusy = ref(false)
  let storageMigrationTimer: ReturnType<typeof setInterval> | undefined

  const queueOverview = (_key: QueueKey) => status.value?.delete_queue

  /** 开关绑在 extras 上,和页面其它设置一样走底部「保存并生效」 */
  function queueAuto(_key: QueueKey): boolean {
    if (!extras.value) return false
    return extras.value.auto_delete_message
  }

  function setQueueAuto(_key: QueueKey, value: boolean) {
    if (!extras.value) return
    extras.value.auto_delete_message = value
  }

  /** 开关改了还没保存时,「立即执行」的语义会含糊,先提示去保存 */
  const queueAutoDirty = (_key: QueueKey) => extKeys().includes('auto_delete_message')

  async function runQueue(key: QueueKey) {
    queueBusy.value = key
    try {
      const { data } = await api.admin.runDeleteQueue()
      queueLastRun.value = { ...queueLastRun.value, [key]: data }
      const label = QUEUES.find((q) => q.key === key)!.label
      const more = data.stopped_early ? ',本轮超时收工,可再点一次' : ''
      if (data.completed + data.failed + data.discarded === 0) {
        // 一条都没跑完又是超时收工:队列没空,是 Telegram 迟迟不回,别说成「没有待处理」
        if (data.stopped_early) {
          ElMessage.warning(`${label}:20 秒内一条都没处理完,Telegram 可能不可达,队列原样保留`)
        } else {
          ElMessage.info(`${label}:没有待处理的任务`)
        }
      } else if (data.failed || data.discarded) {
        ElMessage.warning(
          `${label}:成功 ${data.completed} 条,永久跳过 ${data.discarded} 条,失败 ${data.failed} 条${more}`,
        )
      } else {
        ElMessage.success(`${label}:成功处理 ${data.completed} 条${more}`)
      }
      await reloadStatus()
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      queueBusy.value = ''
    }
  }

  async function clearQueue(key: QueueKey) {
    const q = QUEUES.find((item) => item.key === key)!
    const pending = queueOverview(key)?.pending ?? 0
    try {
      await appConfirm({
        title: `清空${q.label}队列?`,
        message: `将丢弃 ${pending} 条待处理任务,且不会再重试。\n${q.cost}`,
        kind: 'warning',
        confirmText: '清空',
      })
    } catch {
      return
    }
    queueBusy.value = key
    try {
      const { data } = await api.admin.clearDeleteQueue()
      queueLastRun.value = { ...queueLastRun.value, [key]: undefined }
      ElMessage.success(`已清空 ${data.cleared} 条任务`)
      await reloadStatus()
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      queueBusy.value = ''
    }
  }

  async function loadStorageMigration(silent = false) {
    if (!cfg.value?.configured) return
    try {
      storageMigration.value = (await api.admin.storageMigration()).data
    } catch (e) {
      if (!silent) ElMessage.error(errMsg(e))
    }
  }

  const storageMigrationPct = computed(() => {
    const migration = storageMigration.value
    if (!migration?.total_messages) return migration?.remaining_messages === 0 ? 100 : 0
    return Math.min(100, Math.round((migration.migrated_messages / migration.total_messages) * 100))
  })

  async function startStorageMigration() {
    const migration = storageMigration.value
    if (!migration?.remaining_messages) {
      ElMessage.info('没有需要迁移的历史消息')
      return
    }
    try {
      await appConfirm({
        title: migration.status === 'failed' ? '重试历史存储迁移?' : '开始历史存储迁移?',
        message:
          `将通过主 Bot 在 Telegram 服务端复制 ${migration.remaining_messages} 条历史消息，` +
          `并切换 ${migration.remaining_chunks} 个分块引用。\n` +
          '旧消息会保留，不会自动删除；用户上传期间迁移会主动让路。',
        kind: 'warning',
        confirmText: migration.status === 'failed' ? '重试' : '开始迁移',
      })
    } catch {
      return
    }
    storageMigrationBusy.value = true
    try {
      storageMigration.value = (await api.admin.startStorageMigration()).data
      ElMessage.success('历史存储迁移已在后台启动')
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      storageMigrationBusy.value = false
    }
  }

  async function pauseStorageMigration() {
    storageMigrationBusy.value = true
    try {
      storageMigration.value = (await api.admin.pauseStorageMigration()).data
      ElMessage.info('已请求暂停，当前复制消息完成后生效')
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      storageMigrationBusy.value = false
    }
  }

  onMounted(() => {
    storageMigrationTimer = setInterval(() => {
      if (section.value === 'queues') void loadStorageMigration(true)
    }, 5000)
  })
  onUnmounted(() => {
    if (storageMigrationTimer) clearInterval(storageMigrationTimer)
  })

  return {
    queueLastRun,
    queueBusy,
    queueOverview,
    queueAuto,
    setQueueAuto,
    queueAutoDirty,
    runQueue,
    clearQueue,
    storageMigration,
    storageMigrationBusy,
    storageMigrationPct,
    loadStorageMigration,
    startStorageMigration,
    pauseStorageMigration,
  }
}
