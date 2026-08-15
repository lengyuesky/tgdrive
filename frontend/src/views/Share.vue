<template>
  <div class="wrap">
    <div class="card glass">
      <div class="head">
        <div class="logo-badge"><el-icon><Promotion /></el-icon></div>
        <div class="brand brand-text">tgdrive</div>
        <div class="sub">文件分享</div>
      </div>

      <!-- 加载失败(过期/不存在) -->
      <el-result v-if="error" icon="warning" :title="error" sub-title="请与分享者确认链接是否有效" />

      <template v-else-if="meta">
        <!-- 提取码 -->
        <div v-if="meta.need_password && !unlocked" class="pwd-box">
          <div class="fname">
            <el-icon v-if="meta.is_dir" color="#f6a821"><Folder /></el-icon>
            <el-icon v-else color="#64748b"><Document /></el-icon>
            {{ meta.name }}
          </div>
          <el-input
            v-model="pwd"
            placeholder="请输入提取码"
            maxlength="16"
            @keyup.enter="unlock"
          />
          <el-button type="primary" :loading="unlocking" style="width: 100%" @click="unlock">
            提取文件
          </el-button>
        </div>

        <!-- 单文件 -->
        <div v-else-if="!meta.is_dir" class="file-box">
          <el-image
            v-if="isImage(meta.name)"
            class="preview"
            :src="api.share.downloadUrl(token, '', true)"
            fit="contain"
          />
          <el-icon v-else class="big-ico" color="#64748b"><Document /></el-icon>
          <div class="fname">{{ meta.name }}</div>
          <div class="fmeta">
            {{ fmtSize(meta.size) }}
            <span v-if="meta.max_downloads"> · 剩余下载 {{ remain }}</span>
            <span v-if="meta.expires_at"> · {{ fmtTime(meta.expires_at) }} 过期</span>
          </div>
          <el-button type="primary" size="large" :icon="Download" @click="downloadRoot">
            下载文件
          </el-button>
        </div>

        <!-- 目录 -->
        <div v-else class="dir-box">
          <el-breadcrumb separator="/">
            <el-breadcrumb-item>
              <span class="clickable" @click="goto('')">{{ meta.name }}</span>
            </el-breadcrumb-item>
            <el-breadcrumb-item v-for="(seg, i) in segs" :key="i">
              <span class="clickable" @click="goto(segs.slice(0, i + 1).join('/'))">{{ seg }}</span>
            </el-breadcrumb-item>
          </el-breadcrumb>
          <el-table :data="entries" v-loading="listing" size="small" class="dir-table">
            <el-table-column label="名称" min-width="220">
              <template #default="{ row }">
                <span class="name-cell clickable" @click="openEntry(row as Entry)">
                  <el-icon v-if="row.is_dir" color="#f6a821"><Folder /></el-icon>
                  <el-icon v-else color="#64748b"><Document /></el-icon>
                  {{ row.name }}
                </span>
              </template>
            </el-table-column>
            <el-table-column label="大小" width="100">
              <template #default="{ row }">{{ row.is_dir ? '—' : fmtSize(row.size) }}</template>
            </el-table-column>
            <el-table-column label="" width="90" align="right">
              <template #default="{ row }">
                <el-button
                  v-if="!row.is_dir"
                  text
                  size="small"
                  type="primary"
                  @click="dl(row as Entry)"
                >
                  下载
                </el-button>
              </template>
            </el-table-column>
            <template #empty><el-empty description="空目录" :image-size="64" /></template>
          </el-table>
        </div>
      </template>

      <div v-else class="loading-box" v-loading="true" />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Document, Download, Folder, Promotion } from '@element-plus/icons-vue'
import { api, errMsg, fmtSize, fmtTime, type Entry, type ShareMeta } from '../api'

const route = useRoute()
const token = String(route.params.token ?? '')
const meta = ref<ShareMeta | null>(null)
const error = ref('')
const pwd = ref('')
const unlocked = ref(false)
const unlocking = ref(false)
const entries = ref<Entry[]>([])
const listing = ref(false)
const rel = ref('')
const segs = computed(() => rel.value.split('/').filter(Boolean))
const remain = computed(() =>
  meta.value?.max_downloads ? Math.max(0, meta.value.max_downloads - meta.value.downloads) : 0,
)

const IMG_EXT = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'avif', 'ico']
function isImage(name: string) {
  return IMG_EXT.includes(name.split('.').pop()?.toLowerCase() ?? '')
}

onMounted(async () => {
  try {
    meta.value = (await api.share.meta(token)).data
    if (!meta.value.need_password) {
      await api.share.authorize(token)
      unlocked.value = true
      if (meta.value.is_dir) await goto('')
    }
  } catch (e) {
    error.value = errMsg(e)
  }
})

async function unlock() {
  if (!pwd.value.trim()) return
  unlocking.value = true
  try {
    await api.share.authorize(token, pwd.value)
    pwd.value = ''
    if (meta.value?.is_dir) {
      await goto('')
    }
    unlocked.value = true
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    unlocking.value = false
  }
}

async function goto(p: string) {
  listing.value = true
  try {
    entries.value = (await api.share.list(token, p)).data.entries
    rel.value = p
  } catch (e) {
    ElMessage.error(errMsg(e))
    throw e
  } finally {
    listing.value = false
  }
}

function openEntry(row: Entry) {
  if (row.is_dir) {
    goto(rel.value ? `${rel.value}/${row.name}` : row.name).catch(() => {})
  } else {
    dl(row)
  }
}

function dl(row: Entry) {
  const p = rel.value ? `${rel.value}/${row.name}` : row.name
  window.open(api.share.downloadUrl(token, p), '_blank')
}

function downloadRoot() {
  window.open(api.share.downloadUrl(token), '_blank')
}
</script>

<style scoped>
.wrap {
  min-height: 100vh;
  display: flex;
  align-items: flex-start;
  justify-content: center;
  padding: 48px 16px;
  box-sizing: border-box;
}
.card {
  width: 640px;
  max-width: 100%;
  border-radius: 20px;
  padding: 28px 28px 32px;
}
.head {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 20px;
}
.brand {
  font-size: 20px;
  font-weight: 800;
}
.sub {
  color: var(--text-soft);
  font-size: 13px;
}
.pwd-box {
  display: flex;
  flex-direction: column;
  gap: 12px;
  max-width: 320px;
  margin: 24px auto;
}
.file-box {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  padding: 8px 0 4px;
}
.preview {
  max-width: 100%;
  max-height: 340px;
  border-radius: 12px;
  overflow: hidden;
}
.big-ico {
  font-size: 84px;
}
.fname {
  font-size: 16px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 6px;
  word-break: break-all;
}
.fmeta {
  color: var(--text-soft);
  font-size: 13px;
}
.dir-table {
  margin-top: 10px;
}
.loading-box {
  height: 160px;
}
</style>
