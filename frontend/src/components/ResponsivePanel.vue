<template>
  <section v-if="inline" class="inline-panel"><header v-if="$slots.header"><slot name="header" /></header><slot /><footer v-if="$slots.footer"><slot name="footer" /></footer></section>
  <el-dialog v-else v-bind="$attrs" v-model="visible" :title="title" :width="mobile ? '100%' : width"
    :fullscreen="mobile && !sheet" :class="['responsive-panel', { 'mobile-sheet': mobile && sheet }]"
    :lock-scroll="false" :align-center="!mobile" append-to-body @closed="$emit('closed')" @close="$emit('close')">
    <template v-if="$slots.header" #header><slot name="header" /></template>
    <slot />
    <template v-if="$slots.footer" #footer><slot name="footer" /></template>
  </el-dialog>
</template>

<script setup lang="ts">
import { watch } from 'vue'
import { mobile } from '../mobile/viewport'
import { useOverlay } from '../mobile/overlays'
defineOptions({ inheritAttrs: false })
const props = withDefaults(defineProps<{ title?: string; width?: string | number; sheet?: boolean; inline?: boolean }>(), { width: '560px' })
const visible = defineModel<boolean>({ default: false })
defineEmits<{ close: []; closed: [] }>()
useOverlay(visible)
watch(() => props.inline, () => { visible.value = false })
</script>

<style>
.responsive-panel .el-dialog__body { min-height: 0; overflow: auto; overscroll-behavior: contain; }
.inline-panel { min-width: 0; }
.inline-panel > header { margin-bottom: 20px; }
@media (max-width: 767px) {
  .el-overlay-dialog:has(.responsive-panel) { overflow: hidden; }
  .responsive-panel.el-dialog { position: absolute; inset: 0; margin: 0; height: 100%; max-height: 100%; display: flex; flex-direction: column; border-radius: 0; padding: max(16px, var(--safe-top)) max(16px, var(--safe-right)) max(16px, var(--safe-bottom)) max(16px, var(--safe-left)); box-sizing: border-box; }
  .responsive-panel .el-dialog__header { flex-shrink: 0; padding-bottom: 16px; padding-right: 44px; }
  .responsive-panel .el-dialog__title { display: block; overflow-wrap: anywhere; font-weight: 650; }
  .responsive-panel .el-dialog__headerbtn { top: max(2px, var(--safe-top)); right: max(2px, var(--safe-right)); width: 44px; height: 44px; }
  .responsive-panel .el-dialog__body { flex: 1; }
  .responsive-panel .el-dialog__footer { flex-shrink: 0; display: flex; flex-wrap: wrap; gap: 8px; padding-top: 16px; }
  .responsive-panel .el-dialog__footer .el-button { margin: 0; flex: 1; }
  .responsive-panel.mobile-sheet { top: auto; height: auto; max-height: calc(100% - max(16px, var(--safe-top))); padding-top: 20px; border-radius: 22px 22px 0 0; }
  .keyboard-open .el-overlay-dialog:has(.responsive-panel) { top: var(--visual-top); height: var(--visual-height); bottom: auto; }
}
</style>
