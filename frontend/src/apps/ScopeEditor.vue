<template>
  <ResponsivePanel v-model="visible" title="目录授权" width="680px" append-to-body @closed="!visible && finish(null)">
    <p v-if="reason" class="reason">{{ reason }}</p>
    <p class="scope-help">授权跟随目录身份；重叠目录取权限并集，子目录只读不会否决祖先读写。</p>
    <el-radio-group v-model="mode" aria-label="授权范围">
      <el-radio value="none">不授权</el-radio><el-radio value="selected" :disabled="!app?.manifest.files.read">指定目录</el-radio><el-radio value="all" :disabled="!app?.manifest.files.read">全部目录</el-radio>
    </el-radio-group>
    <div v-if="mode === 'all'" class="all-access"><span>全部目录的权限</span><el-select v-model="allAccess"><el-option label="只读" value="read_only"/><el-option label="读写" value="read_write" :disabled="!app?.manifest.files.write"/></el-select></div>
    <template v-if="mode === 'selected'">
      <ul class="grant-list">
        <li v-for="grant in grants" :key="grant.directory_id"><span>{{ paths[grant.directory_id] ?? `目录 ${grant.directory_id}（当前不可见）` }}</span>
          <el-select v-model="grant.access" :aria-label="`目录 ${grant.directory_id} 的权限`"><el-option label="只读" value="read_only"/><el-option label="读写" value="read_write" :disabled="!app?.manifest.files.write"/></el-select>
          <el-button text type="danger" @click="remove(grant.directory_id)">移除</el-button>
        </li>
      </ul>
      <details class="scope-tree"><summary>添加授权目录</summary><button class="root-choice" @click="add('/')">选择根目录</button><VirtualDirectoryTree current="/" @select="add" /></details>
    </template>
    <p v-if="app?.manifest.files.write" class="scope-help">读写是授权级别。本阶段只开放读取，应用不能绕过宿主尚未开放的写入能力。</p>
    <el-alert v-if="error" :title="error" type="error" :closable="false" />
    <template #footer><el-button @click="finish(null)">取消</el-button><el-button type="primary" :loading="busy" :disabled="mode === 'selected' && grants.length === 0" @click="confirm">确认授权</el-button></template>
  </ResponsivePanel>
</template>
<script setup lang="ts">
import { ref } from 'vue'
import ResponsivePanel from '../components/ResponsivePanel.vue'
import VirtualDirectoryTree from '../components/VirtualDirectoryTree.vue'
import { center } from './client'
import { LIMITS, type InstalledApp, type ScopeInfo, type AccessLevel, type DirectoryGrant, type DirectoryScope } from './contract'
const visible = ref(false), busy = ref(false), error = ref(''), reason = ref('')
const app = ref<InstalledApp>(), mode = ref<DirectoryScope['mode']>('none'), allAccess = ref<AccessLevel>('read_only')
const grants = ref<DirectoryGrant[]>([]), paths = ref<Record<string, string>>({})
let resolver: ((scope: ScopeInfo | null) => void) | undefined
function finish(scope: ScopeInfo | null) { visible.value = false; resolver?.(scope); resolver = undefined }
async function open(value: InstalledApp, message = '', preferred: AccessLevel = 'read_only'): Promise<ScopeInfo | null> {
  finish(null); error.value = ''; reason.value = message; app.value = await center.app(value.manifest.id)
  const scope = app.value.scope.scope; mode.value = scope.mode
  allAccess.value = scope.mode === 'all' ? scope.access : preferred === 'read_write' && app.value.manifest.files.write ? preferred : 'read_only'
  grants.value = scope.mode === 'selected' ? scope.directories.map((grant) => ({ ...grant })) : []
  paths.value = Object.fromEntries(app.value.scope.roots.filter((root) => root.node).map((root) => [root.directory_id, root.node!.path]))
  visible.value = true; return new Promise((resolve) => { resolver = resolve })
}
async function add(path: string) {
  try {
    if (grants.value.length >= LIMITS.MAX_QUERY_ROOTS) throw new Error('授权目录数量已达到上限')
    const node = await center.node({ by: 'path', path })
    if (node.kind !== 'directory') throw new Error('只能选择目录')
    paths.value[node.id] = node.path
    if (!grants.value.some((grant) => grant.directory_id === node.id)) grants.value.push({ directory_id: node.id, access: allAccess.value })
  } catch (failure) { error.value = failure instanceof Error ? failure.message : '目录读取失败' }
}
function remove(id: string) { grants.value = grants.value.filter((grant) => grant.directory_id !== id) }
async function confirm() {
  if (!app.value) return; busy.value = true; error.value = ''
  try {
    const scope: DirectoryScope = mode.value === 'none' ? { mode: 'none' } : mode.value === 'all' ? { mode: 'all', access: allAccess.value } : { mode: 'selected', directories: grants.value }
    const result = await center.scope(app.value.manifest.id, { expected_revision: app.value.scope.revision, scope }); finish(result)
  } catch (failure) { error.value = failure instanceof Error ? failure.message : '授权失败，请重新打开设置' }
  finally { busy.value = false }
}
defineExpose({ open, close: () => finish(null) })
</script>
<style scoped>
.reason { white-space: pre-wrap; }.scope-help { color: var(--el-text-color-secondary); font-size: 13px; line-height: 1.6; }.all-access { display: flex; align-items: center; gap: 16px; margin-top: 16px; }.all-access .el-select { width: 130px; }.grant-list { list-style: none; padding: 0; }.grant-list li { display: flex; gap: 10px; align-items: center; padding: 10px 0; border-bottom: 1px solid var(--glass-border); }.grant-list li > span { flex: 1; overflow-wrap: anywhere; }.grant-list .el-select { width: 108px; flex-shrink: 0; }.scope-tree { border: 1px solid var(--glass-border); padding: 12px; border-radius: 12px; }.scope-tree summary { cursor: pointer; }.root-choice { border: 0; padding: 10px; background: transparent; color: inherit; cursor: pointer; }
</style>
