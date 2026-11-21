<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import ResponsivePanel from '../../components/ResponsivePanel.vue'
import { api, errMsg, fmtSize, type RestoreBackup, type RestorePrepare } from '../../api'
import { restorePhaseLabel, type RestoreController } from './useRestore'

const props = defineProps<{ controller: RestoreController }>()
const r = props.controller
const { status, busy, loaded, error, waiting, loginRequired } = r
const source = ref<'channel' | 'history' | 'manual'>('channel')
const backups = ref<RestoreBackup[]>([])
const discovered = ref<RestoreBackup[]>([])
const discovering = ref(false)
const discoveryError = ref('')
const scanned = ref(0)
const hasMore = ref(false)
const discoveryStarted = ref(false)
const discoveryComplete = computed(() => discovered.value.filter(b => b.available).length >= 5)
const stopDiscovery = ref(false)
const historyLoading = ref(false)
const historyError = ref('')
const backupId = ref('')
const messages = ref('')
const channel = ref('')
const decryptPassword = ref('')
const acknowledge = ref(false)
const password = ref('')
const totp = ref('')
const confirmation = ref('')
const resumeAcknowledged = ref(false)
let disposed = false
const editable = computed(() => loaded.value && !r.active.value && !waiting.value && !busy.value && !status.value.safe_mode)
const choices = computed(() => source.value === 'channel' ? discovered.value : backups.value)
const sourceValid = computed(() => source.value !== 'manual'
  ? choices.value.some(b => b.id === backupId.value && b.available)
  : !!messages.value.trim() && (!!channel.value.trim() || messages.value.trim().split(/[\s,;]+/).every(item => /^https?:\/\/t\.me\//i.test(item))))
const canPrepare = computed(() => editable.value && !discovering.value && sourceValid.value && acknowledge.value)
const canConfirm = computed(() => status.value.phase === 'ready' && !!status.value.id && confirmation.value === '恢复' && !!password.value && !busy.value && !waiting.value)
function clearSecrets() { decryptPassword.value = ''; password.value = ''; totp.value = ''; confirmation.value = '' }
watch(source, () => { decryptPassword.value = ''; acknowledge.value = false; backupId.value = ''; stopDiscovery.value = true })
watch(() => status.value.phase, () => { password.value = ''; totp.value = ''; confirmation.value = ''; resumeAcknowledged.value = false })
async function loadHistory() {
  historyLoading.value = true
  historyError.value = ''
  try { const { data } = await api.admin.restore.history(); if (!disposed) backups.value = data.backups }
  catch (e) { if (!disposed) historyError.value = errMsg(e) }
  finally { if (!disposed) historyLoading.value = false }
}
async function discover(reset = false) {
  if (discovering.value || !editable.value) return
  discovering.value = true
  stopDiscovery.value = false
  discoveryError.value = ''
  if (reset) { discovered.value = []; scanned.value = 0; backupId.value = ''; hasMore.value = false }
  try {
    do {
      const { data } = await api.admin.restore.discover(reset)
      if (disposed) return
      reset = false
      discoveryStarted.value = true
      discovered.value = data.backups
      scanned.value = data.scanned
      hasMore.value = data.has_more
      // 服务端达到 5 份完整备份后停止；不足 5 份则继续到频道起点。
    } while (hasMore.value && !discoveryComplete.value && !stopDiscovery.value)
  } catch (e) { if (!disposed) discoveryError.value = errMsg(e) }
  finally { if (!disposed) discovering.value = false }
}
function prepare() {
  if (!canPrepare.value) return
  const body: RestorePrepare = { acknowledge_forward: true }
  if (source.value !== 'manual') body.backup_id = backupId.value
  else { body.messages = messages.value.trim(); if (channel.value.trim()) body.channel = channel.value.trim() }
  if (decryptPassword.value) body.password = decryptPassword.value
  clearSecrets()
  void r.prepare(body)
}
function confirm() {
  if (!canConfirm.value) return
  const body = { id: status.value.id!, password: password.value, totp: totp.value.trim() || undefined, confirmation: '恢复' as const }
  clearSecrets()
  void r.confirm(body)
}
function resume() {
  if (!resumeAcknowledged.value || !password.value || busy.value || waiting.value) return
  const body = { password: password.value, totp: totp.value.trim() || undefined }
  clearSecrets(); resumeAcknowledged.value = false
  void r.resume(body)
}
function close() { stopDiscovery.value = true; clearSecrets(); r.opened.value = false }
onMounted(() => void loadHistory())
onUnmounted(() => { disposed = true; stopDiscovery.value = true; clearSecrets() })
</script>

<template>
  <ResponsivePanel :model-value="true" title="从 Telegram 恢复" width="680px" align-center @update:model-value="close">
    <div class="restore-content">
      <p>恢复文件索引与配置，不会重新下载所有已存储文件。仅支持有效的网页登录 Cookie，WebDAV Basic / 应用密码不可用。</p>
      <el-alert :title="restorePhaseLabel[status.phase]" :type="status.phase === 'failed' || status.phase === 'rolled_back' ? 'error' : 'info'" :closable="false" />
      <p v-if="status.message" role="status">{{ status.message }}</p>
      <el-alert v-if="error" :title="error" type="error" :closable="false" />
      <ul v-if="status.warnings.length" aria-label="恢复警告"><li v-for="(warning, i) in status.warnings" :key="i">{{ warning }}</li></ul>
      <div v-if="waiting" class="note warn" role="status">
        <div>已进入重启等待，请勿重复提交。服务启动后请重新登录，再到「备份」查看持久结果。当前管理员安全设置保留，会话将失效；非容器部署需要进程管理器或手动启动服务。</div>
      </div>
      <template v-if="!waiting">
        <div v-if="status.summary" class="restore-summary">
          <p>备份：{{ status.summary.name }}</p>
          <p>大小：{{ fmtSize(status.summary.bytes) }}；文件：{{ status.summary.files }}；目录：{{ status.summary.directories }}；{{ status.summary.encrypted ? '加密备份' : '未加密备份' }}</p>
          <p>预检不等于验证来源真实性，请仅恢复可信备份。确认后会覆盖当前数据库，并保留本机回滚副本。</p>
        </div>
        <p v-if="status.rollback_path">本机回滚副本：<code>{{ status.rollback_path }}</code>（私有文件，不会自动删除）</p>
        <template v-if="editable">
          <el-radio-group v-model="source" aria-label="备份来源">
            <el-radio-button value="channel">探测频道备份</el-radio-button>
            <el-radio-button value="history">本地记录的备份</el-radio-button>
            <el-radio-button value="manual">粘贴消息链接 / ID</el-radio-button>
          </el-radio-group>
          <el-form label-position="top" @submit.prevent="prepare">
            <template v-if="source !== 'manual'">
              <template v-if="source === 'channel'">
                <p>全新部署只需在 .env 填写 Bot Token、频道 ID、TELEGRAM_API_ID 和 TELEGRAM_API_HASH，重启后即可探测旧备份。使用当前保存的频道配置，无需旧数据库或手机登录。</p>
                <p>点击「探测频道备份」后开始查找最近 5 份完整备份，找到后停止；不足 5 份时显示已找到的备份。探测只读取消息信息。</p>
                <el-alert v-if="discoveryError" :title="discoveryError" type="error" :closable="false" />
                <p v-if="discovering || discoveryStarted" role="status">已检查 {{ scanned }} 个消息位置，发现 {{ discovered.length }} 组备份。{{ discovering ? '正在探测…' : discoveryComplete ? '已找到最近 5 份完整备份，探测已停止。' : hasMore ? '探测已暂停，可继续查找。' : '已探测到频道起点。' }}</p>
                <div>
                  <el-button :disabled="discovering" @click="discover(true)">{{ discoveryStarted || discoveryError ? '重新探测频道' : '探测频道备份' }}</el-button>
                  <el-button v-if="!discoveryComplete && (hasMore || discoveryError)" :disabled="discovering" @click="discover(false)">继续探测</el-button>
                  <el-button v-if="discovering" @click="stopDiscovery = true">停止继续探测</el-button>
                </div>
                <p v-if="discoveryStarted && !discovering && !discoveryError && !hasMore && !discovered.length">频道中未发现 tgdrive 备份文档，请检查频道是否正确、Bot 是否有访问权限。</p>
              </template>
              <el-alert v-else-if="historyError" :title="historyError" type="error" :closable="false" />
              <el-form-item label="选择频道备份">
                <el-select v-model="backupId" :loading="source === 'channel' ? discovering : historyLoading" placeholder="请选择一份完整备份" style="width: 100%">
                  <el-option v-for="b in choices" :key="b.id" :value="b.id" :label="`${b.name}（${b.parts} 块）${b.available ? '' : ' · 不可用'}`" :disabled="!b.available" />
                </el-select>
              </el-form-item>
              <p v-if="source === 'history' && !historyLoading && !historyError && !backups.length">没有本地记录？请切换到「探测频道备份」，也可粘贴消息链接 / ID。</p>
              <p v-for="b in choices.filter(item => !item.available)" :key="b.id">{{ b.name }}：{{ b.error || '记录损坏或存储目标不可用' }}</p>
              <el-button v-if="source === 'history'" text :disabled="historyLoading" @click="loadHistory">重新加载记录</el-button>
            </template>
            <template v-else>
              <p>适用于全新部署或本地记录丢失。粘贴同一份备份完整一组消息链接（支持公开频道和 t.me/c/ 私有频道），或填写频道 ID 和多个消息 ID。每行一条，消息 ID 不必连续；必须包含全部分块，不要混入其他备份。</p>
              <p>手动来源无需配置历史探测所需的 API ID / Hash。使用当前已保存的 Bot、API 端点和代理，由后端下载全部备份数据。</p>
              <el-form-item label="频道 ID（使用消息 ID 时必填；完整链接可留空）"><el-input v-model="channel" placeholder="例如 -1001234567890" /></el-form-item>
              <el-form-item label="完整一组消息链接，或多个消息 ID"><el-input v-model="messages" type="textarea" :rows="4" placeholder="https://t.me/c/1234567890/42&#10;https://t.me/c/1234567890/57" /></el-form-item>
            </template>
            <el-form-item label="备份解密密码（可选，仅用于本次预检）"><el-input v-model="decryptPassword" type="password" autocomplete="off" placeholder="加密备份需填写，不是当前登录密码" /></el-form-item>
            <p>准备时将原备份临时转发到同一备份频道，Bot 需要读取和发送权限，频道不能禁止转发。只删除本任务创建且已知的临时消息，绝不删除原备份；清理失败会单独警告。</p>
            <el-checkbox v-model="acknowledge">我同意临时转发到同一频道，并仅清理本任务的临时消息</el-checkbox>
            <div><el-button type="primary" :disabled="!canPrepare" @click="prepare">下载并预检</el-button></div>
          </el-form>
        </template>
        <p v-if="status.phase === 'preparing'">后端正在下载、合并、解密和校验；此阶段不会替换当前数据库，可以取消。</p>
        <template v-if="status.safe_mode">
          <el-alert title="恢复安全模式：旧上传清理、删除队列、迁移、自动备份清理及收件暂停。不会自动恢复这些任务。" type="warning" :closable="false" />
          <p>解除后旧任务可能继续删除真实频道文件或执行迁移。请先核对备份中的存储目标、索引及旧任务；不能确认时请保持安全模式。解除需要再次认证并重启，不会再次恢复数据库。</p>
          <el-checkbox v-if="!r.active.value" v-model="resumeAcknowledged">我已检查旧任务，理解可能删除真实频道文件的风险，确认解除安全模式并重启</el-checkbox>
        </template>
        <el-form v-if="status.phase === 'ready' || (status.safe_mode && !r.active.value)" label-position="top" @submit.prevent>
          <el-form-item v-if="status.phase === 'ready'" label="输入“恢复”以确认覆盖当前数据库"><el-input v-model="confirmation" autocomplete="off" /></el-form-item>
          <el-form-item label="当前登录密码"><el-input v-model="password" type="password" autocomplete="off" /></el-form-item>
          <el-form-item label="两步验证码（已启用 2FA 时必填）"><el-input v-model="totp" autocomplete="off" inputmode="numeric" /></el-form-item>
          <el-button v-if="status.phase === 'ready'" type="danger" :disabled="!canConfirm" :loading="busy" @click="confirm">确认恢复并重启</el-button>
          <el-button v-else type="danger" :disabled="!resumeAcknowledged || !password || busy" :loading="busy" @click="resume">解除安全模式并重启</el-button>
        </el-form>
      </template>
    </div>
    <template #footer>
      <el-button v-if="['preparing', 'ready'].includes(status.phase) && !waiting" :disabled="busy || !status.id" @click="clearSecrets(); r.cancel()">取消恢复任务</el-button>
      <el-button v-if="waiting || loginRequired" type="primary" @click="close(); $router.push('/login')">重新登录</el-button>
      <el-button :disabled="busy" @click="r.refresh">刷新状态</el-button>
      <el-button @click="close">关闭</el-button>
    </template>
  </ResponsivePanel>
</template>

<style scoped>
.restore-content { display: grid; gap: 14px; overflow-wrap: anywhere; }
.restore-content p { margin: 0; line-height: 1.7; }
.restore-content .el-form { display: grid; gap: 12px; }
.restore-content .el-form-item { margin-bottom: 0; }
.restore-content .el-checkbox { white-space: normal; height: auto; align-items: flex-start; }
.restore-content :deep(.el-checkbox__label) { white-space: normal; line-height: 1.6; }
.restore-summary { padding: 12px; border: 1px solid var(--el-border-color); border-radius: 8px; }
</style>
