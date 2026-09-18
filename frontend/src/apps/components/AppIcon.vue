/** 应用图标：安装后的应用显示包内 SVG（img 上下文不执行脚本）；缺失或加载失败回退字母渐变。 */
<template>
  <img v-if="src && !failed" class="app-icon-img" :src="src" :alt="manifest.name" @error="failed = true" />
  <span v-else class="app-icon-letter" :style="appIconStyle(manifest.id)">{{ letter }}</span>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { appIconStyle } from '../icon'
import type { AppManifest } from '../types'

const props = defineProps<{ manifest: AppManifest; revision?: string }>()
const failed = ref(false)
const letter = computed(() => Array.from(props.manifest.name)[0] ?? '')
// 目录中未安装应用没有本地包字节：仅在提供修订时生成图标地址。
const src = computed(() =>
  props.manifest.icon && props.revision
    ? `/api/apps/runtime/${encodeURIComponent(props.manifest.id)}/${encodeURIComponent(props.revision)}/${encodeURIComponent(props.manifest.icon)}`
    : '',
)
watch(() => [props.manifest.id, props.revision], () => { failed.value = false })
</script>

<style scoped>
.app-icon-img { display: block; width: 100%; height: 100%; object-fit: contain; border-radius: inherit; }
.app-icon-letter { display: grid; place-items: center; width: 100%; height: 100%; color: #fff; font-weight: 600; line-height: 1; text-transform: uppercase; }
</style>
