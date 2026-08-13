<template>
  <el-dialog v-model="visible" :title="name" width="70%" top="5vh" destroy-on-close>
    <div class="preview-body">
      <img v-if="kind === 'image'" :src="url" class="preview-media" />
      <video v-else-if="kind === 'video'" :src="url" controls autoplay class="preview-media" />
      <audio v-else-if="kind === 'audio'" :src="url" controls style="width: 100%" />
      <iframe v-else-if="kind === 'pdf'" :src="url" class="preview-frame" />
      <pre v-else-if="kind === 'text'" class="preview-text">{{ text }}</pre>
      <el-empty v-else description="该类型暂不支持预览,请直接下载" />
    </div>
    <template #footer>
      <el-button @click="download">下载</el-button>
      <el-button type="primary" @click="visible = false">关闭</el-button>
    </template>
  </el-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import axios from 'axios'
import { api } from '../api'

const visible = ref(false)
const path = ref('')
const name = computed(() => path.value.split('/').pop() ?? '')
const url = computed(() => api.downloadUrl(path.value, true))
const text = ref('')

const EXT: Record<string, string[]> = {
  image: ['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg', 'bmp', 'avif', 'ico'],
  video: ['mp4', 'webm', 'ogv', 'mov', 'm4v'],
  audio: ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus'],
  pdf: ['pdf'],
  text: [
    'txt', 'md', 'log', 'json', 'yaml', 'yml', 'toml', 'ini', 'csv', 'xml', 'html', 'css',
    'js', 'ts', 'rs', 'py', 'go', 'java', 'c', 'h', 'cpp', 'sh', 'sql', 'conf',
  ],
}

const kind = computed(() => {
  const ext = name.value.split('.').pop()?.toLowerCase() ?? ''
  for (const [k, list] of Object.entries(EXT)) if (list.includes(ext)) return k
  return 'other'
})

watch(visible, async (v) => {
  if (v && kind.value === 'text') {
    text.value = '加载中 ...'
    try {
      // 只取前 256KB,避免大文件卡死
      const r = await axios.get(url.value, {
        headers: { Range: 'bytes=0-262143' },
        responseType: 'text',
        transformResponse: [(d) => d],
      })
      text.value = r.data
    } catch {
      text.value = '(读取失败)'
    }
  }
})

function open(p: string) {
  path.value = p
  visible.value = true
}

function download() {
  window.open(api.downloadUrl(path.value), '_blank')
}

defineExpose({ open })
</script>

<style scoped>
.preview-body {
  display: flex;
  justify-content: center;
  max-height: 70vh;
  overflow: auto;
}
.preview-media {
  max-width: 100%;
  max-height: 68vh;
}
.preview-frame {
  width: 100%;
  height: 68vh;
  border: none;
}
.preview-text {
  width: 100%;
  white-space: pre-wrap;
  word-break: break-all;
  font-size: 13px;
  background: rgba(255, 255, 255, 0.5);
  border-radius: 10px;
  padding: 12px;
  margin: 0;
}
</style>
