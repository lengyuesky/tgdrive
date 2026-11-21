<template>
  <div v-if="status" class="security">
    <div v-if="!status.password_hashed" class="note warn">
      <el-icon><WarnTriangleFilled /></el-icon>
      <div>
        当前登录密码来自 .env 明文。在上方重新设置一次密码并保存,即可改为哈希保存在数据库里;
        也可以用 <code>tgdrive hash-password</code> 生成哈希后写进 .env(需加单引号)。
      </div>
    </div>

    <div class="block">
      <div class="block-head">两步验证</div>

      <!-- 刚绑定或刚重新生成:恢复码只显示这一次 -->
      <div v-if="recoveryCodes.length" class="sec-card">
        <div class="note warn">
          <el-icon><WarnTriangleFilled /></el-icon>
          <div>
            请立即保存下面的恢复码,每个只能用一次;手机丢失时可用它代替验证码登录。
            离开此页面后将无法再次查看。
          </div>
        </div>
        <div class="codes">
          <code v-for="code in recoveryCodes" :key="code">{{ code }}</code>
        </div>
        <div class="row">
          <el-button :icon="CopyDocument" @click="copyText(recoveryCodes.join('\n'))">
            复制全部
          </el-button>
          <el-button type="primary" @click="recoveryCodes = []">我已保存</el-button>
        </div>
      </div>

      <div v-else-if="status.totp_enabled" class="sec-card">
        <div class="sec-state">
          <el-tag type="success" round disable-transitions>已启用</el-tag>
          <span>剩余恢复码 {{ status.recovery_remaining }} 个</span>
        </div>
        <p class="sec-desc">
          网页登录需要输入验证器上的 6 位验证码。关闭或重新生成恢复码前,请先输入当前验证码或一个恢复码。
        </p>
        <div class="row">
          <el-input
            v-model="manageCode"
            class="code-input"
            placeholder="验证码或恢复码"
            autocomplete="one-time-code"
            @keydown.enter.prevent
          />
          <el-button :loading="busy === 'recovery'" :disabled="!manageCode.trim()" @click="regenerate">
            重新生成恢复码
          </el-button>
          <el-button
            type="danger"
            plain
            :loading="busy === 'disable'"
            :disabled="!manageCode.trim()"
            @click="disable"
          >
            关闭两步验证
          </el-button>
        </div>
      </div>

      <div v-else-if="setup" class="sec-card">
        <p class="sec-desc">
          用 Google Authenticator、Microsoft Authenticator、1Password 等验证器扫描二维码,
          然后输入显示的 6 位验证码完成绑定。二维码 10 分钟内有效。
        </p>
        <div class="setup">
          <!-- 后端用自有数据渲染的 SVG,不含外部内容 -->
          <div class="qr" v-html="setup.qr_svg" />
          <div class="setup-side">
            <span class="sec-label">无法扫码时手动输入密钥</span>
            <div class="secret">
              <code>{{ groupedSecret }}</code>
              <el-button link :icon="CopyDocument" @click="copyText(setup.secret)" />
            </div>
            <el-input
              v-model="enableCode"
              class="code-input"
              placeholder="6 位验证码"
              inputmode="numeric"
              maxlength="7"
              autocomplete="one-time-code"
              @keydown.enter.prevent="enable"
            />
            <div class="row">
              <el-button
                type="primary"
                :loading="busy === 'enable'"
                :disabled="enableCode.replace(/\s/g, '').length !== 6"
                @click="enable"
              >
                确认绑定
              </el-button>
              <el-button @click="setup = undefined">取消</el-button>
            </div>
          </div>
        </div>
      </div>

      <div v-else class="sec-card">
        <p class="sec-desc">
          启用后,网页登录除了密码还需要手机验证器上的动态验证码;其他已登录设备会被登出。
          WebDAV 客户端与命令行 API 将不再接受登录密码,需改用下方的应用密码。
        </p>
        <div class="row">
          <el-button type="primary" :icon="Lock" :loading="busy === 'setup'" @click="beginSetup">
            启用两步验证
          </el-button>
        </div>
      </div>
    </div>

    <div class="block">
      <div class="block-head">WebDAV 应用密码</div>
      <div class="sec-card">
        <p class="sec-desc">
          供 rclone、Finder、Windows 资源管理器等 WebDAV 客户端及命令行 API 使用,用户名不变。
          应用密码随时可用;启用两步验证后,这些客户端<b>只能</b>使用应用密码。
        </p>
        <div v-if="newWebdavPassword" class="secret">
          <code>{{ newWebdavPassword }}</code>
          <el-button link :icon="CopyDocument" @click="copyText(newWebdavPassword)" />
        </div>
        <p v-if="newWebdavPassword" class="sec-desc warn-text">
          应用密码只显示这一次,请现在填进客户端。
        </p>
        <div class="sec-state">
          <el-tag :type="status.webdav_password_set ? 'success' : 'info'" round disable-transitions>
            {{ status.webdav_password_set ? '已设置' : '未设置' }}
          </el-tag>
        </div>
        <div class="row">
          <el-button :icon="Key" :loading="busy === 'webdav'" @click="generateWebdav">
            {{ status.webdav_password_set ? '重新生成' : '生成应用密码' }}
          </el-button>
          <el-button
            v-if="status.webdav_password_set"
            type="danger"
            plain
            :loading="busy === 'revoke'"
            @click="revokeWebdav"
          >
            撤销
          </el-button>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { CopyDocument, Key, Lock, WarnTriangleFilled } from '@element-plus/icons-vue'
