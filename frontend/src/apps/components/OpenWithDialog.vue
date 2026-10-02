<template>
  <ResponsivePanel v-model="visible" title="打开方式" width="460px" align-center>
    <p>{{ target?.name }}</p><p>从文件打开时，应用会将所在目录加入内容来源；目录访问仍需授权。</p>
    <el-alert v-if="error" :title="error" type="error" :closable="false" />
    <div v-loading="loading" class="open-with-list">
      <el-button v-for="app in matches" :key="app.manifest.id" @click="launch(app)">用{{ app.manifest.name }}打开</el-button>
      <el-empty v-if="!loading && !matches.length" description="尚未安装支持此类型的应用" />
    </div>
    <template #footer><el-button @click="center">浏览应用中心</el-button><el-button @click="visible = false">取消</el-button></template>
  </ResponsivePanel>
</template>
<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import ResponsivePanel from '@drive/components/ResponsivePanel.vue'
import { api } from '@drive/api'
const appApi = api.apps
import { errMsg } from '@drive/api'
import type { InstalledApp } from '../types'
import { acceptsFile } from '../integration'
const router = useRouter(), visible = ref(false), loading = ref(false), error = ref('')
const target = ref<{ name: string; is_dir: boolean; path: string }>()
const matches = ref<InstalledApp[]>([])
let from = '/', generation = 0
async function open(file: NonNullable<typeof target.value>, directory: string) {
  const current = ++generation
  target.value = file; from = directory; visible.value = true; loading.value = true; error.value = ''; matches.value = []
  try {
    const { data } = await appApi.list()
    if (current === generation) matches.value = data.installed.filter(app => app.enabled && acceptsFile(app.manifest, file))
  } catch (e) { if (current === generation) error.value = errMsg(e) }
  finally { if (current === generation) loading.value = false }
}
function launch(app: InstalledApp) {
  if (!target.value) return
  visible.value = false
  void router.push({ path: `/apps/${app.manifest.id}`, query: { open: target.value.path, from } })
}
function center() { visible.value = false; void router.push({ path: '/apps', query: { from } }) }
defineExpose({ open })
</script>
<style scoped>.open-with-list { display: flex; flex-direction: column; gap: 12px; min-height: 80px; }.open-with-list .el-button { margin: 0; }</style>
