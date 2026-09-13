<template>
  <ResponsivePanel :model-value="modelValue" :title="`${app?.manifest.name ?? '应用'}设置`" width="min(540px, 94vw)"
    align-center append-to-body @update:model-value="emit('update:modelValue', $event)">
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon />
    <div v-loading="loading" class="settings-fields">
      <el-empty v-if="!loading && !app?.manifest.settings.length" description="此应用没有可配置项" />
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
import { onUnmounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { api, errMsg } from '@drive/api'
import type { InstalledApp, SettingValues } from '../types'
import DirPicker from '@drive/components/DirPicker.vue'

const props = defineProps<{ modelValue: boolean; app: InstalledApp | null }>()
const emit = defineEmits<{ 'update:modelValue': [value: boolean]; saved: [values: SettingValues] }>()
const loading = ref(false)
const saving = ref(false)
const error = ref('')
const values = ref<SettingValues>({})
const picker = ref<InstanceType<typeof DirPicker>>()
let original: SettingValues = {}
let generation = 0
watch(() => [props.modelValue, props.app?.revision], async () => {
  const current = ++generation
  saving.value = false
  if (!props.modelValue || !props.app) return
  loading.value = true
  error.value = ''
  try {
    const { data } = await api.apps.settings(props.app.manifest.id)
    if (current !== generation) return
    original = { ...data }
    values.value = { ...data }
  } catch (e) { if (current === generation) error.value = errMsg(e) }
  finally { if (current === generation) loading.value = false }
}, { immediate: true })
onUnmounted(() => { generation++ })
async function chooseDirectory(key: string, label: string) {
  const current = generation
  const result = await picker.value?.open(`选择${label}`, String(values.value[key] ?? '/'))
  if (current === generation && result != null) values.value[key] = result
}
async function save() {
  if (!props.app || saving.value) return
  const current = generation
  saving.value = true
  try {
    const patch = Object.fromEntries(Object.entries(values.value).filter(([key, value]) => value !== original[key]))
    const { data } = await api.apps.saveSettings(props.app.manifest.id, patch)
    if (current !== generation) return
    emit('saved', data)
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
</style>
