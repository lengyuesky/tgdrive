<template>
  <main class="mobile-page me-page">
    <header class="page-heading"><router-link to="/files/">返回文件</router-link><h1>我的</h1></header>
    <section class="group-card account-card"><div class="logo-badge">TG</div><div><h2>{{ username || '我的网盘' }}</h2><p>文件随身，随时访问</p></div></section>
    <section class="group-card menu-group" aria-label="网盘管理">
      <router-link to="/files/?view=favorites"><el-icon><Star /></el-icon>收藏<span>›</span></router-link>
      <button @click="shares?.open()"><el-icon><Share /></el-icon>分享管理<span>›</span></button>
      <button @click="trash?.open()"><el-icon><Delete /></el-icon>回收站<span>›</span></button>
      <router-link to="/transfers"><el-icon><UploadFilled /></el-icon>传输任务<span>›</span></router-link>
    </section>
    <section class="group-card menu-group" aria-label="偏好设置">
      <button @click="toggleDark"><el-icon><component :is="isDark ? Sunny : Moon" /></el-icon>{{ isDark ? '切换亮色' : '切换暗色' }}<span>›</span></button>
      <button v-if="!isStandalone" @click="installOpen = true"><el-icon><Plus /></el-icon>添加到主屏幕<span>›</span></button>
      <router-link to="/admin"><el-icon><Setting /></el-icon>系统设置<span>›</span></router-link>
      <button class="danger-text" @click="logout"><el-icon><SwitchButton /></el-icon>退出登录<span>›</span></button>
    </section>
    <p class="page-note">主屏幕模式依然需要联网；iOS 锁屏或切到后台可能暂停上传。</p>
    <ShareManager ref="shares" /><TrashPanel ref="trash" />
    <ResponsivePanel v-model="installOpen" title="把网盘放到主屏幕" sheet width="460px">
      <ol class="install-steps"><li>使用 <strong>Safari</strong> 打开此网盘。</li><li>点击浏览器的<strong>分享按钮（方框向上箭头）</strong>。</li><li>在菜单中选择<strong>添加到主屏幕</strong>，然后点击“添加”。</li><li>从主屏幕图标打开，即可使用独立窗口。</li></ol>
      <p class="page-note">若菜单中未显示此项，请检查是否使用 Safari。安装不会下载你的私人文件，也不提供离线阅读。</p>
      <template #footer><el-button type="primary" @click="dismissInstall">知道了</el-button></template>
    </ResponsivePanel>
  </main>
</template>
<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { Delete, Moon, Plus, Setting, Share, Star, Sunny, SwitchButton, UploadFilled } from '@element-plus/icons-vue'
import { api, errMsg } from '../api'
import { ElMessage } from 'element-plus'
import { appConfirm } from '../dialog'
import { isDark, toggleDark } from '../theme'
import { isStandalone, mobile } from '../mobile/viewport'
import ShareManager from '../components/ShareManager.vue'
import TrashPanel from '../components/TrashPanel.vue'
import ResponsivePanel from '../components/ResponsivePanel.vue'
const router = useRouter()
const username = ref('')
const shares = ref<InstanceType<typeof ShareManager>>()
const trash = ref<InstanceType<typeof TrashPanel>>()
const installOpen = ref(false)
function dismissInstall() { installOpen.value = false; localStorage.setItem('tgdrive.install-dismissed', String(Date.now())) }
onMounted(async () => {
  try { username.value = (await api.me()).data.username } catch { /* 认证拦截器处理失效会话。 */ }
  const dismissed = Number(localStorage.getItem('tgdrive.install-dismissed') || 0)
  if (mobile.value && !isStandalone.value && Date.now() - dismissed > 30 * 86400_000) {
    installOpen.value = true
    localStorage.setItem('tgdrive.install-dismissed', String(Date.now()))
  }
})
async function logout() {
  try {
    await appConfirm({ title: '退出登录', message: '当前上传将暂停，可续传会话会保留。确定退出吗？', confirmText: '退出登录' })
  } catch { return }
  try {
    await api.logout()
    await router.push('/login')
  } catch (error) { ElMessage.error(`退出登录失败：${errMsg(error)}`) }
}
</script>
<style scoped>
.account-card { display: flex; gap: 16px; align-items: center; }.account-card h2 { margin: 0; font-size: 20px; }.account-card p { margin: 6px 0 0; color: var(--text-soft); font-size: 14px; }
.menu-group { padding: 0 !important; overflow: hidden; }.menu-group > * { display: flex; align-items: center; gap: 12px; width: 100%; min-height: 56px; padding: 12px 16px; border: 0; border-bottom: 1px solid var(--glass-border); text-decoration: none; background: transparent; color: var(--el-text-color-primary); font: inherit; box-sizing: border-box; text-align: left; cursor: pointer; }.menu-group > *:last-child { border-bottom: 0; }.menu-group span { margin-left: auto; color: var(--text-soft); }.menu-group .danger-text { color: var(--el-color-danger); }.install-steps { padding-left: 24px; line-height: 1.8; }.install-steps li { margin-bottom: 16px; }
</style>
