<template>
  <ResponsivePanel :model-value="modelValue" :title="`${app?.manifest.name ?? '应用'}设置`" width="min(540px, 94vw)"
    align-center append-to-body @update:model-value="emit('update:modelValue', $event)">
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon />
    <div v-loading="loading" class="settings-fields">
      <el-empty v-if="!loading && !app?.manifest.settings.length && !scoped && !showCovers" description="此应用没有可配置项" />
      <div v-for="field in app?.manifest.settings ?? []" :key="field.key" class="app-field">
        <label :for="`app-setting-${field.key}`">{{ field.label }}</label>
        <el-switch v-if="field.type === 'boolean'" :id="`app-setting-${field.key}`"
          :model-value="Boolean(values[field.key])" @update:model-value="values[field.key] = Boolean($event)" />
        <el-input-number v-else-if="field.type === 'number'" :id="`app-setting-${field.key}`"
          :model-value="Number(values[field.key])" @update:model-value="values[field.key] = $event ?? 0" />
        <div v-else class="app-field-row">
          <el-input :id="`app-setting-${field.key}`" :model-value="String(values[field.key] ?? '')"
            @update:model-value="values[field.key] = $event" />
          <el-button v-if="field.type === 'directory'" @click="chooseDirectory(field.key, field.label)">选择目录</el-button>
        </div>
        <p v-if="field.description">{{ field.description }}</p>
      </div>
      <div v-if="scoped" class="app-field scope-field">
        <label>目录范围</label>
        <div class="scope-head">
          <el-select v-model="scopeMode"><el-option label="未授权" value="none" /><el-option label="指定目录" value="selected" /><el-option label="全盘访问" value="all" /></el-select>
        </div>
        <div v-if="scopeMode === 'selected'" class="scope-body">
          <div v-for="(path, index) in scopePaths" :key="path" class="scope-row">
            <code>{{ path }}</code>
            <el-button text type="danger" :icon="Delete" @click="scopePaths.splice(index, 1)">移除</el-button>
          </div>
          <el-button :icon="FolderAdd" @click="addScopeDir">添加目录</el-button>
          <p class="scope-hint">至少选择一个目录；应用只能列出、搜索和读取这些目录之内的文件。</p>
        </div>
      </div>
      <div v-if="showCovers && coverStats" class="app-field cover-field">
        <label>封面缓存</label>
        <p class="cover-usage">已缓存 {{ coverStats.entries }} 张 · {{ fmtSize(coverStats.bytes) }}（上限 {{ fmtSize(coverStats.limit_bytes) }}）</p>
        <p>封面生成一次后保存在服务器封面库，下次打开直接显示；超出上限时自动淘汰最久没看的封面。清理不影响设置、进度和书签。</p>
        <el-button :icon="Delete" :loading="clearing" :disabled="!coverStats.entries" @click="clearCovers">清理封面缓存</el-button>
      </div>
    </div>
    <template #footer>
      <el-button @click="emit('update:modelValue', false)">取消</el-button>
      <el-button type="primary" :loading="saving" :disabled="loading || !!error" @click="save">保存设置</el-button>
    </template>
    <DirPicker ref="picker" />
  </ResponsivePanel>
</template>

