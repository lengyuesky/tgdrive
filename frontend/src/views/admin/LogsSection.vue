<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      这里记录关键后台事件（例如消息清理、失败重试、维护与备份）。
      日志保留最近 2000 条；完整 HTTP 诊断仍请查看容器日志。
    </div>
  </div>

  <div class="log-toolbar">
    <div class="row">
      <el-button :loading="backendLogsBusy" :icon="Refresh" @click="loadBackendLogs()">
        刷新
      </el-button>
      <el-switch
        v-model="backendLogsAutoRefresh"
        inline-prompt
        active-text="自动"
        inactive-text="手动"
      />
      <span class="muted">{{ backendLogsAutoRefresh ? '每 5 秒自动刷新' : '已暂停自动刷新' }}</span>
    </div>
  </div>

  <div class="log-filters">
    <el-select
      v-model="backendLogLevel"
      placeholder="全部级别"
      clearable
      class="log-filter-sel"
    >
      <el-option
        v-for="lv in LOG_LEVELS"
        :key="lv.value"
        :label="`${lv.label} · ${backendLogFacets.level_counts[lv.value] ?? 0}`"
        :value="lv.value"
        :disabled="!backendLogFacets.level_counts[lv.value]"
      />
    </el-select>
    <el-select
      v-model="backendLogCategory"
      placeholder="全部类别"
      clearable
      class="log-filter-sel"
    >
      <el-option
        v-for="c in backendLogFacets.categories"
        :key="c"
        :label="c"
        :value="c"
      />
    </el-select>
    <el-input
      v-model="backendLogKeyword"
      placeholder="搜索类别或事件内容"
      clearable
      :prefix-icon="Search"
      class="log-filter-kw"
    />
    <el-button v-if="backendLogFiltered" text @click="resetBackendLogFilters">
      清除筛选
    </el-button>
    <span class="muted log-count">
      已加载 {{ backendLogs.length }} 条{{ backendLogsHasMore ? '(还有更早)' : '' }}
    </span>
  </div>

  <div v-loading="backendLogsBusy && !backendLogs.length" class="log-list">
    <div
      v-for="log in backendLogs"
      :key="log.id"
      class="log-item"
      :class="log.level"
    >
      <div class="log-meta">
        <el-tag :type="logLevelType(log.level)" size="small" round>
          {{ logLevelLabel(log.level) }}
        </el-tag>
        <span class="log-cat">{{ log.category }}</span>
        <span class="log-time">{{ fmtTime(log.created_at) }}</span>
      </div>
      <!-- 事件原文整段换行显示:这里的消息常有近百字,截断成一行等于看不到重点 -->
      <div class="log-msg">{{ log.message }}</div>
    </div>
    <el-empty
      v-if="!backendLogs.length && !backendLogsBusy"
      :description="backendLogFiltered ? '没有符合筛选条件的日志' : '暂无后台日志'"
      :image-size="64"
    />
  </div>

  <div v-if="backendLogsHasMore" class="log-more">
    <el-button :loading="backendLogsBusy" plain @click="loadMoreBackendLogs">
      加载更早的日志
    </el-button>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled, Refresh, Search } from '@element-plus/icons-vue'
import { fmtTime } from '../../api'
import { LOG_LEVELS, logLevelLabel, logLevelType } from './useBackendLogs'
import { useAdminContext } from './useAdmin'

const {
  backendLogCategory,
  backendLogFacets,
  backendLogFiltered,
  backendLogKeyword,
  backendLogLevel,
  backendLogs,
  backendLogsAutoRefresh,
  backendLogsBusy,
  backendLogsHasMore,
  loadBackendLogs,
  loadMoreBackendLogs,
  loading,
  resetBackendLogFilters,
} = useAdminContext()
</script>
