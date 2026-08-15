<template>
  <el-dialog v-model="visible" title="分享管理" width="820px" append-to-body>
    <el-table :data="items" v-loading="loading" size="small">
      <el-table-column label="内容" min-width="200">
        <template #default="{ row }">
          <div class="name-line">
            <el-icon v-if="row.is_dir" color="#f6a821"><Folder /></el-icon>
            <el-icon v-else color="#64748b"><Document /></el-icon>
            <span :title="row.path">{{ row.name }}</span>
          </div>
          <div class="path">{{ row.path }}</div>
        </template>
      </el-table-column>
      <el-table-column label="提取码" width="80">
        <template #default="{ row }">{{ row.password ?? '—' }}</template>
      </el-table-column>
      <el-table-column label="有效期" width="130">
        <template #default="{ row }">
          <el-text v-if="row.expires_at && row.expires_at < now" type="danger">已过期</el-text>
          <span v-else-if="row.expires_at">{{ fmtTime(row.expires_at) }}</span>
          <span v-else>永久</span>
        </template>
      </el-table-column>
      <el-table-column label="下载" width="80">
        <template #default="{ row }">
          {{ row.downloads }}{{ row.max_downloads ? ` / ${row.max_downloads}` : '' }}
        </template>
      </el-table-column>
      <el-table-column label="操作" width="150" align="right">
        <template #default="{ row }">
          <el-button text size="small" type="primary" @click="copy(row as ShareItem)">
            复制链接
          </el-button>
          <el-button text size="small" type="danger" @click="remove(row as ShareItem)">
            取消分享
          </el-button>
        </template>
      </el-table-column>
      <template #empty>
        <el-empty description="还没有分享;在文件的「更多」菜单里创建" :image-size="72" />
      </template>
    </el-table>
  </el-dialog>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Document, Folder } from '@element-plus/icons-vue'
import { api, errMsg, fmtTime, type ShareItem } from '../api'

const visible = ref(false)
const loading = ref(false)
const items = ref<ShareItem[]>([])
const now = computed(() => Math.floor(Date.now() / 1000))

async function load() {
  loading.value = true
  try {
    items.value = (await api.shares.list()).data.items
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    loading.value = false
  }
}

function open() {
  visible.value = true
  load()
}

async function copy(row: ShareItem) {
  const url = `${location.origin}/s/${row.token}`
  const text = row.password ? `${url}  提取码: ${row.password}` : url
  try {
    await navigator.clipboard.writeText(text)
    ElMessage.success('已复制')
  } catch {
    ElMessage.warning('浏览器不允许自动复制,请手动复制:' + text)
  }
}

async function remove(row: ShareItem) {
  try {
    await ElMessageBox.confirm(`取消「${row.name}」的分享?已发出的链接将立即失效。`, '取消分享', {
      type: 'warning',
      appendTo: 'body',
    })
  } catch {
    return
  }
  try {
    await api.shares.remove(row.id)
    ElMessage.success('已取消')
    load()
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

defineExpose({ open })
</script>

<style scoped>
.name-line {
  display: flex;
  align-items: center;
  gap: 6px;
}
.path {
  font-size: 12px;
  color: var(--text-soft);
  margin-left: 20px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