<script setup lang="ts">
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { computed, onUnmounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { Delete, FolderAdd } from '@element-plus/icons-vue'
import { api, errMsg, fmtSize } from '@drive/api'
import { appConfirm } from '@drive/dialog'
import type { CoverStats, InstalledApp, ScopeMode, SettingValues } from '../types'
import DirPicker from '@drive/components/DirPicker.vue'

const props = defineProps<{ modelValue: boolean; app: InstalledApp | null }>()
const emit = defineEmits<{ 'update:modelValue': [value: boolean]; saved: [values: SettingValues]; 'scope-saved': [paths: string[]] }>()
const loading = ref(false)
const saving = ref(false)
const error = ref('')
const values = ref<SettingValues>({})
const scopePaths = ref<string[]>([])
const scopeMode = ref<ScopeMode>('none')
let originalMode: ScopeMode = 'none'
const picker = ref<InstanceType<typeof DirPicker>>()
const scoped = computed(() => !!props.app?.manifest.permissions.includes('files.read'))
const coverStats = ref<CoverStats | null>(null)
const clearing = ref(false)
// 封面缓存只对 v2 媒体类应用有意义；已有缓存时始终显示，便于清理。
const showCovers = computed(() => !!coverStats.value && (coverStats.value.entries > 0
  || (props.app?.manifest.api_version ?? 1) >= 2 && !!props.app?.manifest.permissions.includes('media.read')))
let original: SettingValues = {}
let originalScope: string[] = []
let generation = 0
watch(() => [props.modelValue, props.app?.revision], async () => {
  const current = ++generation
  saving.value = false
  clearing.value = false
  if (!props.modelValue || !props.app) return
  loading.value = true
  error.value = ''
  coverStats.value = null
  void loadCoverStats(current)
  try {
    const { data } = await api.apps.settings(props.app.manifest.id)
    if (current !== generation) return
    original = { ...data }
    values.value = { ...data }
    originalScope = [...(props.app.scope ?? [])]
    scopePaths.value = [...(props.app.scope ?? [])]
    scopeMode.value = props.app.scope_mode ?? (originalScope.length ? 'selected' : 'all')
    originalMode = scopeMode.value
  } catch (e) { if (current === generation) error.value = errMsg(e) }
  finally { if (current === generation) loading.value = false }
}, { immediate: true })
onUnmounted(() => { generation++ })
async function loadCoverStats(current: number) {
  if (!props.app) return
  try {
    const { data } = await api.apps.coverStats(props.app.manifest.id)
    if (current === generation) coverStats.value = data
  } catch { /* 统计失败时隐藏封面缓存一栏，不影响设置保存。 */ }
}
async function clearCovers() {
  if (!props.app || clearing.value) return
  const current = generation
  try {
    await appConfirm({ title: '清理封面缓存', message: `清理「${props.app.manifest.name}」的全部封面缓存？下次浏览时会重新生成封面，不影响设置、进度和书签。`, kind: 'warning', confirmText: '清理' })
  } catch { return }
  if (current !== generation || !props.app) return
  clearing.value = true
  try {
    const { data } = await api.apps.clearCovers(props.app.manifest.id)
    if (current !== generation) return
    ElMessage.success(`已清理 ${data.removed} 张封面`)
    await loadCoverStats(current)
  } catch (e) { if (current === generation) ElMessage.error(errMsg(e)) }
  finally { if (current === generation) clearing.value = false }
}
async function chooseDirectory(key: string, label: string) {
  const current = generation
  const result = await picker.value?.open(`选择${label}`, String(values.value[key] ?? '/'))
  if (current === generation && result != null) values.value[key] = result
}
async function addScopeDir() {
  const current = generation
  const result = await picker.value?.open('添加允许访问的目录', '/')
  if (current === generation && result != null && !scopePaths.value.includes(result)) scopePaths.value.push(result)
}
async function save() {
  if (!props.app || saving.value) return
  const current = generation
  saving.value = true
  try {
    const patch = Object.fromEntries(Object.entries(values.value).filter(([key, value]) => value !== original[key]))
    const paths = scopeMode.value === 'selected' ? [...scopePaths.value] : []
    if (scopeMode.value === 'selected' && !paths.length) throw new Error('请至少选择一个目录，或明确选择未授权')
    const scopeDirty = scopeMode.value !== originalMode || paths.length !== originalScope.length || paths.some((path, index) => path !== originalScope[index])
    const [savedSettings] = await Promise.all([
      Object.keys(patch).length ? api.apps.saveSettings(props.app.manifest.id, patch) : Promise.resolve(null),
      scopeDirty ? api.apps.saveScope(props.app.manifest.id, paths, scopeMode.value) : Promise.resolve(null),
    ])
    if (current !== generation) return
    if (savedSettings) emit('saved', savedSettings.data)
    if (scopeDirty) emit('scope-saved', paths)
    emit('update:modelValue', false)
    ElMessage.success('应用设置已保存')
  } catch (e) { if (current === generation) ElMessage.error(errMsg(e)) }
  finally { if (current === generation) saving.value = false }
}
</script>

<style scoped>
.settings-fields { min-height: 90px; }
.app-field { display: flex; flex-direction: column; align-items: flex-start; gap: 9px; margin: 20px 0; }
.app-field label { font-weight: 600; }
.app-field-row { display: flex; gap: 8px; width: 100%; }
.app-field p { color: var(--el-text-color-secondary); font-size: 13px; margin: 0; line-height: 1.6; }
.scope-field { border-top: 1px solid var(--glass-border); padding-top: 18px; }
.scope-head { width: 100%; }
.scope-body { width: 100%; display: flex; flex-direction: column; align-items: flex-start; gap: 8px; }
.scope-row { display: flex; align-items: center; justify-content: space-between; gap: 8px; width: 100%; }
.scope-row code { word-break: break-all; }
.scope-hint { color: var(--el-text-color-secondary); font-size: 12px; margin: 0; }
.cover-field { border-top: 1px solid var(--glass-border); padding-top: 18px; }
.app-field p.cover-usage { color: var(--el-text-color-primary); font-size: 14px; }
</style>
