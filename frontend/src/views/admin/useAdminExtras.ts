import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import { api, errMsg, type BackupLastTimes, type Extras, type InboxRouteRule } from '../../api'
import type DirPicker from '../../components/DirPicker.vue'
import { sizeInput } from './sections'

export type InboxDirKey = 'inbox_dir' | 'inbox_video_dir' | 'inbox_image_dir' | 'inbox_book_dir'
export const INBOX_DIRS: { key: InboxDirKey; label: string; help: string }[] = [
  {
    key: 'inbox_dir',
    label: '默认收件目录',
    help: '没有命中任何类型规则的文件放这里,目录不存在时自动创建。',
  },
  {
    key: 'inbox_video_dir',
    label: '视频目录',
    help: 'mp4、mkv、mov、avi、webm、flv、wmv、ts、m2ts、mpeg、3gp 等。',
  },
  {
    key: 'inbox_image_dir',
    label: '图片目录',
    help: 'jpg、png、gif、webp、bmp、avif、heic、tiff、svg 等。',
  },
  {
    key: 'inbox_book_dir',
    label: '书籍目录',
    help: 'pdf、epub、mobi、azw3、fb2、djvu、cbz、cbr、txt、rtf 等。',
  },
]

/**
 * 维护类设置(回收站/备份/Bot 收件/后台任务开关),走独立的 extras 保存接口。
 * 首次加载由调用方在 onMounted 里触发,以保持与拆分前一致的请求顺序。
 */
