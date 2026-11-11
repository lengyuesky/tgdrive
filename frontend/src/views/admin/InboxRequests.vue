<template>
  <div class="block inbox-requests">
    <div class="block-head request-head">
      <span>收件待办与记录</span>
      <div class="request-controls">
        <el-radio-group v-model="active" size="small">
          <el-radio-button :value="true">待处理</el-radio-button>
          <el-radio-button :value="false">已结束</el-radio-button>
        </el-radio-group>
        <el-button size="small" :loading="loading" @click="load()">刷新</el-button>
      </div>
    </div>
    <div class="request-summary">
      <el-tag>待处理 {{ total }}</el-tag>
      <el-tag v-if="counts.failed" type="danger">失败 {{ counts.failed }}</el-tag>
      <el-tag v-if="counts.uncertain" type="warning">待核实 {{ counts.uncertain }}</el-tag>
      <span>待确认收件保留 24 小时，已结束记录保留 30 天。</span>
    </div>
    <div v-if="error" class="note warn">{{ error }}</div>
    <div v-if="restore.status.value.safe_mode" class="note warn">恢复安全模式已暂停收件，请在备份页面检查并解除安全模式。</div>
    <div class="request-table">
      <el-table :data="items" v-loading="loading" size="small" empty-text="暂无收件记录">
        <el-table-column label="文件" min-width="180">
          <template #default="{ row }">
            <b class="request-name">{{ row.name }}</b>
            <div v-if="row.original_name !== row.name" class="request-detail request-original">原文件：{{ row.original_name }}</div>
            <div class="request-detail">用户 {{ row.chat_id }}</div>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="100">
          <template #default="{ row }"><el-tag :type="statusType(row.status)">{{ statusLabel(row.status) }}</el-tag></template>
        </el-table-column>
        <el-table-column label="目录与结果" min-width="200">
          <template #default="{ row }">
            <span class="request-path">{{ row.result_path || row.selected_path || '尚未选择目录' }}</span>
            <div v-if="row.last_error" class="request-error">{{ row.last_error }}</div>
            <div v-if="row.target_channel" class="request-detail">频道 {{ row.target_channel }}{{ row.stored_message_id ? ` · 消息 ${row.stored_message_id}` : '' }}</div>
            <div v-if="row.copy_started_at" class="request-detail">转存时间 {{ fmtTime(row.copy_started_at) }}</div>
          </template>
        </el-table-column>
        <el-table-column label="收到时间" width="155">
          <template #default="{ row }">{{ fmtTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" min-width="225">
          <template #default="{ row }">
            <div class="request-actions">
              <el-button v-if="row.can_retry" link type="primary" :disabled="actionsBlocked || !!busyId" @click="act(row.id, 'retry')">重试</el-button>
              <el-button link :disabled="actionsBlocked || !!busyId || row.bot_id !== currentBotId" @click="act(row.id, 'resend')">重发操作消息</el-button>
              <el-button v-if="row.can_cancel" link type="danger" :disabled="actionsBlocked || !!busyId" @click="act(row.id, 'cancel')">{{ row.status === 'uncertain' ? '结束待办' : '取消' }}</el-button>
            </div>
          </template>
        </el-table-column>
      </el-table>
    </div>
    <div class="request-cards">
      <div v-for="row in items" :key="row.id" class="request-card">
        <div class="request-card-head"><b class="request-name">{{ row.name }}</b><el-tag :type="statusType(row.status)">{{ statusLabel(row.status) }}</el-tag></div>
        <div v-if="row.original_name !== row.name" class="request-detail request-original">原文件：{{ row.original_name }}</div>
        <div class="request-detail">用户 {{ row.chat_id }} · {{ fmtTime(row.created_at) }}</div>
        <div class="request-path">{{ row.result_path || row.selected_path || '尚未选择目录' }}</div>
        <div v-if="row.last_error" class="request-error">{{ row.last_error }}</div>
        <div v-if="row.target_channel" class="request-detail">频道 {{ row.target_channel }}{{ row.stored_message_id ? ` · 消息 ${row.stored_message_id}` : '' }}</div>
        <div v-if="row.copy_started_at" class="request-detail">转存时间 {{ fmtTime(row.copy_started_at) }}</div>
        <div class="request-actions">
          <el-button v-if="row.can_retry" link type="primary" :disabled="actionsBlocked || !!busyId" @click="act(row.id, 'retry')">重试</el-button>
          <el-button link :disabled="actionsBlocked || !!busyId || row.bot_id !== currentBotId" @click="act(row.id, 'resend')">重发操作消息</el-button>
          <el-button v-if="row.can_cancel" link type="danger" :disabled="actionsBlocked || !!busyId" @click="act(row.id, 'cancel')">{{ row.status === 'uncertain' ? '结束待办' : '取消' }}</el-button>
        </div>
      </div>
      <p v-if="!items.length && !loading" class="request-detail">暂无收件记录</p>
    </div>
    <el-button v-if="nextCursor" class="request-more" :loading="loading" @click="load(true)">查看更多</el-button>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { api, errMsg, fmtTime, type InboxRequest } from '../../api'
