import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import { api, errMsg, type BackupLastTimes, type Extras, type ExtrasPatch, type InboxRouteRule, type InboxDirectory, type InboxTypeDirectories } from '../../api'
import type DirPicker from '../../components/DirPicker.vue'
import { sizeInput } from './sections'

export type InboxDirKey = 'inbox_dir' | 'inbox_video_dir' | 'inbox_image_dir' | 'inbox_book_dir'
export const INBOX_DIRS: { key: InboxDirKey; typeKey: string; label: string; help: string }[] = [
  {
    key: 'inbox_dir',
    typeKey: 'default',
    label: '默认收件目录',
    help: '未匹配文件使用本组目录；新配置的路径在保存时创建。',
  },
  {
    key: 'inbox_video_dir',
    typeKey: 'video',
    label: '视频目录',
    help: 'mp4、mkv、mov、avi、webm、flv、wmv、ts、m2ts、mpeg、3gp 等。',
  },
  {
    key: 'inbox_image_dir',
    typeKey: 'image',
    label: '图片目录',
    help: 'jpg、png、gif、webp、bmp、avif、heic、tiff、svg 等。',
  },
  {
    key: 'inbox_book_dir',
    typeKey: 'book',
    label: '书籍目录',
    help: 'pdf、epub、mobi、azw3、fb2、djvu、cbz、cbr、txt、rtf 等。',
  },
]

/**
 * 维护类设置(回收站/备份/Bot 收件/后台任务开关),走独立的 extras 保存接口。
 * 首次加载由调用方在 onMounted 里触发,以保持与拆分前一致的请求顺序。
 */
export function useAdminExtras(writesBlocked = () => false, backupsBlocked = writesBlocked) {
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
  let ruleRoutes = new Map<InboxRouteRule, InboxTypeDirectories>()

  function newInboxRoute(key: string, path: string): InboxTypeDirectories {
    return { key, directories: [{ node_id: null, path, automatic: true, missing: false }] }
  }

  function inboxRouteForType(key: string): InboxTypeDirectories {
    const ex = extras.value!
    ex.inbox_routes ??= []
    let route = ex.inbox_routes.find((route) => route.key === key)
    if (!route) {
      const field = INBOX_DIRS.find((field) => field.typeKey === key)
      ex.inbox_routes.push(newInboxRoute(key, field ? ex[field.key] || '/收件箱' : '/收件箱'))
      route = ex.inbox_routes[ex.inbox_routes.length - 1]!
    }
    return route
  }

  function inboxRouteForRule(rule: InboxRouteRule): InboxTypeDirectories {
    const ex = extras.value!
    ex.inbox_routes ??= []
    const key = `custom:${rule.name.trim().toLowerCase()}`
    let route = ruleRoutes.get(rule)
    if (!route) {
      const used = new Set(ruleRoutes.values())
      route = ex.inbox_routes.find((route) => route.key === key && !used.has(route))
      if (!route) {
        ex.inbox_routes.push(newInboxRoute(key, rule.dir || ex.inbox_dir || '/收件箱'))
        route = ex.inbox_routes[ex.inbox_routes.length - 1]!
      }
      ruleRoutes.set(rule, route)
    }
    route.key = key
    return route
  }

  function syncInboxRoutes() {
    if (!extras.value) return
    const ex = extras.value
    const builtin = INBOX_DIRS.map((field) => inboxRouteForType(field.typeKey))
    const custom = ex.inbox_rules.map((rule) => inboxRouteForRule(rule))
    ex.inbox_routes = [...builtin, ...custom]
    INBOX_DIRS.forEach((field, index) => {
      const directory = builtin[index]?.directories.find((directory) => directory.automatic)
      if (directory) ex[field.key] = directory.path
    })
    ex.inbox_rules.forEach((rule, index) => {
      const directory = custom[index]?.directories.find((directory) => directory.automatic)
      if (directory) rule.dir = directory.path
    })
  }

  function setInboxDirectoryPath(directory: InboxDirectory, path: string) {
    if (directory.path === path) return
    directory.path = path
    directory.node_id = null
    directory.missing = false
  }

  function addInboxDirectory(route: InboxTypeDirectories) {
    if (route.directories.length >= 20) return
    route.directories.push({ node_id: null, path: '', automatic: route.directories.length === 0, missing: false })
  }

  function removeInboxDirectory(route: InboxTypeDirectories, index: number) {
    if (route.directories.length <= 1) return
    const automatic = route.directories[index]?.automatic
    route.directories.splice(index, 1)
    if (automatic && route.directories[0]) route.directories[0].automatic = true
  }

  function moveInboxDirectory(route: InboxTypeDirectories, index: number, delta: number) {
    const next = index + delta
    if (next < 0 || next >= route.directories.length) return
    const directory = route.directories.splice(index, 1)[0]!
    route.directories.splice(next, 0, directory)
  }

  function setAutomaticInboxDirectory(route: InboxTypeDirectories, index: number) {
    route.directories.forEach((directory, current) => { directory.automatic = current === index })
  }

  async function chooseInboxDirectory(directory: InboxDirectory) {
    const picked = await inboxDirPicker.value?.open('选择收件目录', directory.path || '/收件箱')
    if (!picked) return
    if (picked === '/') {
      ElMessage.warning('收件目录不能是根目录，请选择或新建一个文件夹')
      return
    }
    // 明确重新选择可替换同一路径下已删除或重新创建的文件夹身份。
    directory.path = picked
    directory.node_id = null
    directory.missing = false
  }

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
    const rule = extras.value?.inbox_rules[index]
    if (rule && extras.value) {
      const route = ruleRoutes.get(rule)
      extras.value.inbox_routes = extras.value.inbox_routes?.filter((item) => item !== route)
      ruleRoutes.delete(rule)
      extras.value.inbox_rules.splice(index, 1)
    }
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
      ruleRoutes = new Map()
      syncInboxRoutes()
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
    if (writesBlocked()) return
    if (!extras.value) return
    // 用户直接点保存时，也要收下输入框里尚未按回车的后缀，避免静默丢失。
    extras.value.inbox_rules.forEach((rule, index) => addRuleExtensions(rule, index))
    syncInboxRoutes()
    const keys = extrasPatchKeys()
    if (!keys.length) return
    const patch = Object.fromEntries(keys.filter(key => key !== 'backup_chunk_size' && key !== 'inbox_routes_revision')
      .map(key => [key, extras.value![key as keyof Extras]])) as ExtrasPatch
    if (keys.includes('backup_chunk_size')) patch.backup_chunk_size = backupChunkSizeInput.value
    if (keys.some(key => key === 'inbox_routes' || key === 'inbox_rules' || INBOX_DIRS.some(field => field.key === key))) {
      patch.inbox_routes_revision = extras.value.inbox_routes_revision
    }
    extrasSaving.value = true
    try {
      await api.admin.saveExtras(patch)
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
      .filter((k) => k !== 'inbox_routes_revision' && JSON.stringify(cur[k]) !== JSON.stringify(org[k]))
      .map(String)
    if (backupChunkSizeInput.value !== backupChunkOrigin.value) keys.push('backup_chunk_size')
    return keys
  }

  async function backupNow() {
    if (backupsBlocked() || backingUp.value) return
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
    inboxRouteForType,
    inboxRouteForRule,
    setInboxDirectoryPath,
    addInboxDirectory,
    removeInboxDirectory,
    moveInboxDirectory,
    setAutomaticInboxDirectory,
    chooseInboxDirectory,
    loadExtras,
    saveExtras,
    hasRuleDraft,
    extrasPatchKeys,
    backupNow,
    downloadBackup,
  }
}
