<template>
  <div class="fields">
    <SettingField
      label="用户名"
      :source="cfg?.sources.username"
      @reset="resetKey('username', '用户名')"
    >
      <el-input v-model="form.username" autocomplete="username" />
    </SettingField>
    <SettingField
      label="密码"
      :source="cfg?.sources.password"
      @reset="resetKey('password', '密码')"
    >
      <el-input
        v-model="form.password"
        type="password"
        show-password
        autocomplete="new-password"
      />
    </SettingField>
  </div>
  <div class="note warn">
    <el-icon><WarnTriangleFilled /></el-icon>
    <div>
      改动账号后所有设备的登录状态立即失效,需要重新登录;使用登录密码的 WebDAV
      客户端也要同步修改。密码以 argon2 哈希保存,数据库里不留明文。
    </div>
  </div>
  <SecurityPanel />
</template>

<script setup lang="ts">
import { WarnTriangleFilled } from '@element-plus/icons-vue'
import SettingField from '../../components/SettingField.vue'
import SecurityPanel from './SecurityPanel.vue'
import { useAdminContext } from './useAdmin'

const {
  cfg,
  form,
  resetKey,
} = useAdminContext()
</script>
