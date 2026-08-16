<template>
  <el-dialog v-model="visible" title="回收站" width="760px" align-center append-to-body>
    <div class="bar">
      <span class="tip">
        删除的内容先进回收站,Telegram 消息在彻底删除时才释放;保留天数可在「系统设置 → 维护」调整。
      </span>
      <el-button
        size="small"
        type="danger"
        plain
        :disabled="!items.length"
        @click="purgeAll"
      >
        清空回收站
      </el-button>
    </div>
    <el-table :data="items" v-loading="loading" size="small">
      <el-table-column label="名称" min-width="200">
        <template #default="{ row }">
          <div class="name-line">
            <el-icon v-if="row.is_dir" color="#f6a821"><Folder /></el-icon>
            <el-icon v-else color="#64748b"><Document /></el-icon>
            <span>{{ row.name }}</span>
          </div>
        </template>
      </el-table-column>
      <el-table-column label="原位置" min-width="160">
        <template #default="{ row }">
          <span class="path">{{ row.orig_path }}</span>
        </template>
      </el-table-column>
      <el-table-column label="大小" width="90">
        <template #default="{ row }">{{ row.is_dir ? '—' : fmtSize(row.size) }}</template>
      </el-table-column>
      <el-table-column label="删除时间" width="150">
        <template #default="{ row }">{{ fmtTime(row.trashed_at) }}</template>
      </el-table-column>
      <el-table-column label="操作" width="140" align="right">
        <template #default="{ row }">
          <el-button text size="small" type="primary" @click="restore(row as TrashItem)">
            恢复
          </el-button>
          <el-button text size="small" type="danger" @click="purge(row as TrashItem)">
            彻底删除
          </el-button>
        </template>
      </el-table-column>
      <template #empty>
        <el-empty description="回收站是空的" :image-size="72" />
      </template>
    </el-table>
  </el-dialog>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import { DeleteFilled, Document, Folder } from '@element-plus/icons-vue'
import { api, errMsg, fmtSize, fmtTime, type TrashItem } from '../api'
import { appConfirm } from '../dialog'

const visible = ref(false)
const loading = ref(false)
const items = ref<TrashItem[]>([])
const emit = defineEmits<{ changed: [] }>()

async function load() {
  loading.value = true
  try {
    items.value = (await api.trash.list()).data.items
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

async function restore(row: TrashItem) {
  try {
    const { data } = await api.trash.restore([row.id])
    if (data.restored.length) {
      ElMessage.success(`已恢复到 ${data.restored[0].path}`)
      emit('changed')
    } else if (data.failed.length) {
      ElMessage.error(data.failed[0].error)
    }
    load()
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

async function purge(row: TrashItem) {
  try {
    await appConfirm({
      title: '彻底删除',
      message: `确定彻底删除「${row.name}」吗?\n对应的 Telegram 消息会一并删除,无法再恢复。`,
      kind: 'danger',
      icon: DeleteFilled,
      confirmText: '彻底删除',
    })
  } catch {
    return
  }
  try {
    await api.trash.purge([row.id])
    ElMessage.success('已彻底删除')
    load()
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

async function purgeAll() {
  try {
    await appConfirm({
      title: '清空回收站',
      message: `确定清空回收站(共 ${items.value.length} 项)吗?\n全部对应的 Telegram 消息都会删除,无法再恢复。`,
      kind: 'danger',
      icon: DeleteFilled,
      confirmText: '全部删除',
    })
  } catch {
    return
  }
  try {
    const { data } = await api.trash.purge([], true)
    ElMessage.success(`已清空(${data.purged} 项)`)
    load()
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

defineExpose({ open })
</script>

<style scoped>
.bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 10px;
}
.tip {
  color: var(--text-soft);
  font-size: 12px;
  line-height: 1.6;
}
.name-line {
  display: flex;
  align-items: center;
  gap: 6px;
}
.path {
  font-size: 12px;
  color: var(--text-soft);
}
</style>
