<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      消息清理队列负责删除已不再被网盘引用的 Telegram 消息。
      默认<b>手动</b>:任务照常记账排队,但要你在这里点「立即执行」才会真的发请求。
      Telegram 不允许 Bot 删除发出超过 48 小时的消息,这类任务会自动跳过;
      想让上传残片及时清掉,建议切到<b>自动</b>。
    </div>
  </div>

  <div class="queue-cards">
    <div v-for="q in QUEUES" :key="q.key" class="queue-card">
      <div class="queue-head">
        <span class="queue-ico"><el-icon><component :is="q.icon" /></el-icon></span>
        <div class="queue-title">
          <b>{{ q.label }}</b>
          <em>{{ q.desc }}</em>
        </div>
        <el-switch
          :model-value="queueAuto(q.key)"
          inline-prompt
          active-text="自动"
          inactive-text="手动"
          @update:model-value="setQueueAuto(q.key, $event as boolean)"
        />
      </div>

      <div class="queue-stats">
        <div class="queue-stat">
          <b :class="{ warn: (queueOverview(q.key)?.pending ?? 0) > 0 }">
            {{ queueOverview(q.key)?.pending ?? 0 }}
          </b>
          <em>待处理</em>
        </div>
        <div class="queue-stat">
          <b :class="{ warn: (queueOverview(q.key)?.retrying ?? 0) > 0 }">
            {{ queueOverview(q.key)?.retrying ?? 0 }}
          </b>
          <em>失败重试过</em>
        </div>
        <div class="queue-stat">
          <b>{{ queueOverview(q.key)?.max_attempts ?? 0 }}</b>
          <em>最多试过几次</em>
        </div>
      </div>

      <!-- 卡住的根因直接摊开,不用再去日志里翻 -->
      <div v-if="queueOverview(q.key)?.last_error" class="queue-error">
        <el-icon><WarnTriangleFilled /></el-icon>
        <span>{{ queueOverview(q.key)?.last_error }}</span>
      </div>

      <div v-if="queueLastRun[q.key]" class="queue-result">
        本次执行:成功 {{ queueLastRun[q.key]!.completed }} 条,超 48 小时跳过
        {{ queueLastRun[q.key]!.discarded }} 条,失败 {{ queueLastRun[q.key]!.failed }} 条,剩余
        {{ queueLastRun[q.key]!.pending }} 条<template
          v-if="queueLastRun[q.key]!.stopped_early"
          >;本轮超时收工,可再点一次继续</template
        >
      </div>

      <div class="queue-actions">
        <el-button
          type="primary"
          plain
          :icon="Refresh"
          :loading="queueBusy === q.key"
          :disabled="!queueOverview(q.key)?.pending"
          @click="runQueue(q.key)"
        >
          立即执行
        </el-button>
        <el-button
          type="danger"
          text
          :disabled="!queueOverview(q.key)?.pending || queueBusy === q.key"
          @click="clearQueue(q.key)"
        >
          清空队列
        </el-button>
        <span v-if="queueAutoDirty(q.key)" class="queue-dirty">
          开关改动尚未保存
        </span>
        <span v-else class="muted queue-hint">{{ q.cost }}</span>
      </div>
    </div>
  </div>

  <div v-if="storageMigration" class="queue-card migration-card">
    <div class="queue-head">
      <span class="queue-ico"><el-icon><Promotion /></el-icon></span>
      <div class="queue-title">
        <b>历史存储迁移</b>
        <em>通过 Telegram 服务端复制，把其他 Bot 的分块切换到主 Bot。</em>
      </div>
      <el-tag
        size="small"
        round
        :type="
          storageMigration.status === 'failed'
            ? 'danger'
            : storageMigration.running
              ? 'warning'
              : storageMigration.remaining_messages === 0
                ? 'success'
                : 'info'
        "
      >
        {{
          storageMigration.running
            ? '迁移中'
            : storageMigration.status === 'failed'
              ? '失败'
              : storageMigration.status === 'paused'
                ? '已暂停'
                : storageMigration.remaining_messages === 0
                  ? '已完成'
                  : '未开始'
        }}
      </el-tag>
    </div>

    <div class="queue-stats">
      <div class="queue-stat">
        <b>{{ storageMigration.main_bot_id }}</b>
        <em>主 Bot</em>
      </div>
      <div class="queue-stat">
        <b>{{ storageMigration.migrated_messages }}</b>
        <em>已迁移消息</em>
      </div>
      <div class="queue-stat">
        <b :class="{ warn: storageMigration.remaining_messages > 0 }">
          {{ storageMigration.remaining_messages }}
        </b>
        <em>剩余消息</em>
      </div>
      <div class="queue-stat">
        <b>{{ storageMigration.remaining_chunks }}</b>
        <em>剩余分块引用</em>
      </div>
    </div>

    <el-progress
      :percentage="storageMigrationPct"
      :stroke-width="8"
      :status="storageMigration.status === 'failed' ? 'exception' : undefined"
    />

    <div v-if="storageMigration.sources.length" class="migration-sources">
      <div v-for="source in storageMigration.sources" :key="source.storage_target_id">
        <span>Bot {{ source.bot_id }}</span>
        <span>{{ source.messages }} 条消息 / {{ source.chunks }} 个引用</span>
        <span>{{ fmtSize(source.bytes) }}</span>
      </div>
    </div>

    <div v-if="storageMigration.last_error" class="queue-error">
      <el-icon><WarnTriangleFilled /></el-icon>
      <span>{{ storageMigration.last_error }}</span>
    </div>

    <div class="queue-actions">
      <el-button
        v-if="storageMigration.running"
        type="warning"
        plain
        :icon="VideoPause"
        :loading="storageMigrationBusy"
        @click="pauseStorageMigration"
      >
        暂停
      </el-button>
      <el-button
        v-else
        type="primary"
        plain
        :icon="VideoPlay"
        :loading="storageMigrationBusy"
        :disabled="storageMigration.remaining_messages === 0"
        @click="startStorageMigration"
      >
        {{ storageMigration.status === 'failed' ? '重试' : storageMigration.status === 'paused' ? '继续' : '开始迁移' }}
      </el-button>
      <span class="muted queue-hint">
        旧消息会保留；每 3 秒最多复制 1 条，用户上传优先。完成后可从上传 Bot 池移除旧 Bot，避免新文件再次分流。
      </span>
    </div>
  </div>

  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      单次「立即执行」最多处理 200 条,剩余数量不为 0 时可以再点一次。
      每条任务只尝试一次,失败原因会写进<a class="link" @click="go('logs')">后台日志</a>。
    </div>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled, Promotion, Refresh, VideoPause, VideoPlay, WarnTriangleFilled } from '@element-plus/icons-vue'
import { fmtSize } from '../../api'
import { QUEUES } from './useTaskQueues'
import { useAdminContext } from './useAdmin'

const {
  clearQueue,
  go,
  loading,
  pauseStorageMigration,
  queueAuto,
  queueAutoDirty,
  queueBusy,
  queueLastRun,
  queueOverview,
  runQueue,
  setQueueAuto,
  startStorageMigration,
  status,
  storageMigration,
  storageMigrationBusy,
  storageMigrationPct,
} = useAdminContext()
</script>
