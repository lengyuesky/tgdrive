<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      <div>代理用于 Telegram 上传、下载、MTProto 流式播放、Bot 收件及 WebDAV / S3 备份。项目内的自建 Bot API 共用这份配置。</div>
      <div>保存后无需重启容器。自建 Bot API 会重新连接 Telegram，在途传输可能短暂停顿；其他已打开的下载流继续使用原连接。</div>
    </div>
  </div>
  <div class="fields">
    <SettingField
      label="代理方式"
      :source="cfg?.sources.proxy_provider"
      wide
      @reset="resetKey('proxy_provider', '代理设置（方式、地址与开关）')"
    >
      <el-select v-model="form.proxy_provider" aria-label="代理方式">
        <el-option label="自定义代理（HTTP / SOCKS5）" value="manual" />
        <el-option label="usque（Cloudflare WARP）" value="usque" />
      </el-select>
      <template #help>usque 使用 Cloudflare WARP 提供代理，无需填写代理地址；切换方式会保留自定义地址。</template>
    </SettingField>
    <SettingField
      label="启用代理"
      :source="cfg?.sources.proxy_enabled"
      inline
      wide
      help="关闭时保留代理地址，tgdrive 和项目内的自建 Bot API 恢复直连。"
      @reset="resetKey('proxy_enabled', '代理开关')"
    >
      <el-switch v-model="form.proxy_enabled" aria-label="启用代理" />
    </SettingField>
    <SettingField
      v-if="form.proxy_provider === 'manual'"
      label="代理地址"
      :source="cfg?.sources.proxy_url"
      wide
      @reset="resetKey('proxy_url', '代理设置（地址与开关）')"
    >
      <el-input
        v-model="form.proxy_url"
        type="password"
        show-password
        clearable
        autocomplete="new-password"
        aria-label="代理地址"
        placeholder="http://127.0.0.1:7890 或 socks5h://127.0.0.1:1080"
      />
      <template #help>
        HTTP：<code>http://主机:端口</code>；SOCKS5：<code>socks5://主机:端口</code>，
        推荐 <code>socks5h://主机:端口</code> 由代理解析目标域名，<code>socks://</code> 与之等效。
        需要认证时使用 <code>协议://用户名:密码@主机:端口</code>，特殊字符需 URL 编码。
        已保存的认证信息会脱敏；不修改可保留原样，修改地址时请重新输入完整凭据。
      </template>
    </SettingField>
    <SettingField v-else label="usque 状态" wide>
      <div role="status">{{ usque?.message ?? '正在读取状态…' }}</div>
      <div v-if="!usque?.registered" class="usque-setup">
        <p>
          首次使用需要注册 WARP 设备。点击下方按钮表示同意
          <a href="https://www.cloudflare.com/application/terms/" target="_blank" rel="noopener noreferrer">Cloudflare 服务条款</a>。
        </p>
        <el-button :loading="registering" @click="registerUsque">同意条款并初始化 usque</el-button>
      </div>
      <template #help>
        <a href="https://github.com/Diniboy1123/usque" target="_blank" rel="noopener noreferrer">usque 项目</a>
        · 注册信息会保存在数据目录，容器重建后自动复用。默认通过 HTTP/2（TCP/443）连接 Cloudflare；初始化后开启代理并保存。
      </template>
    </SettingField>
  </div>
  <div class="note warn">
    <el-icon><WarnTriangleFilled /></el-icon>
    <div>
      <div v-if="form.proxy_provider === 'manual'">HTTP 代理需允许 HTTP CONNECT，才能连接 Telegram 数据中心。</div>
      <div>通过本项目 Compose 启动的 telegram-bot-api 自动共用代理；其他外部 Bot API 服务仍需自行配置网络。</div>
      <div>HTTP 请求到 localhost、127.0.0.1、::1 和 telegram-bot-api 时保持直连，避免本地服务被转发到远端代理。</div>
      <div v-if="form.proxy_provider === 'manual'">Docker 中的 127.0.0.1 指容器本身。使用宿主机代理时，请填写容器可访问的宿主机地址并允许容器连接。</div>
      <div>「测试连接」不会保存配置。usque 模式会额外验证 WARP 网络；自建 Bot API 访问 Telegram 使用已保存的代理，新的代理在保存后接管连接。</div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { onUnmounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { InfoFilled, WarnTriangleFilled } from '@element-plus/icons-vue'
import { api, errMsg, type UsqueStatus } from '../../api'
import SettingField from '../../components/SettingField.vue'
import { useAdminContext } from './useAdmin'

const {
  cfg,
  form,
  resetKey,
} = useAdminContext()

const usque = ref<UsqueStatus | undefined>(cfg.value?.usque)
const registering = ref(false)
let timer: ReturnType<typeof setTimeout> | undefined
let disposed = false

async function refreshUsque() {
  try { usque.value = (await api.admin.usqueStatus()).data } catch { /* 保留上次状态 */ }
  if (!disposed && form.value.proxy_provider === 'usque') timer = setTimeout(refreshUsque, 3000)
}
watch(() => form.value.proxy_provider, (provider) => {
  clearTimeout(timer)
  if (provider === 'usque') void refreshUsque()
}, { immediate: true })
onUnmounted(() => { disposed = true; clearTimeout(timer) })

async function registerUsque() {
  registering.value = true
  try {
    usque.value = (await api.admin.usqueRegister()).data
    ElMessage.success('usque 初始化完成，开启代理并保存即可使用')
  } catch (error) { ElMessage.error(errMsg(error)) }
  finally { registering.value = false }
}
</script>

<style scoped>
.usque-setup { margin-top: 12px; }
.usque-setup p { margin: 0 0 12px; line-height: 1.7; }
a { color: var(--el-color-primary); }
</style>
