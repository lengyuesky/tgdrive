<template>
  <el-dialog v-model="visible" :title="title" width="440px" align-center append-to-body @close="finish(null)">
    <div class="picked">
      目标:<code>{{ picked }}</code>
    </div>
    <div class="tree-box">
      <div class="root-row" :class="{ on: picked === '/' }" @click="picked = '/'">
        <el-icon color="#f6a821"><Folder /></el-icon>
        <span>根目录</span>
      </div>
      <VirtualDirectoryTree v-if="visible" :key="treeKey" :current="picked" @select="(path) => picked = path" />
    </div>
    <template #footer>
      <el-button text :icon="FolderAdd" @click="mkdirHere">在此新建文件夹</el-button>
      <span class="spacer" />
      <el-button @click="finish(null)">取消</el-button>
      <el-button type="primary" @click="finish(picked)">确定</el-button>
    </template>
  </el-dialog>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import VirtualDirectoryTree from './VirtualDirectoryTree.vue'
import { Folder, FolderAdd } from '@element-plus/icons-vue'
import { api, errMsg } from '../api'
import { appPrompt } from '../dialog'

const visible = ref(false)
const title = ref('选择目录')
const picked = ref('/')
const treeKey = ref(0)
let resolver: ((v: string | null) => void) | null = null

function joinPath(dir: string, name: string) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`
}

async function mkdirHere() {
  const base = picked.value
  try {
    const { value } = await appPrompt({
      title: '新建文件夹',
      message: `将创建在 ${base} 下`,
      placeholder: '输入文件夹名称',
      icon: FolderAdd,
      confirmText: '创建',
      pattern: /^[^/\\]+$/,
      errorMessage: '名称不能为空,且不能包含斜杠',
    })
    await api.mkdir(joinPath(base, value.trim()))
    picked.value = joinPath(base, value.trim())
    treeKey.value++ // 重建树让新目录出现
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

function finish(v: string | null) {
  visible.value = false
  resolver?.(v)
  resolver = null
}

/** 打开选择器,resolve 选中的目录路径;取消时 resolve null */
function open(dialogTitle: string, initial = '/'): Promise<string | null> {
  resolver?.(null)
  title.value = dialogTitle
  picked.value = initial
  treeKey.value++
  visible.value = true
  return new Promise((res) => (resolver = res))
}

defineExpose({ open })
</script>

<style scoped>
.picked {
  margin-bottom: 8px;
  font-size: 13px;
}
.picked code {
  word-break: break-all;
}
.tree-box {
  max-height: 320px;
  overflow: auto;
  border: 1px solid var(--glass-border);
  border-radius: 10px;
  padding: 6px;
}
.root-row {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 14px;
}
.root-row:hover {
  background: var(--tile-bg-hover);
}
.root-row.on {
  color: var(--el-color-primary);
  background: rgba(64, 158, 255, 0.12);
}
.node {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}
.node-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.spacer {
  flex: 1;
}
:deep(.el-dialog__footer) {
  display: flex;
  align-items: center;
}
</style>
