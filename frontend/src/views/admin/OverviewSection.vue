<template>
  <div class="hero">
    <span class="status-dot" :class="status?.bot ? 'ok' : 'bad'"><i /></span>
    <div class="hero-text">
      <div class="hero-title">
        {{
          status?.bot
            ? `@${status.bot}`
            : status?.configured
              ? '连接不上 Telegram'
              : '尚未配置 Telegram'
        }}
      </div>
      <div class="hero-sub">
        {{ status?.official_api ? '官方 Bot API' : '自建 Bot API' }} · 已运行
        {{ uptime }} · 监听 {{ status?.listen }}
      </div>
    </div>
    <el-button :loading="testing" @click="test">测试连接</el-button>
  </div>

  <div class="tiles">
    <div
      v-for="t in statTiles"
      :key="t.label"
      class="tile"
      :class="[t.tone, { clickable: !!t.action }]"
      @click="t.action?.()"
    >
      <span class="tile-ico"><el-icon><component :is="t.icon" /></el-icon></span>
      <div class="tile-body">
        <div class="tile-val">{{ t.value }}</div>
        <div class="tile-label">{{ t.label }}</div>
      </div>
    </div>
  </div>

  <div class="block">
    <div class="block-head">后台任务</div>
    <div class="tag-row">
      <el-tooltip
        v-for="task in status?.tasks ?? []"
        :key="task.name"
        :content="task.last_error ?? `重启 ${task.restarts} 次`"
      >
        <el-tag size="small" round :type="taskHealthy(task) ? 'success' : 'danger'">
          {{ task.name }}
        </el-tag>
      </el-tooltip>
      <span v-if="!status?.tasks?.length" class="muted">暂无任务</span>
    </div>
  </div>

  <div class="block">
    <div class="block-head">快捷操作</div>
    <div class="quick">
      <button
        v-for="q in quickActions"
        :key="q.desc"
        class="quick-item"
        :class="{ danger: q.danger }"
        @click="q.action()"
      >
        <el-icon><component :is="q.icon" /></el-icon>
        <span class="quick-text">
          <b>{{ q.label }}</b>
          <em>{{ q.desc }}</em>
        </span>
      </button>
    </div>
  </div>

  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      元数据库是文件的唯一索引 —— Bot API 读不了频道历史,库丢了文件就找不回来,
      记得在「备份与回收站」里至少启用一个远端渠道。数据库
      {{ status?.db_path }}。
    </div>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled } from '@element-plus/icons-vue'
import { useAdminContext } from './useAdmin'

const {
  loading,
  quickActions,
  statTiles,
  status,
  taskHealthy,
  test,
  testing,
  uptime,
} = useAdminContext()
</script>
