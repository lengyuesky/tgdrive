<template>
  <ResponsivePanel v-model="visible" :inline="inline" title="上传队列" width="460px">
    <p class="transfer-explanation">管理上传任务，下载由浏览器或系统接管。锁屏或切到后台可能暂停上传，请返回此页查看实际状态。</p>
    <el-alert v-if="syncError" :title="syncError" type="warning" :closable="false" />
    <el-button text :loading="syncing" @click="reconcile">核实服务器进度</el-button>
    <template #header>
      <div class="head">
        <b>上传队列</b>
        <div class="spacer" />
        <el-button
          v-if="finishedExists"
          text
          size="small"
          :disabled="batchBusy"
          @click="clearFinished"
        >
          清除已完成
        </el-button>
      </div>
    </template>

    <div v-if="totalCount" class="queue-summary" aria-label="上传任务统计">
      <div class="summary-item">
        <strong class="summary-value">{{ totalCount }}</strong>
        <span class="summary-label">总任务</span>
      </div>
      <div class="summary-item summary-success">
        <strong class="summary-value">{{ successCount }}</strong>
        <span class="summary-label">成功</span>
      </div>
      <div class="summary-item summary-skipped">
        <strong class="summary-value">{{ skippedCount }}</strong>
        <span class="summary-label">跳过</span>
      </div>
      <div class="summary-item summary-error">
        <strong class="summary-value">{{ errorCount }}</strong>
        <span class="summary-label">失败</span>
      </div>
    </div>

    <div v-if="totalCount || recentSuccessCount" class="queue-rate" aria-live="polite">
      <span>最近 1 分钟上传成功</span>
      <strong>{{ recentSuccessCount }}</strong>
      <span>个</span>
    </div>

    <div v-if="totalCount" class="queue-breakdown">
      <el-tag v-if="uploadedCount" size="small" type="success" effect="plain" round>
        普通上传 {{ uploadedCount }}
      </el-tag>
      <el-tag v-if="instantCount" size="small" type="success" effect="plain" round>
        秒传 {{ instantCount }}
      </el-tag>
      <el-tag v-if="activeCount" size="small" type="warning" effect="plain" round>
        处理中 {{ activeCount }}
      </el-tag>
      <el-tag v-if="pausedCount" size="small" type="info" effect="plain" round>
        已暂停 {{ pausedCount }}
      </el-tag>
      <el-tag v-if="needsFileCount" size="small" type="warning" effect="plain" round>
        待选原文件 {{ needsFileCount }}
      </el-tag>
      <el-tag v-if="cancellingCount" size="small" type="info" effect="plain" round>
        取消中 {{ cancellingCount }}
      </el-tag>
    </div>

    <div v-if="pendingCount" class="queue-actions">
      <div v-if="needsFileCount" class="queue-status">
        <span class="queue-hint">需逐项选择原文件后才能继续</span>
      </div>
      <div class="queue-buttons">
        <el-button
          v-if="pauseableCount"
          size="small"
          plain
          :icon="VideoPause"
          :disabled="batchBusy"
          @click="pauseAll"
        >
          全部暂停
        </el-button>
        <el-button
          v-if="resumableCount"
          size="small"
          type="primary"
          plain
          :icon="VideoPlay"
          :disabled="batchBusy"
          @click="resumeAll"
        >
          全部继续
        </el-button>
        <el-button
          v-if="retryableCount"
          size="small"
          plain
          :icon="Refresh"
          :disabled="batchBusy"
          @click="retryAll"
        >
          重试失败
        </el-button>
        <el-button
          size="small"
          type="danger"
          plain
          :icon="Close"
          :loading="batchBusy"
          :disabled="batchBusy"
          @click="cancelAll"
        >
          取消全部
        </el-button>
      </div>
    </div>

    <div v-if="preparingFileCount" class="window-hint">
      正在分批整理并检查 {{ preparingFileCount }} 个文件，页面仍可继续操作…
    </div>
    <div v-if="chunkedExclusiveBusy" class="window-hint">
      多分块大文件独占上传中，其余任务暂缓，完成后自动继续
    </div>
    <div v-if="totalCount === 0 && !preparingFileCount" class="empty">暂无上传任务</div>
    <div v-if="hiddenTaskCount" class="window-hint">
      队列共 {{ totalCount }} 项；当前上传中的任务置顶，另展示待处理的前
      {{ DISPLAY_PENDING_LIMIT }} 项
    </div>
    <div
      v-for="it in displayedItems"
      :key="it.id"
      class="upload-item"
      :class="`tone-${tone(it)}`"
    >
      <div class="item-top">
        <el-tag v-if="it.status === 'instant'" size="small" type="success" class="badge">秒传</el-tag>
        <el-tag v-else-if="it.status === 'skipped'" size="small" type="info" class="badge">跳过</el-tag>
        <el-tag v-else-if="it.session" size="small" type="warning" class="badge">可续传</el-tag>
        <span class="item-name" :title="displayName(it)">{{ displayName(it) }}</span>
        <span v-if="!isDone(it) && it.status !== 'error'" class="item-pct">{{ it.pct }}%</span>
      </div>

      <el-progress
        v-if="it.status !== 'skipped'"
        class="item-bar"
        :percentage="it.pct"
        :stroke-width="6"
        :show-text="false"
        :status="it.status === 'error' ? 'exception' : isDone(it) ? 'success' : undefined"
      />

      <div class="item-foot">
        <span class="state">
          <span v-if="it.status === 'hashing'">校验中(秒传检测),可暂停…</span>
          <span v-else-if="it.status === 'waiting'">排队中</span>
          <span v-else-if="it.status === 'uploading' && telegramWaitSeconds(it) > 0">
            Telegram 限流，约 {{ telegramWaitSeconds(it) }} 秒后自动重试…
          </span>
          <span v-else-if="it.status === 'uploading'">上传中,可随时暂停</span>
          <span v-else-if="it.status === 'paused'">已暂停,可继续上传</span>
          <span v-else-if="it.status === 'needs_file'">请选择原文件继续</span>
          <span v-else-if="it.status === 'cancelling'">正在清理已上传分块…</span>
          <span v-else-if="it.status === 'done'">上传成功</span>
          <span v-else-if="it.status === 'instant'">秒传成功</span>
          <span v-else-if="it.status === 'skipped'">已跳过(目录里有同名文件)</span>
          <span v-else-if="it.status === 'error'">上传失败</span>
        </span>
        <span v-if="it.size" class="size">{{ fmtSize(it.size) }}</span>
        <div class="spacer" />
        <div class="item-btns">
          <el-button
            v-if="isPauseable(it)"
            text
            size="small"
            :disabled="batchBusy"
            @click="pause(it)"
          >
            暂停
          </el-button>
          <el-button
            v-else-if="it.status === 'needs_file'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="chooseFile(it)"
          >
            选择文件
          </el-button>
          <el-button
            v-else-if="it.status === 'paused'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="resume(it)"
          >
            继续
          </el-button>
          <el-button
            v-else-if="it.status === 'error'"
            text
            size="small"
            type="primary"
            :disabled="batchBusy"
            @click="retry(it)"
          >
            重试
          </el-button>
          <el-button
            v-if="!isDone(it) && it.status !== 'cancelling'"
            text
            size="small"
            type="danger"
            :disabled="batchBusy"
            @click="cancelItem(it)"
          >
            取消
          </el-button>
        </div>
      </div>

      <div v-if="it.error" class="upload-err" :title="it.error">{{ it.error }}</div>
    </div>
  </ResponsivePanel>

  <!-- 上传前发现同名文件时的三选一;X/ESC 关闭视为取消 -->
  <ResponsivePanel
    v-model="conflictVisible"
    title="同名文件已存在"
    width="440px"
    append-to-body
    @close="onConflictClose"
  >
    <p class="conflict-hint">目标目录已有 {{ conflictTotal }} 个同名文件,覆盖会替换原文件:</p>
    <ul class="conflict-list">
      <li v-for="n in conflictNames" :key="n" :title="n">{{ n }}</li>
    </ul>
    <p v-if="conflictTotal > conflictNames.length" class="conflict-hint">
      仅列出前 {{ conflictNames.length }} 项，其余 {{ conflictTotal - conflictNames.length }} 项已折叠
    </p>
    <template #footer>
      <el-button @click="resolveConflict('cancel')">取消上传</el-button>
      <el-button @click="resolveConflict('skip')">跳过这些</el-button>
      <el-button type="primary" @click="resolveConflict('overwrite')">全部覆盖</el-button>
    </template>
  </ResponsivePanel>