import { appConfirm } from '../../dialog'
import { useAdminContext } from './useAdmin'

const { restore, restoreLocked } = useAdminContext()
const actionsBlocked = computed(() => restoreLocked.value || restore.status.value.safe_mode)
const active = ref(true)
const items = ref<InboxRequest[]>([])
const counts = ref<Record<string, number>>({})
const currentBotId = ref('')
const nextCursor = ref<string | null>(null)
const loading = ref(false)
const busyId = ref('')
const error = ref('')
const total = computed(() => Object.values(counts.value).reduce((sum, count) => sum + count, 0))
let epoch = 0
let expanded = false
let timer: ReturnType<typeof setInterval> | undefined

function statusLabel(status: string) {
  return ({ pending: '待确认', renaming: '等待改名', queued: '等待存储', copying: '正在转存', copied: '正在入库', failed: '失败', uncertain: '待核实', stored: '已存储', cancelled: '已取消', expired: '已过期', invalidated: '已失效' } as Record<string, string>)[status] || status
}
function statusType(status: string): 'success' | 'warning' | 'danger' | 'info' | 'primary' {
  if (status === 'stored') return 'success'
  if (status === 'failed') return 'danger'
  if (status === 'uncertain') return 'warning'
  if (['cancelled', 'expired', 'invalidated'].includes(status)) return 'info'
  return 'primary'
}

async function load(append = false, quiet = false) {
  if (loading.value) return
  const current = epoch
  loading.value = true
  try {
    const { data } = await api.admin.inboxRequests({ active: active.value, limit: 50, ...(append && nextCursor.value ? { cursor: nextCursor.value } : {}) })
    if (current !== epoch) return
    items.value = append ? [...items.value, ...data.items.filter(item => !items.value.some(old => old.id === item.id))] : data.items
    counts.value = data.counts
    currentBotId.value = data.current_bot_id
    nextCursor.value = data.next_cursor
    error.value = ''
    expanded = append
  } catch (failure) {
    if (current === epoch && !quiet) error.value = `收件记录加载失败：${errMsg(failure)}`
  } finally {
    if (current === epoch) loading.value = false
  }
}

async function act(id: string, action: 'retry' | 'resend' | 'cancel') {
  const row = items.value.find(item => item.id === id)
  if (!row) return
  if (actionsBlocked.value || busyId.value) return
  if (action === 'cancel') {
    try {
      await appConfirm({
        title: row.status === 'uncertain' ? '结束待核实收件' : '取消收件',
        message: row.status === 'uncertain'
          ? '请先核实存储频道。结束待办不会删除无法确认的频道副本，确定结束吗？'
          : '取消本次收件？用户原文件消息将保留。',
        kind: 'warning',
      })
    } catch { return }
  }
  busyId.value = row.id
  try {
    if (action === 'retry') await api.admin.inboxRequestRetry(row.id)
    else if (action === 'resend') await api.admin.inboxRequestResend(row.id)
    else await api.admin.inboxRequestCancel(row.id)
    ElMessage.success(action === 'retry' ? '已安排安全重试' : action === 'resend' ? '已安排重发操作消息' : '待办已结束')
    await load()
  } catch (failure) { ElMessage.error(errMsg(failure)) }
  finally { busyId.value = '' }
}

watch(active, () => { epoch++; loading.value = false; expanded = false; items.value = []; nextCursor.value = null; void load() })
onMounted(() => {
  void load()
  timer = setInterval(() => { if (active.value && !expanded && !busyId.value) void load(false, true) }, 5000)
})
onUnmounted(() => { epoch++; if (timer) clearInterval(timer) })
</script>

<style scoped>
.inbox-requests { margin-top: 24px; min-width: 0; }
.request-head, .request-controls, .request-summary, .request-actions, .request-card-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.request-head { justify-content: space-between; }
.request-summary { margin: 12px 0; }
.request-summary > span, .request-detail { color: var(--el-text-color-secondary); font-size: 12px; }
.request-name, .request-path, .request-error, .request-original { overflow-wrap: anywhere; }
.request-name { display: block; }
.request-error { color: var(--el-color-danger); font-size: 12px; margin-top: 5px; }
.request-table { min-width: 0; overflow: auto; }
.request-cards { display: none; }
.request-more { margin-top: 12px; }
@media (max-width: 600px) {
  .request-table { display: none; }
  .request-cards { display: grid; gap: 12px; }
  .request-card { border: 1px solid var(--el-border-color); border-radius: 10px; padding: 12px; }
  .request-card-head { justify-content: space-between; flex-wrap: nowrap; align-items: flex-start; }
  .request-card-head .request-name { min-width: 0; }
  .request-card-head .el-tag { flex-shrink: 0; }
  .request-detail, .request-path, .request-actions { margin-top: 8px; }
  .request-summary { align-items: flex-start; }
}
</style>
