<script setup lang="ts">
import ResponsivePanel from '../components/ResponsivePanel.vue'
import { ref } from 'vue'
import { ArrowLeft, Download, InfoFilled, Moon, Refresh, Setting, Sunny, WarnTriangleFilled } from '@element-plus/icons-vue'
import { isDark, toggleDark } from '../theme'
import { api, fmtTime } from '../api'
import DirPicker from '../components/DirPicker.vue'
import { useAdmin } from './admin/useAdmin'
import { SECTIONS } from './admin/sections'
import type { DetectedChat } from '../api'

// 全部状态与动作在 useAdmin 中创建并 provide，分区子组件注入同一份。
const {
  activeSection,
  restore,
  restoreLocked,
  addInboxRule,
  addWhitelist,
  backingUp,
  backupNow,
  badges,
  cfg,
  changed,
  clearInboxFailures,
  copyEnv,
  detect,
  detectBot,
  detectDialog,
  detectedChats,
  detecting,
  dirtySections,
  discardAll,
  envDialog,
  envText,
  exportEnv,
  extras,
  extrasSaving,
  go,
  inboxDirPicker,
  inboxFailureDialog,
  inboxFailureLoading,
  inboxFailures,
  loading,
  reloadAll,
  removeInboxFailure,
  router,
  saveAll,
  saving,
  section,
  status,
  test,
  testing,
  useChatId,
} = useAdmin()

const KIND_LABEL: Record<string, string> = {
  channel: '频道',
  private: '用户',
  user: '用户',
  group: '群组',
  supergroup: '群组',
}
const isUserKind = (k: string) => k === 'private' || k === 'user'
import './admin/admin.css'
import OverviewSection from './admin/OverviewSection.vue'
import QueuesSection from './admin/QueuesSection.vue'
import LogsSection from './admin/LogsSection.vue'
import TelegramSection from './admin/TelegramSection.vue'
import ProxySection from './admin/ProxySection.vue'
import StreamSection from './admin/StreamSection.vue'
import AccountSection from './admin/AccountSection.vue'
import RestoreDialog from './admin/RestoreDialog.vue'
import { restorePhaseLabel } from './admin/useRestore'
import BackupSection from './admin/BackupSection.vue'
import InboxSection from './admin/InboxSection.vue'
import AdvancedSection from './admin/AdvancedSection.vue'
</script>

