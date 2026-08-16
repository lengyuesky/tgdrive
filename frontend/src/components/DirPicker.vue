<template>
  <el-dialog v-model="visible" :title="title" width="440px" align-center append-to-body>
    <div class="picked">
      目标:<code>{{ picked }}</code>
    </div>
    <div class="tree-box">
      <div class="root-row" :class="{ on: picked === '/' }" @click="picked = '/'">
        <el-icon color="#f6a821"><Folder /></el-icon>
        <span>根目录</span>
      </div>
      <el-tree
        :key="treeKey"
        lazy
        :load="loadDirs"
        :props="{ label: 'name' }"
        node-key="path"
        highlight-current
        :expand-on-click-node="false"
        @node-click="(d: DirNode) => (picked = d.path)"
      >
        <template #default="{ data }">
          <span class="node">
            <el-icon color="#f6a821"><Folder /></el-icon>
            <span class="node-name">{{ data.name }}</span>
          </span>
        </template>
      </el-tree>
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
import { ElMessage, type LoadFunction } from 'element-plus'
import { Folder, FolderAdd } from '@element-plus/icons-vue'
import { api, errMsg } from '../api'
import { appPrompt } from '../dialog'

const visible = ref(false)
const title = ref('选择目录')
const picked = ref('/')
const treeKey = ref(0)
let resolver: ((v: string | null) => void) | null = null

interface DirNode {
  name: string
  path: string
}

function joinPath(dir: string, name: string) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`
}

const loadDirs: LoadFunction = async (node, resolve) => {
  const base = node.level === 0 ? '/' : (node.data as DirNode).path
  try {
    const r = await api.list(base)
    resolve(
      r.data.entries
        .filter((e) => e.is_dir)
        .sort((a, b) => a.name.localeCompare(b.name, 'zh'))
        .map((e) => ({ name: e.name, path: joinPath(base, e.name) })),
    )
  } catch {
    resolve([])
  }
}

async function mkdirHere() {
  try {
    const { value } = await appPrompt({
      title: '新建文件夹',
      message: `将创建在 ${picked.value} 下`,
      placeholder: '输入文件夹名称',
      icon: FolderAdd,
      confirmText: '创建',
      pattern: /^[^/\\]+$/,
      errorMessage: '名称不能为空,且不能包含斜杠',
    })
    await api.mkdir(joinPath(picked.value, value.trim()))
    picked.value = joinPath(picked.value, value.trim())
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