</template>

<script lang="ts">
export type { UploadFile, UploadCandidate } from '../transfers/controller'
</script>
<script setup lang="ts">
import { Close, Refresh, VideoPause, VideoPlay } from '@element-plus/icons-vue'
import { fmtSize } from '../api'
import { useTransfers } from '../transfers/context'
import ResponsivePanel from './ResponsivePanel.vue'
defineProps<{ inline?: boolean }>()
const { reconcile, syncError, syncing, visible, batchBusy, totalCount, successCount, skippedCount, errorCount, recentSuccessCount, uploadedCount, instantCount, activeCount, pausedCount, needsFileCount, cancellingCount, pendingCount, pauseableCount, resumableCount, retryableCount, preparingFileCount, chunkedExclusiveBusy, hiddenTaskCount, DISPLAY_PENDING_LIMIT, displayedItems, finishedExists, clearFinished, pauseAll, resumeAll, retryAll, cancelAll, tone, displayName, isDone, telegramWaitSeconds, isPauseable, pause, chooseFile, resume, retry, cancelItem, conflictVisible, conflictTotal, conflictNames, onConflictClose, resolveConflict, enqueue, open, resolveUploadConflicts, waitForCapacity, addDeferredFiles, releaseDeferredFiles, clearDeferredFiles, onUploaded, onCancelled } = useTransfers()
</script>