<template>
  <div class="page admin-page" v-loading="loading">
    <div class="topbar glass">
      <el-tooltip content="返回文件">
        <el-button class="icon-btn" aria-label="返回文件" :icon="ArrowLeft" @click="router.push('/files/')" />
      </el-tooltip>
      <div class="logo-badge"><el-icon><Setting /></el-icon></div>
      <div class="brand brand-text">系统设置</div>
      <el-tag v-if="status" size="small" round type="info" class="ver">v{{ status.version }}</el-tag>
      <div class="spacer" />
      <el-tooltip content="导出 .env">
        <el-button class="icon-btn" aria-label="导出环境配置" :icon="Download" @click="exportEnv" />
      </el-tooltip>
      <el-tooltip content="刷新">
        <el-button class="icon-btn" aria-label="刷新" :icon="Refresh" @click="reloadAll" />
      </el-tooltip>
      <el-tooltip :content="isDark ? '切换亮色' : '切换暗色'">
        <el-button class="icon-btn" :aria-label="isDark ? '切换亮色' : '切换暗色'" :icon="isDark ? Sunny : Moon" @click="toggleDark" />
      </el-tooltip>
    </div>

    <div v-if="cfg && !cfg.configured" class="note warn setup-note">
      <el-icon><WarnTriangleFilled /></el-icon>
      <div>
        <b>还没配置 Telegram 存储</b>,网盘暂时不能上传下载。Token 找 @BotFather
        申请;存储频道建议新建一个私有频道,把 bot 拉进去设为管理员。
      </div>
      <el-button type="primary" size="small" @click="go('telegram')">去配置</el-button>
    </div>

    <div v-if="restore.status.value.phase !== 'idle' || restore.status.value.safe_mode || restore.error.value" class="note warn" role="status">
      <div>
        <b>{{ restorePhaseLabel[restore.status.value.phase] }}</b>
        <p v-if="restore.waiting.value">等待服务重启后重新登录，不会自动重复恢复操作。</p>
        <p v-if="restore.status.value.safe_mode">恢复安全模式已启用：旧清理、删除、迁移、备份清理及收件任务暂停。请检查旧任务风险，不会自动解除。</p>
        <p v-if="restore.error.value">{{ restore.error.value }}</p>
      </div>
      <el-button @click="restore.open">查看恢复状态</el-button>
    </div>
    <RestoreDialog v-if="restore.opened.value" :controller="restore" />

    <div class="layout" :inert="restoreLocked || undefined">
      <nav class="nav glass">
        <button
          v-for="s in SECTIONS"
          :key="s.key"
          class="nav-item"
          :class="{ on: section === s.key }"
          @click="go(s.key)"
        >
          <span class="nav-ico"><el-icon><component :is="s.icon" /></el-icon></span>
          <span class="nav-text">
            <span class="nav-label">{{ s.label }}</span>
            <span class="nav-desc">{{ s.desc }}</span>
          </span>
          <span class="nav-tail">
            <span v-if="dirtySections.has(s.key)" class="nav-dot" title="有未保存的改动" />
            <span v-if="badges[s.key]" class="nav-badge" :class="badges[s.key]!.tone">
              {{ badges[s.key]!.text }}
            </span>
          </span>
        </button>
      </nav>

      <div class="pane glass">
        <div class="pane-head">
          <div class="pane-ico">
            <el-icon><component :is="activeSection.icon" /></el-icon>
          </div>
          <div class="pane-title">
            <h2>{{ activeSection.label }}</h2>
            <div class="pane-desc">{{ activeSection.desc }}</div>
          </div>
          <div class="pane-actions">
            <el-button v-if="section === 'telegram' || section === 'proxy'" :loading="testing" @click="test">
              测试连接
            </el-button>
            <el-button v-if="section === 'backup'" :loading="backingUp" @click="backupNow">
              立即备份
            </el-button>
            <el-button v-if="section === 'inbox'" plain @click="addInboxRule">
              添加自定义类型
            </el-button>
          </div>
        </div>

        <transition name="pane" mode="out-in">
          <div :key="section" class="pane-body">
            <!-- ========== 概览 ========== -->
            <OverviewSection v-if="section === 'overview'" />

            <!-- ========== 后台任务 ========== -->
            <QueuesSection v-else-if="section === 'queues'" />

            <!-- ========== 后台日志 ========== -->
            <LogsSection v-else-if="section === 'logs'" />

            <!-- ========== Telegram 存储 ========== -->
            <TelegramSection v-else-if="section === 'telegram'" />

            <!-- ========== 网络代理 ========== -->
            <ProxySection v-else-if="section === 'proxy'" />

            <!-- ========== 流式播放 ========== -->
            <StreamSection v-else-if="section === 'stream'" />

            <!-- ========== 账号 ========== -->
            <AccountSection v-else-if="section === 'account'" />

            <!-- ========== 备份与回收站 ========== -->
            <BackupSection v-else-if="section === 'backup'" />

            <!-- ========== Bot 收件 ========== -->
            <InboxSection v-else-if="section === 'inbox'" />

            <!-- ========== 高级 ========== -->
            <AdvancedSection v-else />
          </div>
        </transition>
      </div>
    </div>

    <transition name="savebar">
      <div v-if="changed.total" class="savebar glass">
        <span class="savebar-dot" />
        <span class="savebar-text">{{ changed.total }} 项改动待保存</span>
        <el-button text @click="discardAll">放弃</el-button>
        <el-button type="primary" :disabled="restoreLocked" :loading="saving || extrasSaving" @click="saveAll">
          保存并生效
        </el-button>
      </div>
    </transition>

    <ResponsivePanel class="admin-panel" v-model="inboxFailureDialog" title="Bot 收件死信" width="760px" align-center>
      <el-table :data="inboxFailures" v-loading="inboxFailureLoading" size="small">
        <el-table-column prop="update_id" label="Update ID" width="120" />
        <el-table-column prop="attempts" label="尝试" width="72" />
        <el-table-column label="失败时间" width="170">
          <template #default="{ row }">{{ fmtTime(row.failed_at) }}</template>
        </el-table-column>
        <el-table-column prop="last_error" label="最后错误" show-overflow-tooltip />
        <el-table-column label="操作" width="72" align="right">
          <template #default="{ row }">
            <el-button link type="danger" @click="removeInboxFailure(row.update_id)">删除</el-button>
          </template>
        </el-table-column>
        <template #empty><el-empty description="没有死信" :image-size="56" /></template>
      </el-table>
      <template #footer>
        <el-button :disabled="!inboxFailures.length" type="danger" plain @click="clearInboxFailures">
          清空全部
        </el-button>
        <el-button type="primary" @click="inboxFailureDialog = false">关闭</el-button>
      </template>
    </ResponsivePanel>

    <DirPicker ref="inboxDirPicker" />

    <ResponsivePanel class="admin-panel" v-model="detectDialog" title="检测频道 / 用户 ID" width="680px" align-center>
      <div class="note">
        <el-icon><InfoFilled /></el-icon>
        <div>
          <div>
            找<b>频道 ID</b>:把 bot{{ detectBot ? `(@${detectBot})` : '' }}
            拉进目标频道并设为<b>管理员</b>,在频道里随便发一条消息,再点「重新检测」。
          </div>
          <div>找<b>用户 ID</b>(收件白名单用):用该账号<b>私聊 bot</b> 随便发一条消息,再点「重新检测」。</div>
        </div>
      </div>
      <el-table
        :data="detectedChats"
        size="small"
        v-loading="detecting"
        empty-text="还没检测到会话,按上面的提示给 bot 发条消息后点「重新检测」"
      >
        <el-table-column label="类型" width="76">
          <template #default="{ row }">
            <el-tag size="small" round :type="row.kind === 'channel' ? 'primary' : 'info'">
              {{ KIND_LABEL[row.kind] ?? row.kind }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="名称" show-overflow-tooltip>
          <template #default="{ row }">
            {{ row.name }}<span v-if="row.username" class="muted"> @{{ row.username }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="id" label="ID" width="150" />
        <el-table-column label="最近出现" width="90">
          <template #default="{ row }">{{ fmtTime(row.at).slice(5, 16) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="130">
          <template #default="{ row }">
            <el-button
              v-if="!isUserKind(row.kind)"
              link
              type="primary"
              @click="useChatId(row as DetectedChat)"
            >
              填入存储频道
            </el-button>
            <el-button
              v-else-if="extras"
              link
              type="primary"
              @click="addWhitelist(row as DetectedChat)"
            >
              加入白名单
            </el-button>
          </template>
        </el-table-column>
      </el-table>
      <div class="muted detect-foot">
        列表来自 bot 最近收到的消息(开启收件后也会持续记录),不会保存任何配置改动。
      </div>
      <template #footer>
        <el-button @click="detectDialog = false">关闭</el-button>
        <el-button type="primary" :loading="detecting" @click="detect">重新检测</el-button>
      </template>
    </ResponsivePanel>

    <ResponsivePanel class="admin-panel" v-model="envDialog" title="当前生效配置(.env 格式)" width="640px" align-center>
      <div class="note warn">
        <el-icon><WarnTriangleFilled /></el-icon>
        <div>包含 Bot Token 与密码,注意不要外传。</div>
      </div>
      <el-input v-model="envText" type="textarea" :rows="16" readonly />
      <template #footer>
        <el-button @click="envDialog = false">关闭</el-button>
        <el-button type="primary" @click="copyEnv">复制</el-button>
      </template>
    </ResponsivePanel>
  </div>
</template>