export function useAdminExtras() {
  const extras = ref<Extras>()
  /** 加载时的深拷贝快照,用于统计维护类设置改了几项 */
  const extrasOrigin = ref<Extras>()
  const backupChunkSizeInput = ref('18MB')
  const backupChunkOrigin = ref('18MB')
  const backupLastTs = ref<number | null>(null)
  const backupLastByTarget = ref<BackupLastTimes>({ telegram: null, webdav: null, s3: null })
  const trashCount = ref(0)
  const extrasSaving = ref(false)
  const backingUp = ref(false)
  const inboxDirPicker = ref<InstanceType<typeof DirPicker>>()
  const inboxRuleExtensionDrafts = ref<string[]>([])

  async function chooseInboxDir(key: InboxDirKey) {
    if (!extras.value) return
    const label = INBOX_DIRS.find((d) => d.key === key)?.label ?? '收件目录'
    const picked = await inboxDirPicker.value?.open(`选择${label}`, extras.value[key])
    if (!picked) return
    if (picked === '/') {
      ElMessage.warning('收件目录不能是根目录，请选择或新建一个文件夹')
      return
    }
    extras.value[key] = picked
  }

  async function chooseRuleDir(rule: InboxRouteRule) {
    const picked = await inboxDirPicker.value?.open(
      `选择「${rule.name.trim() || '自定义类型'}」的目录`,
      rule.dir || extras.value?.inbox_dir || '/收件箱',
    )
    if (!picked) return
    if (picked === '/') {
      ElMessage.warning('收件目录不能是根目录，请选择或新建一个文件夹')
      return
    }
    rule.dir = picked
  }

  function addInboxRule() {
    if (!extras.value) return
    extras.value.inbox_rules.push({
      name: '',
      extensions: [],
      dir: extras.value.inbox_dir,
    })
    inboxRuleExtensionDrafts.value.push('')
  }

  function removeInboxRule(index: number) {
    extras.value?.inbox_rules.splice(index, 1)
    inboxRuleExtensionDrafts.value.splice(index, 1)
  }

  function addRuleExtensions(rule: InboxRouteRule, index: number) {
    const draft = inboxRuleExtensionDrafts.value[index] ?? ''
    const parts = draft
      .split(/[,;\s]+/)
      .map((value) => value.trim().replace(/^\.+/, '').toLowerCase())
      .filter(Boolean)
    if (!parts.length) return

    const seen = new Set(rule.extensions.map((value) => value.replace(/^\.+/, '').toLowerCase()))
    let added = 0
    for (const extension of parts) {
      if (seen.has(extension)) continue
      rule.extensions.push(extension)
      seen.add(extension)
      added++
    }
    inboxRuleExtensionDrafts.value[index] = ''
    if (!added) ElMessage.info('这些文件后缀已经添加过了')
  }

  function removeRuleExtension(rule: InboxRouteRule, extensionIndex: number) {
    rule.extensions.splice(extensionIndex, 1)
  }

  async function loadExtras() {
    try {
      const { data } = await api.admin.extras()
      extras.value = data.extras
      extrasOrigin.value = JSON.parse(JSON.stringify(data.extras))
      backupChunkSizeInput.value = sizeInput(data.extras.backup_chunk_size)
      backupChunkOrigin.value = backupChunkSizeInput.value
      inboxRuleExtensionDrafts.value = data.extras.inbox_rules.map(() => '')
      backupLastTs.value = data.backup_last_ts
      backupLastByTarget.value = data.backup_last_by_target
      trashCount.value = data.trash_count
    } catch (e) {
      ElMessage.error(errMsg(e))
    }
  }

  async function saveExtras() {
    if (!extras.value) return
    // 用户直接点保存时，也要收下输入框里尚未按回车的后缀，避免静默丢失。
    extras.value.inbox_rules.forEach((rule, index) => addRuleExtensions(rule, index))
    extrasSaving.value = true
    try {
      await api.admin.saveExtras({
        ...extras.value,
        backup_chunk_size: backupChunkSizeInput.value,
      })
      // 这个保存路径被维护、备份、Bot 收件、后台任务几个分区共用,别写死某一个的名字
      ElMessage.success('设置已保存并生效')
      await loadExtras()
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      extrasSaving.value = false
    }
  }

  /** 后缀输入框里还有没敲回车的内容,保存时会一并收下 */
  const hasRuleDraft = () => inboxRuleExtensionDrafts.value.some((d) => d.trim().length > 0)

  function extrasPatchKeys(): string[] {
    const cur = extras.value
    const org = extrasOrigin.value
    if (!cur || !org) return []
    const keys = (Object.keys(cur) as (keyof Extras)[])
      .filter((k) => JSON.stringify(cur[k]) !== JSON.stringify(org[k]))
      .map(String)
    if (backupChunkSizeInput.value !== backupChunkOrigin.value) keys.push('backup_chunk_size')
    return keys
  }

  async function backupNow() {
    backingUp.value = true
    try {
      const { data } = await api.admin.backupNow()
      const succeeded = data.destinations.filter((item) => item.ok).map((item) => item.label)
      const failed = data.destinations
        .filter((item) => !item.ok)
        .map((item) => `${item.label}：${item.error ?? '失败'}`)
      if (data.ok) {
        ElMessage.success(`备份完成：${data.name}（${succeeded.join('、')}）`)
      } else if (data.succeeded > 0) {
        ElMessage.warning(`部分完成：${succeeded.join('、')}；失败：${failed.join('；')}`)
      } else {
        ElMessage.error(`备份失败：${failed.join('；')}`)
      }
      await loadExtras()
    } catch (e) {
      ElMessage.error(errMsg(e))
    } finally {
      backingUp.value = false
    }
  }

  function downloadBackup() {
    window.open(api.admin.backupUrl, '_blank')
  }

  return {
    extras,
    extrasOrigin,
    backupChunkSizeInput,
    backupLastTs,
    backupLastByTarget,
    trashCount,
    extrasSaving,
    backingUp,
    inboxDirPicker,
    inboxRuleExtensionDrafts,
    chooseInboxDir,
    chooseRuleDir,
    addInboxRule,
    removeInboxRule,
    addRuleExtensions,
    removeRuleExtension,
    loadExtras,
    saveExtras,
    hasRuleDraft,
    extrasPatchKeys,
    backupNow,
    downloadBackup,
  }
}