<style scoped>
.head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
}
.head b {
  font-size: 15px;
}
:deep(.el-drawer__header) {
  margin-bottom: 14px;
}
.spacer {
  flex: 1;
}
.queue-summary {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 10px;
  margin-bottom: 12px;
}
.summary-item {
  display: flex;
  min-width: 0;
  flex-direction: column;
  align-items: center;
  padding: 12px 4px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.summary-value {
  font-size: 22px;
  line-height: 1.15;
}
.summary-label {
  margin-top: 5px;
  color: var(--text-soft);
  font-size: 12px;
  white-space: nowrap;
}
.summary-success .summary-value {
  color: var(--el-color-success);
}
.summary-skipped .summary-value {
  color: var(--el-color-info);
}
.summary-error .summary-value {
  color: var(--el-color-danger);
}
.queue-rate {
  display: flex;
  align-items: baseline;
  gap: 6px;
  margin: 0 0 12px;
  padding: 9px 12px;
  border: 1px solid var(--glass-border);
  border-radius: 10px;
  background: var(--tile-bg);
  color: var(--text-soft);
  font-size: 12px;
}
.queue-rate strong {
  color: var(--el-color-success);
  font-size: 18px;
  line-height: 1;
  font-variant-numeric: tabular-nums;
}
.queue-breakdown {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 14px;
}
.queue-actions {
  margin-bottom: 18px;
  padding: 12px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.queue-status,
.queue-buttons {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.queue-status + .queue-buttons {
  margin-top: 10px;
}
.queue-buttons .el-button {
  margin-left: 0;
}
.queue-hint {
  color: var(--text-soft);
  font-size: 12px;
}
.empty {
  color: var(--text-soft);
  text-align: center;
  margin-top: 32px;
}
.window-hint {
  margin-bottom: 10px;
  padding: 8px 10px;
  border-radius: 8px;
  background: var(--el-color-info-light-9);
  color: var(--text-soft);
  font-size: 12px;
  line-height: 1.5;
}

/* ---- 任务卡片 ---- */
.upload-item {
  position: relative;
  margin-bottom: 12px;
  padding: 12px 14px 12px 16px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
  overflow: hidden;
  transition: background 0.15s, border-color 0.15s;
}
.upload-item:hover {
  background: var(--tile-bg-hover);
}
/* 左侧状态色条 */
.upload-item::before {
  content: '';
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  width: 3px;
  background: var(--el-color-info);
}
.upload-item.tone-ok::before {
  background: var(--el-color-success);
}
.upload-item.tone-err::before {
  background: var(--el-color-danger);
}
.upload-item.tone-run::before {
  background: var(--el-color-primary);
}
.item-top {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.item-name {
  flex: 1;
  min-width: 0;
  font-size: 14px;
  font-weight: 600;
  line-height: 1.5;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.item-pct {
  flex-shrink: 0;
  font-size: 13px;
  font-variant-numeric: tabular-nums;
  color: var(--text-soft);
}
.badge {
  flex-shrink: 0;
}
.item-bar {
  margin: 10px 0 8px;
}
.item-foot {
  display: flex;
  align-items: center;
  gap: 10px;
  min-height: 26px;
}
.state {
  color: var(--text-soft);
  font-size: 12px;
}
.size {
  flex-shrink: 0;
  color: var(--text-soft);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}
.item-btns {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}
.item-btns .el-button {
  margin-left: 0;
}
.upload-err {
  margin-top: 8px;
  padding: 6px 8px;
  border-radius: 8px;
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);
  font-size: 12px;
  line-height: 1.5;
  word-break: break-all;
}
.conflict-hint {
  margin: 0 0 8px;
  color: var(--text-soft);
  font-size: 13px;
}
.conflict-list {
  max-height: 200px;
  overflow: auto;
  margin: 0;
  padding-left: 20px;
  font-size: 13px;
}
.conflict-list li {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
