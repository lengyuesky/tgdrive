<script setup lang="ts">
/** 设置项磁贴:标题 + 来源标签 + 控件 + 说明,统一后台各分区的排版 */
import { RefreshLeft } from '@element-plus/icons-vue'
import type { CfgSource } from '../api'

defineProps<{
  label: string
  /** 有值时右上角显示来源;后台改过(db)时给一键恢复 */
  source?: CfgSource
  help?: string
  /** 控件跟在标题行右侧,适合开关、数字框这类窄控件 */
  inline?: boolean
  /** 两列栅格下占满整行 */
  wide?: boolean
}>()
defineEmits<{ reset: [] }>()

const SOURCE_LABEL: Record<CfgSource, string> = {
  db: '后台已改',
  env: '来自 .env',
  default: '默认值',
}
</script>

<template>
  <div class="field" :class="{ wide }">
    <div class="field-head">
      <span class="field-label">{{ label }}</span>
      <div class="field-meta">
        <el-tag v-if="source" size="small" round :type="source === 'db' ? 'primary' : 'info'">
          {{ SOURCE_LABEL[source] }}
        </el-tag>
        <el-tooltip v-if="source === 'db'" content="恢复为 .env / 默认值">
          <el-button class="field-reset" link :icon="RefreshLeft" @click="$emit('reset')" />
        </el-tooltip>
        <div v-if="inline" class="field-ctl"><slot /></div>
      </div>
    </div>
    <div v-if="!inline" class="field-ctl"><slot /></div>
    <div v-if="help || $slots.help" class="field-help">
      <slot name="help">{{ help }}</slot>
    </div>
  </div>
</template>

<style scoped>
.field {
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-width: 0;
  padding: 12px 14px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.field.wide {
  grid-column: 1 / -1;
}
.field-head {
  display: flex;
  align-items: center;
  gap: 8px;
}
.field-label {
  font-size: 13.5px;
  font-weight: 600;
}
.field-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-left: auto;
}
.field-reset.el-button {
  padding: 0;
  height: auto;
  font-size: 15px;
}
.field-ctl {
  min-width: 0;
}
.field-ctl :deep(.el-input),
.field-ctl :deep(.el-select) {
  width: 100%;
}
.field-help {
  font-size: 12px;
  line-height: 1.7;
  color: var(--text-soft);
}
.field-help :deep(code) {
  padding: 1px 5px;
  border-radius: 5px;
  background: var(--tile-media-bg);
  word-break: break-all;
}
</style>
