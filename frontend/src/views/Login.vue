<template>
  <div class="login-wrap">
    <el-card class="login-card">
      <template #header>
        <div class="login-head">
          <div class="logo-badge login-logo"><el-icon><Promotion /></el-icon></div>
          <div class="login-brand brand-text">tgdrive</div>
          <div class="login-sub">Telegram 网盘 · 空间无限</div>
        </div>
      </template>
      <el-form @submit.prevent="doLogin" label-position="top">
        <el-form-item label="用户名">
          <el-input v-model="username" autocomplete="username" @keydown.enter="onEnter" />
        </el-form-item>
        <el-form-item label="密码">
          <el-input
            v-model="password"
            type="password"
            show-password
            autocomplete="current-password"
            @keydown.enter="onEnter"
          />
        </el-form-item>
        <el-form-item v-if="needTotp" label="两步验证码">
          <el-input
            ref="totpInput"
            v-model="totp"
            inputmode="numeric"
            autocomplete="one-time-code"
            placeholder="验证器上的 6 位数字,或一个恢复码"
            @keydown.enter="onEnter"
          />
        </el-form-item>
        <el-button type="primary" style="width: 100%" :loading="loading" @click="doLogin">
          {{ needTotp ? '验证并登录' : '登录' }}
        </el-button>
      </el-form>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { nextTick, ref } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Promotion } from '@element-plus/icons-vue'
import { api, errCode, errMsg } from '../api'
import { isComposingKey } from '../mobile/keyboard'

const router = useRouter()
const username = ref('')
const password = ref('')
const totp = ref('')
/** 密码校验通过但账号启用了两步验证,后端返回 totp_required 后才显示验证码框 */
const needTotp = ref(false)
const totpInput = ref<{ focus: () => void }>()
const loading = ref(false)

function onEnter(event: Event) {
  if (isComposingKey(event)) return
  event.preventDefault()
  void doLogin()
}

async function doLogin() {
  if (loading.value || !username.value || !password.value) return
  if (needTotp.value && !totp.value.trim()) return
  loading.value = true
  try {
    await api.login(username.value, password.value, needTotp.value ? totp.value : undefined)
    router.push('/files/')
  } catch (e) {
    const code = errCode(e)
    if (code === 'totp_required') {
      needTotp.value = true
      await nextTick()
      totpInput.value?.focus()
      return
    }
    if (code === 'totp_invalid') totp.value = ''
    ElMessage.error(errMsg(e))
  } finally {
    loading.value = false
  }
}
</script>

<style scoped>
.login-wrap {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: var(--app-height, 100dvh);
  padding: max(24px, var(--safe-top)) max(16px, var(--safe-right)) max(24px, var(--safe-bottom)) max(16px, var(--safe-left));
  box-sizing: border-box;
}
.login-card {
  width: 380px;
  max-width: 100%;
  border-radius: 20px;
}
.login-head {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
}
.login-logo {
  width: 52px;
  height: 52px;
  border-radius: 16px;
  font-size: 28px;
}
.login-brand {
  text-align: center;
  font-size: 28px;
  font-weight: 800;
  letter-spacing: 1px;
}
.login-sub {
  text-align: center;
  color: var(--text-soft);
  font-size: 12px;
}
@media (max-width: 767px) { .login-wrap { align-items: flex-start; padding-top: max(56px, var(--safe-top)); }.login-card { border: 0; }.login-sub { font-size: 14px; } }
</style>
