<template>
  <el-dialog v-model="visible" :title="`分享「${name}」`" width="460px" align-center append-to-body>
    <!-- 第一步:设置选项 -->
    <el-form v-if="!created" label-width="90px" label-position="left">
      <el-form-item label="提取码">
        <div class="row">
          <el-input v-model="password" placeholder="留空 = 无需提取码" maxlength="16" />
          <el-button @click="password = randomCode()">随机</el-button>
        </div>
      </el-form-item>
      <el-form-item label="有效期">
        <el-select v-model="expiresHours" style="width: 100%">
          <el-option label="永久有效" :value="0" />
          <el-option label="1 天" :value="24" />
          <el-option label="7 天" :value="168" />
          <el-option label="30 天" :value="720" />
        </el-select>
      </el-form-item>
      <el-form-item label="下载次数">
        <el-input-number v-model="maxDownloads" :min="0" :max="99999" />
        <span class="tip" style="margin-left: 8px">0 = 不限次数</span>
      </el-form-item>
    </el-form>

    <!-- 第二步:展示结果 -->
    <div v-else class="result">
      <div class="ok-line">
        <el-icon color="#22c55e"><CircleCheckFilled /></el-icon> 分享创建成功
      </div>
      <el-input :model-value="url" readonly>
        <template #prepend>链接</template>
      </el-input>
      <el-input v-if="password" :model-value="password" readonly style="margin-top: 8px">
        <template #prepend>提取码</template>
      </el-input>
    </div>

    <template #footer>
      <template v-if="!created">
        <el-button @click="visible = false">取消</el-button>
        <el-button type="primary" :loading="loading" @click="create">创建分享</el-button>
      </template>
      <template v-else>
        <el-button @click="visible = false">关闭</el-button>
        <el-button type="primary" @click="copy">复制链接与提取码</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import { CircleCheckFilled } from '@element-plus/icons-vue'
import { api, errMsg } from '../api'

const visible = ref(false)
const name = ref('')
const path = ref('')
const password = ref('')
const expiresHours = ref(0)
const maxDownloads = ref(0)
const loading = ref(false)
const created = ref(false)
const url = ref('')

function randomCode() {
  const chars = 'abcdefghjkmnpqrstuvwxyz23456789'
  return Array.from({ length: 4 }, () => chars[Math.floor(Math.random() * chars.length)]).join('')
}

function open(p: string, n: string) {
  path.value = p
  name.value = n
  password.value = ''
  expiresHours.value = 0
  maxDownloads.value = 0
  created.value = false
  visible.value = true
}

async function create() {
  loading.value = true
  try {
    const { data } = await api.shares.create({
      path: path.value,
      password: password.value.trim() || undefined,
      expires_hours: expiresHours.value || undefined,
      max_downloads: maxDownloads.value || undefined,
    })
    url.value = `${location.origin}/s/${data.share.token}`
    created.value = true
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    loading.value = false
  }
}

async function copy() {
  const text = password.value ? `${url.value}  提取码: ${password.value}` : url.value
  try {
    await navigator.clipboard.writeText(text)
    ElMessage.success('已复制')
  } catch {
    ElMessage.warning('浏览器不允许自动复制,请手动复制')
  }
}

defineExpose({ open })
</script>

<style scoped>
.row {
  display: flex;
  gap: 8px;
  width: 100%;
}
.tip {
  color: var(--text-soft);
  font-size: 12px;
}
.ok-line {
  display: flex;
  align-items: center;
  gap: 6px;
  font-weight: 600;
  margin-bottom: 12px;
}
</style>