import { api, errMsg, type SecurityStatus, type TotpSetup } from '../../api'
import { appConfirm } from '../../dialog'
import { copyText } from '../../mobile/share'

type Busy = '' | 'setup' | 'enable' | 'disable' | 'recovery' | 'webdav' | 'revoke'

const status = ref<SecurityStatus>()
const setup = ref<TotpSetup>()
const enableCode = ref('')
const manageCode = ref('')
const recoveryCodes = ref<string[]>([])
const newWebdavPassword = ref('')
const busy = ref<Busy>('')

/** 每 4 位一组,手动输入时不容易看串 */
const groupedSecret = computed(() => setup.value?.secret.replace(/(.{4})/g, '$1 ').trim() ?? '')

async function load() {
  try {
    status.value = (await api.admin.security.status()).data
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

async function run(kind: Busy, action: () => Promise<void>) {
  busy.value = kind
  try {
    await action()
  } catch (e) {
    if (e !== 'cancel') ElMessage.error(errMsg(e))
  } finally {
    busy.value = ''
  }
}

function beginSetup() {
  return run('setup', async () => {
    setup.value = (await api.admin.security.totpSetup()).data
    enableCode.value = ''
  })
}

function enable() {
  if (enableCode.value.replace(/\s/g, '').length !== 6) return
  return run('enable', async () => {
    const { data } = await api.admin.security.totpEnable(enableCode.value)
    recoveryCodes.value = data.recovery_codes
    setup.value = undefined
    ElMessage.success('两步验证已启用,其他设备需要重新登录')
    await load()
  })
}

function disable() {
  return run('disable', async () => {
    await appConfirm({
      title: '关闭两步验证',
      message: '关闭后网页登录只需要密码,WebDAV 客户端也会重新接受登录密码。',
      kind: 'danger',
      confirmText: '关闭',
    })
    await api.admin.security.totpDisable(manageCode.value)
    manageCode.value = ''
    ElMessage.success('两步验证已关闭')
    await load()
  })
}

function regenerate() {
  return run('recovery', async () => {
    const { data } = await api.admin.security.regenerateRecovery(manageCode.value)
    manageCode.value = ''
    recoveryCodes.value = data.recovery_codes
    await load()
  })
}

function generateWebdav() {
  return run('webdav', async () => {
    if (status.value?.webdav_password_set) {
      await appConfirm({
        title: '重新生成应用密码',
        message: '旧的应用密码会立即失效,使用它的 WebDAV 客户端需要更新。',
        kind: 'warning',
        confirmText: '重新生成',
      })
    }
    newWebdavPassword.value = (await api.admin.security.generateWebdavPassword()).data.password
    await load()
  })
}

function revokeWebdav() {
  return run('revoke', async () => {
    await appConfirm({
      title: '撤销应用密码',
      message: '使用应用密码的 WebDAV 客户端将无法再连接。',
      kind: 'danger',
      confirmText: '撤销',
    })
    await api.admin.security.revokeWebdavPassword()
    newWebdavPassword.value = ''
    await load()
  })
}

onMounted(load)
</script>

<style scoped>
.security {
  display: flex;
  flex-direction: column;
  gap: 14px;
  margin-top: 14px;
}
.sec-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 14px;
  border: 1px solid var(--glass-border);
  border-radius: 12px;
  background: var(--tile-bg);
}
.sec-desc {
  margin: 0;
  color: var(--text-soft);
  font-size: 13px;
  line-height: 1.6;
}
.warn-text {
  color: var(--el-color-warning);
}
.sec-state {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 13px;
}
.sec-label {
  color: var(--text-soft);
  font-size: 12px;
}
.setup {
  display: flex;
  flex-wrap: wrap;
  gap: 16px;
  align-items: flex-start;
}
.qr {
  flex: none;
  width: 200px;
  height: 200px;
  padding: 8px;
  border-radius: 10px;
  background: #fff;
  box-sizing: content-box;
}
.qr :deep(svg) {
  display: block;
  width: 100%;
  height: 100%;
}
.setup-side {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 10px;
  min-width: 220px;
}
.secret {
  display: flex;
  align-items: center;
  gap: 6px;
}
.secret code,
.codes code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 14px;
  letter-spacing: 0.5px;
  word-break: break-all;
}
.codes {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(120px, 1fr));
  gap: 8px;
}
.codes code {
  padding: 6px 10px;
  border-radius: 8px;
  background: var(--el-fill-color-light);
  text-align: center;
}
.code-input {
  max-width: 220px;
}
.sec-card > .note {
  margin: 0;
}
</style>
