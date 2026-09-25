<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      <div>代理用于 tgdrive 的 Telegram 上传、下载、MTProto 流式播放、Bot 收件及 WebDAV / S3 备份。</div>
      <div>保存后新请求使用新配置，无需重启；已发出的请求、已打开的下载流和本轮收件继续使用原连接。</div>
    </div>
  </div>
  <div class="fields">
    <SettingField
      label="启用代理"
      :source="cfg?.sources.proxy_enabled"
      inline
      wide
      help="关闭时保留代理地址，tgdrive 恢复直连，不再使用系统 HTTP_PROXY / ALL_PROXY。"
      @reset="resetKey('proxy_enabled', '代理开关')"
    >
      <el-switch v-model="form.proxy_enabled" aria-label="启用代理" />
    </SettingField>
    <SettingField
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
  </div>
  <div class="note warn">
    <el-icon><WarnTriangleFilled /></el-icon>
    <div>
      <div>HTTP 代理需允许 HTTP CONNECT 才能用于 MTProto 流式播放。</div>
      <div>此设置不会修改独立的 telegram-bot-api 服务；该服务访问 Telegram 的代理需另行配置。</div>
      <div>HTTP 请求到 localhost、127.0.0.1、::1 和 telegram-bot-api 时保持直连，避免本地服务被转发到远端代理。</div>
      <div>Docker 中的 127.0.0.1 指容器本身。使用宿主机代理时，请填写容器可访问的宿主机地址并允许容器连接。</div>
      <div>「测试连接」使用当前表单中的代理、Bot Token 和存储频道，不会保存配置。</div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled, WarnTriangleFilled } from '@element-plus/icons-vue'
import { api } from '../../api'
import SettingField from '../../components/SettingField.vue'
import { useAdminContext } from './useAdmin'

const {
  cfg,
  form,
  resetKey,
} = useAdminContext()
</script>
