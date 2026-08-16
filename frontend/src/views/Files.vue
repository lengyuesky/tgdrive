<template>
  <div class="page" @dragover.prevent @drop.prevent="onDrop">
    <!-- 顶栏:Logo · 搜索 · 统计 · 操作按钮 · 状态灯 · 设置/退出 -->
    <div class="topbar glass">
      <div class="logo-badge"><el-icon><Promotion /></el-icon></div>
      <div class="brand brand-text">tgdrive</div>

      <el-input
        v-model="query"
        class="search"
        placeholder="过滤当前目录,回车全局搜索"
        :prefix-icon="Search"
        clearable
        @keyup.enter="doGlobalSearch"
      />

      <div v-if="info" class="stat-badge hide-sm">
        <el-icon><TrendCharts /></el-icon>
        总文件 <b>{{ info.files }}</b>
        <span class="sep" />
        {{ fmtSize(info.total_size) }}
      </div>

      <div class="spacer" />

      <div class="btns">
        <el-dropdown trigger="click">
          <el-button class="icon-btn" type="primary" :icon="Upload" title="上传" />
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item @click="fileInput?.click()">上传文件</el-dropdown-item>
              <el-dropdown-item @click="dirInput?.click()">上传文件夹</el-dropdown-item>
              <el-dropdown-item disabled>Ctrl+V 可直接粘贴截图</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
        <el-tooltip content="新建文件夹">
          <el-button class="icon-btn" :icon="FolderAdd" @click="mkdir" />
        </el-tooltip>
        <el-tooltip content="刷新">
          <el-button class="icon-btn" :icon="Refresh" @click="refresh" />
        </el-tooltip>
        <el-tooltip content="全选 / 取消全选(Ctrl+A)">
          <el-button
            class="icon-btn"
            :class="{ active: allSelected }"
            :icon="Finished"
            @click="toggleSelectAll"
          />
        </el-tooltip>
        <el-tooltip v-if="selected.length" content="移动所选">
          <el-button class="icon-btn" :icon="Rank" @click="moveSelected" />
        </el-tooltip>
        <el-badge v-if="selected.length" :value="selected.length">
          <el-tooltip content="删除所选(进回收站)">
            <el-button class="icon-btn" type="danger" :icon="Delete" @click="removeSelected" />
          </el-tooltip>
        </el-badge>

        <span class="vsep" />

        <el-dropdown trigger="click" @command="onSortCmd">
          <el-button class="icon-btn" :icon="Sort" title="排序" />
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item
                v-for="s in SORTS"
                :key="s.key"
                :command="s.key"
                :class="{ 'sort-on': sortKey === s.key }"
                >{{ s.label }}</el-dropdown-item
              >
              <el-dropdown-item divided command="asc" :class="{ 'sort-on': sortAsc }">
                升序
              </el-dropdown-item>
              <el-dropdown-item command="desc" :class="{ 'sort-on': !sortAsc }">
                降序
              </el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>

        <el-tooltip content="列表视图">
          <el-button
            class="icon-btn"
            :class="{ active: viewMode === 'list' }"
            :icon="List"
            @click="viewMode = 'list'"
          />
        </el-tooltip>
        <el-tooltip content="网格视图">
          <el-button
            class="icon-btn"
            :class="{ active: viewMode === 'grid' }"
            :icon="Grid"
            @click="viewMode = 'grid'"
          />
        </el-tooltip>
        <el-tooltip content="大图视图">
          <el-button
            class="icon-btn"
            :class="{ active: viewMode === 'photo' }"
            :icon="Picture"
            @click="viewMode = 'photo'"
          />
        </el-tooltip>
        <el-tooltip content="时间线(按日期看图)">
          <el-button
            class="icon-btn"
            :class="{ active: viewMode === 'timeline' }"
            :icon="Calendar"
            @click="viewMode = 'timeline'"
          />
        </el-tooltip>

        <span class="vsep" />

        <el-tooltip :content="isDark ? '切换亮色' : '切换暗色'">
          <el-button class="icon-btn" :icon="isDark ? Sunny : Moon" @click="toggleDark" />
        </el-tooltip>
      </div>

      <div class="lights hide-sm">
        <el-tooltip :content="tgTip">
          <span
            class="status-dot"
            :class="info?.configured ? 'ok' : 'bad'"
            @click="router.push('/admin')"
            ><i />TG</span
          >
        </el-tooltip>
        <el-popover width="420" trigger="click">
          <template #reference>
            <span class="status-dot ok"><i />DAV</span>
          </template>
          <div style="font-size: 13px; line-height: 1.8">
            <div>地址:<code>{{ davUrl }}</code></div>
            <div>账号/密码与本页登录一致(Basic 认证)。</div>
            <div>rclone 示例:<code>rclone lsd :webdav,url='{{ davUrl }}',vendor=other,user=用户名,pass=$(rclone obscure 密码):</code></div>
          </div>
        </el-popover>
      </div>

      <span class="vsep" />

      <el-tooltip content="系统设置">
        <el-button class="icon-btn" :icon="Setting" @click="router.push('/admin')" />
      </el-tooltip>
      <el-tooltip :content="info ? `退出登录(${info.username})` : '退出登录'">
        <el-button class="icon-btn" :icon="SwitchButton" @click="logout" />
      </el-tooltip>
    </div>

    <el-alert
      v-if="info && !info.configured"
      type="warning"
      :closable="false"
      show-icon
      title="还没配置 Telegram 存储"
      class="setup-hint"
    >
      <div>上传下载暂时用不了。去「系统设置」填好 Bot Token 与存储频道即可,不用重启服务。</div>
      <el-button type="primary" size="small" style="margin-top: 8px" @click="router.push('/admin')">
        去配置
      </el-button>
    </el-alert>

    <!-- 面包屑独立胶囊条 -->
    <div class="crumb-bar glass">
      <el-icon class="crumb-ico"><Position /></el-icon>
      <el-breadcrumb separator="/" class="crumbs">
        <el-breadcrumb-item>
          <span class="clickable" @click="goto('/')">根目录</span>
        </el-breadcrumb-item>
        <el-breadcrumb-item v-for="(seg, i) in segments" :key="i">
          <span class="clickable" @click="goto(pathOf(i))">{{ seg }}</span>
        </el-breadcrumb-item>
      </el-breadcrumb>
      <div class="spacer" />
      <span class="crumb-count">{{ countText }}</span>
    </div>

    <div class="layout">
      <!-- 左侧悬浮目录树 -->
      <aside class="sidebar glass">
        <div class="side-head">
          <el-icon><FolderOpened /></el-icon>
          <span>目录</span>
          <div class="spacer" />
          <el-tooltip content="新建文件夹">
            <el-button text circle size="small" :icon="FolderAdd" @click="mkdir" />
          </el-tooltip>
          <el-tooltip content="刷新目录树">
            <el-button text circle size="small" :icon="Refresh" @click="treeKey++" />
          </el-tooltip>
        </div>
        <div class="side-root" :class="{ on: cur === '/' }" @click="goto('/')">
          <el-icon color="#f6a821"><Folder /></el-icon>
          <span>全部文件</span>
          <div class="spacer" />
          <span v-if="info" class="side-count">{{ info.files }}</span>
        </div>
        <el-tree
          ref="treeRef"
          :key="treeKey"
          class="side-tree"
          lazy
          :load="loadDirs"
          :props="{ label: 'name' }"
          node-key="path"
          highlight-current
          :current-node-key="cur"
          :expand-on-click-node="false"
          @node-click="(d: DirNode) => goto(d.path)"
        >
          <template #default="{ data }">
            <span class="tree-node">
              <el-icon color="#f6a821"><Folder /></el-icon>
              <span class="tree-name">{{ data.name }}</span>
            </span>
          </template>
        </el-tree>
        <div class="side-sep" />
        <div class="side-root" @click="trashPanel?.open()">
          <el-icon color="#94a3b8"><DeleteFilled /></el-icon>
          <span>回收站</span>
        </div>
        <div class="side-root" @click="shareMgr?.open()">
          <el-icon color="#0ea5e9"><Share /></el-icon>
          <span>分享管理</span>
        </div>
      </aside>

      <!-- 主内容 -->
      <div class="panel glass" @contextmenu="onBlankCtx">
        <!-- 全局搜索结果 -->
        <div v-if="searchMode">
          <div class="search-head">
            <span>
              全局搜索「<b>{{ lastQuery }}</b>」:{{ searchHits.length }} 个结果
              <template v-if="searchHits.length >= 200">(仅显示前 200 条)</template>
            </span>
            <el-button text size="small" @click="exitSearch">退出搜索</el-button>
          </div>
          <el-table :data="searchHits" size="default">
            <el-table-column label="名称" min-width="240">
              <template #default="{ row }">
                <span class="name-cell clickable" @click="openHit(row as SearchHit)">
                  <el-icon v-if="row.is_dir" class="fico" color="#f6a821"><Folder /></el-icon>
                  <el-icon v-else class="fico" :color="iconOf(row.name).color">
                    <component :is="iconOf(row.name).icon" />
                  </el-icon>
                  {{ row.name }}
                </span>
              </template>
            </el-table-column>
            <el-table-column label="位置" min-width="180">
              <template #default="{ row }">
                <span class="hit-path clickable" @click="goto(parentOf(row.path))">
                  {{ parentOf(row.path) }}
                </span>
              </template>
            </el-table-column>
            <el-table-column label="大小" width="110">
              <template #default="{ row }">{{ row.is_dir ? '—' : fmtSize(row.size) }}</template>
            </el-table-column>
            <el-table-column label="修改时间" width="170">
              <template #default="{ row }">{{ fmtTime(row.modified_at) }}</template>
            </el-table-column>
            <template #empty><el-empty description="没有找到匹配的内容" /></template>
          </el-table>
        </div>

        <el-table
          v-else-if="viewMode === 'list'"
          ref="tableRef"
          v-loading="loading"
          :data="displayEntries"
          class="table"
          @selection-change="(rows: Entry[]) => (selected = rows)"
          @row-contextmenu="onRowCtx"
        >
          <el-table-column type="selection" width="42" />
          <el-table-column label="名称" min-width="320" sortable :sort-by="(r: Entry) => (r.is_dir ? '0' : '1') + r.name">
            <template #default="{ row }">
              <span class="name-cell clickable" @click="openEntry(row)">
                <el-icon v-if="row.is_dir" class="fico" color="#f6a821"><Folder /></el-icon>
                <el-image
                  v-else-if="isImage(row.name)"
                  class="thumb"
                  :src="fileUrl(row)"
                  fit="cover"
                  lazy
                >
                  <template #placeholder>
                    <div class="thumb-fallback"><el-icon color="#10b981"><Picture /></el-icon></div>
                  </template>
                  <template #error>
                    <div class="thumb-fallback"><el-icon color="#10b981"><Picture /></el-icon></div>
                  </template>
                </el-image>
                <VideoThumb
                  v-else-if="isVideo(row.name)"
                  class="thumb"
                  :src="thumbnailUrl(row)"
                />
                <el-icon v-else class="fico" :color="iconOf(row.name).color">
                  <component :is="iconOf(row.name).icon" />
                </el-icon>
                {{ row.name }}
              </span>
            </template>
          </el-table-column>
          <el-table-column
            label="大小"
            width="120"
            sortable
            :sort-method="(a: Entry, b: Entry) => a.size - b.size"
          >
            <template #default="{ row }">{{ row.is_dir ? '—' : fmtSize(row.size) }}</template>
          </el-table-column>
          <el-table-column
            label="修改时间"
            width="180"
            sortable
            :sort-method="(a: Entry, b: Entry) => a.modified_at - b.modified_at"
          >
            <template #default="{ row }">{{ fmtTime(row.modified_at) }}</template>
          </el-table-column>
          <el-table-column label="操作" width="230" align="right">
            <template #default="{ row }">
              <el-button
                v-if="!row.is_dir"
                text
                size="small"
                @click="download(row as Entry)"
              >
                下载
              </el-button>
              <el-button text size="small" @click="rename(row as Entry)">重命名</el-button>
              <el-dropdown
                trigger="click"
                @command="(c: string) => onRowCmd(c, row as Entry)"
              >
                <el-button text size="small">
                  更多<el-icon><ArrowDown /></el-icon>
                </el-button>
                <template #dropdown>
                  <el-dropdown-menu>
                    <el-dropdown-item command="share">分享</el-dropdown-item>
                    <el-dropdown-item command="move">移动到 ...</el-dropdown-item>
                    <el-dropdown-item command="copy">复制到 ...</el-dropdown-item>
                    <el-dropdown-item command="properties">属性</el-dropdown-item>
                    <el-dropdown-item command="delete" divided>删除(进回收站)</el-dropdown-item>
                  </el-dropdown-menu>
                </template>
              </el-dropdown>
            </template>
          </el-table-column>
          <template #empty>
            <el-empty :description="query ? '没有匹配的文件' : '空目录,拖拽文件到此处上传'" />
          </template>
        </el-table>

        <div
          v-else
          v-loading="loading"
          class="grid"
          :class="{ photo: viewMode === 'photo' || viewMode === 'timeline' }"
        >
          <template v-for="cell in gridCells" :key="isHead(cell) ? 'h:' + cell.head : cell.name">
            <div v-if="isHead(cell)" class="tl-head">
              {{ cell.head }}<span class="tl-count">{{ cell.count }} 项</span>
            </div>
            <div
              v-else
              class="tile"
              :class="{
                sel: isSelected(cell),
                img: !cell.is_dir && (isImage(cell.name) || isVideo(cell.name)),
              }"
              @click="openEntry(cell)"
              @contextmenu.prevent.stop="openCtxMenu($event, cell)"
            >
              <span class="tile-check" @click.stop="toggleSelect(cell)">
                <el-checkbox :model-value="isSelected(cell)" />
              </span>
              <span class="tile-more" @click.stop>
                <el-dropdown trigger="click" @command="(c: string) => onTileCmd(c, cell)">
                  <el-button circle size="small" :icon="MoreFilled" />
                  <template #dropdown>
                    <el-dropdown-menu>
                      <el-dropdown-item v-if="!cell.is_dir" command="download">下载</el-dropdown-item>
                      <el-dropdown-item command="share">分享</el-dropdown-item>
                      <el-dropdown-item command="rename">重命名</el-dropdown-item>
                      <el-dropdown-item command="move">移动到 ...</el-dropdown-item>
                      <el-dropdown-item command="copy">复制到 ...</el-dropdown-item>
                      <el-dropdown-item command="properties">属性</el-dropdown-item>
                      <el-dropdown-item command="delete" divided>删除(进回收站)</el-dropdown-item>
                    </el-dropdown-menu>
                  </template>
                </el-dropdown>
              </span>
              <div class="tile-media">
                <el-image
                  v-if="!cell.is_dir && isImage(cell.name)"
                  :src="fileUrl(cell)"
                  fit="cover"
                  lazy
                >
                  <template #placeholder>
                    <div class="thumb-fallback"><el-icon color="#10b981"><Picture /></el-icon></div>
                  </template>
                  <template #error>
                    <div class="thumb-fallback"><el-icon color="#10b981"><Picture /></el-icon></div>
                  </template>
                </el-image>
                <VideoThumb
                  v-else-if="!cell.is_dir && isVideo(cell.name)"
                  :src="thumbnailUrl(cell)"
                />
                <el-icon v-else-if="cell.is_dir" color="#f6a821"><Folder /></el-icon>
                <el-icon v-else :color="iconOf(cell.name).color">
                  <component :is="iconOf(cell.name).icon" />
                </el-icon>
              </div>
              <div class="tile-name" :title="cell.name">{{ cell.name }}</div>
              <div class="tile-meta">{{ cell.is_dir ? '文件夹' : fmtSize(cell.size) }}</div>
            </div>
          </template>
          <el-empty
            v-if="!loading && !gridCells.length"
            class="grid-empty"
            :description="
              viewMode === 'timeline'
                ? '本目录没有文件'
                : query
                  ? '没有匹配的文件'
                  : '空目录,拖拽文件到此处上传'
            "
          />
        </div>
      </div>
    </div>

    <input ref="fileInput" type="file" multiple hidden @change="onPick" />
    <input ref="dirInput" type="file" webkitdirectory hidden @change="onDirPick" />

    <el-image-viewer
      v-if="viewerShow"
      :url-list="imageUrls"
      :initial-index="viewerIndex"
      hide-on-click-modal
      teleported
      @close="viewerShow = false"
    />

    <UploadDrawer ref="drawer" @uploaded="refresh" />
    <PreviewDialog ref="preview" />
    <DirPicker ref="dirPicker" />
    <ShareDialog ref="shareDlg" />
    <ShareManager ref="shareMgr" />
    <TrashPanel ref="trashPanel" @changed="onTrashChanged" />

    <el-dialog
      v-model="propertyVisible"
      :title="`属性：${propertyDisplayName}`"
      width="600px"
      append-to-body
    >
      <div v-loading="propertyLoading" class="property-panel">
        <el-skeleton v-if="propertyLoading" :rows="7" animated />
        <template v-else-if="propertyData">
          <div class="property-head">
            <el-icon v-if="propertyData.is_dir" class="property-icon" color="#f6a821">
              <Folder />
            </el-icon>
            <el-icon v-else class="property-icon" :color="iconOf(propertyData.name).color">
              <component :is="iconOf(propertyData.name).icon" />
            </el-icon>
            <div class="property-heading">
              <strong>{{ propertyDisplayName }}</strong>
              <span>{{ propertyType }}</span>
            </div>
          </div>
          <el-descriptions :column="1" border>
            <el-descriptions-item label="路径">
              <span class="property-value">{{ propertyData.path }}</span>
            </el-descriptions-item>
            <el-descriptions-item label="大小">
              {{ fmtSize(propertyData.size) }}（{{ fmtBytes(propertyData.size) }} 字节）
            </el-descriptions-item>
            <el-descriptions-item v-if="propertyData.is_dir" label="包含">
              {{ propertyData.file_count }} 个文件，{{ propertyData.dir_count }} 个文件夹
            </el-descriptions-item>
            <el-descriptions-item label="存储分块">
              {{ propertyData.chunk_count }} 个 Telegram 分块记录
            </el-descriptions-item>
            <el-descriptions-item label="创建时间">
              {{ fmtTime(propertyData.created_at) }}
            </el-descriptions-item>
            <el-descriptions-item label="修改时间">
              {{ fmtTime(propertyData.modified_at) }}
            </el-descriptions-item>
            <el-descriptions-item v-if="!propertyData.is_dir" label="SHA-256">
              <span class="property-value property-hash">
                {{ propertyData.sha256 || '未记录（历史文件）' }}
              </span>
            </el-descriptions-item>
          </el-descriptions>
        </template>
      </div>
      <template #footer>
        <el-button type="primary" @click="propertyVisible = false">确定</el-button>
      </template>
    </el-dialog>

    <!-- 自定义右键菜单 -->
    <teleport to="body">
      <div
        v-if="ctx.show"
        class="ctx-menu"
        :style="{ left: ctx.x + 'px', top: ctx.y + 'px' }"
        @click.stop
        @contextmenu.prevent
      >
        <template v-for="(it, i) in ctxItems" :key="i">
          <div v-if="it.divided" class="ctx-sep" />
          <div class="ctx-item" :class="{ danger: it.danger }" @click="runCtx(it)">
            {{ it.label }}
          </div>
        </template>
      </div>
    </teleport>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage, ElMessageBox, ElTree, type LoadFunction, type TableInstance } from 'element-plus'
import {
  ArrowDown, Box, Calendar, Delete, DeleteFilled, Document, Film, Finished, Folder, FolderAdd,
  FolderOpened, Grid, Headset, List, Moon, MoreFilled, Picture, Position, Promotion, Rank,
  Refresh, Search, Setting, Share, Sort, Sunny, SwitchButton, Tickets, TrendCharts, Upload,
} from '@element-plus/icons-vue'
import {
  api,
  errMsg,
  fmtSize,
  fmtTime,
  type Entry,
  type EntryProperties,
  type SearchHit,
  type ServerInfo,
} from '../api'
import { isDark, toggleDark } from '../theme'
import UploadDrawer, { type UploadFile } from '../components/UploadDrawer.vue'
import PreviewDialog from '../components/PreviewDialog.vue'
import VideoThumb from '../components/VideoThumb.vue'
import DirPicker from '../components/DirPicker.vue'
import ShareDialog from '../components/ShareDialog.vue'
import ShareManager from '../components/ShareManager.vue'
import TrashPanel from '../components/TrashPanel.vue'

const route = useRoute()
const router = useRouter()
const entries = ref<Entry[]>([])
const selected = ref<Entry[]>([])
const loading = ref(false)
const info = ref<ServerInfo | null>(null)
const fileInput = ref<HTMLInputElement>()
const dirInput = ref<HTMLInputElement>()
const drawer = ref<InstanceType<typeof UploadDrawer>>()
const preview = ref<InstanceType<typeof PreviewDialog>>()
const dirPicker = ref<InstanceType<typeof DirPicker>>()
const shareDlg = ref<InstanceType<typeof ShareDialog>>()
const shareMgr = ref<InstanceType<typeof ShareManager>>()
const trashPanel = ref<InstanceType<typeof TrashPanel>>()
const tableRef = ref<TableInstance>()
const propertyVisible = ref(false)
const propertyLoading = ref(false)
const propertyData = ref<EntryProperties | null>(null)
const propertyFallbackName = ref('')

const propertyDisplayName = computed(
  () => propertyData.value?.name || propertyFallbackName.value || '根目录',
)
const propertyType = computed(() => {
  const item = propertyData.value
  if (!item) return ''
  if (item.is_dir) return '文件夹'
  const ext = extOf(item.name)
  return ext ? `${ext.toUpperCase()} 文件` : '文件'
})

function fmtBytes(size: number) {
  return new Intl.NumberFormat('zh-CN').format(size)
}

const IMG_EXT = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'avif', 'ico']
/** 浏览器 <video> 大概率能解码的容器(mkv 在 Chrome/Edge 可播 H.264/VP9 轨) */
const VIDEO_EXT = ['mp4', 'webm', 'mov', 'm4v', 'mkv', 'ogv']
function extOf(name: string) {
  return name.split('.').pop()?.toLowerCase() ?? ''
}
function isImage(name: string) {
  return IMG_EXT.includes(extOf(name))
}
function isVideo(name: string) {
  return VIDEO_EXT.includes(extOf(name))
}

/** 按扩展名给非图片文件配图标与颜色 */
const ICON_RULES: [string[], typeof Document, string][] = [
  [['mp4', 'webm', 'mov', 'm4v', 'mkv', 'avi', 'ogv', 'ts', 'flv'], Film, '#8b5cf6'],
  [['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus'], Headset, '#f59e0b'],
  [['zip', 'rar', '7z', 'tar', 'gz', 'xz', 'zst', 'bz2', 'iso'], Box, '#b45309'],
  [['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx'], Tickets, '#ef4444'],
]
function iconOf(name: string) {
  const ext = extOf(name)
  for (const [list, icon, color] of ICON_RULES) if (list.includes(ext)) return { icon, color }
  return { icon: Document, color: '#94a3b8' }
}

function fileUrl(row: Entry) {
  return api.downloadUrl(joinPath(cur.value, row.name), true)
}

function thumbnailUrl(row: Entry) {
  return api.thumbnailUrl(joinPath(cur.value, row.name))
}

// ---- 布局:列表 / 网格 / 大图 / 时间线,记住选择 ----
type ViewMode = 'list' | 'grid' | 'photo' | 'timeline'
const viewMode = ref<ViewMode>(
  (['grid', 'photo', 'timeline'].includes(localStorage.getItem('tgdrive.view') ?? '')
    ? localStorage.getItem('tgdrive.view')
    : 'list') as ViewMode,
)
watch(viewMode, (v) => {
  localStorage.setItem('tgdrive.view', v)
  selected.value = []
})

// ---- 搜索:输入即过滤当前目录,回车全局搜索 ----
const query = ref('')
const searchMode = ref(false)
const searchHits = ref<SearchHit[]>([])
const lastQuery = ref('')
const filtered = computed(() => {
  const q = query.value.trim().toLowerCase()
  return q ? entries.value.filter((e) => e.name.toLowerCase().includes(q)) : entries.value
})
const countText = computed(() => {
  if (searchMode.value) return `全局 ${searchHits.value.length} 个结果`
  return query.value.trim()
    ? `匹配 ${filtered.value.length} / ${entries.value.length} 项`
    : `${entries.value.length} 项`
})

async function doGlobalSearch() {
  const q = query.value.trim()
  if (!q) return
  try {
    const r = await api.search(q)
    searchHits.value = r.data.results
    lastQuery.value = q
    searchMode.value = true
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}
function exitSearch() {
  searchMode.value = false
  searchHits.value = []
}
watch(query, (v) => {
  if (!v.trim()) exitSearch()
})
function parentOf(p: string) {
  const i = p.lastIndexOf('/')
  return i <= 0 ? '/' : p.slice(0, i)
}
function openHit(hit: SearchHit) {
  if (hit.is_dir) goto(hit.path)
  else goto(parentOf(hit.path))
}

// ---- 排序:目录始终在前,文件按所选键排 ----
const SORTS = [
  { key: 'name', label: '按名称' },
  { key: 'size', label: '按大小' },
  { key: 'mtime', label: '按修改时间' },
] as const
type SortKey = (typeof SORTS)[number]['key']
const sortKey = ref<SortKey>(
  (SORTS.some((s) => s.key === localStorage.getItem('tgdrive.sort'))
    ? localStorage.getItem('tgdrive.sort')
    : 'name') as SortKey,
)
const sortAsc = ref(localStorage.getItem('tgdrive.sortAsc') !== '0')
function onSortCmd(cmd: string) {
  if (cmd === 'asc') sortAsc.value = true
  else if (cmd === 'desc') sortAsc.value = false
  else sortKey.value = cmd as SortKey
  localStorage.setItem('tgdrive.sort', sortKey.value)
  localStorage.setItem('tgdrive.sortAsc', sortAsc.value ? '1' : '0')
}
const displayEntries = computed(() => {
  const dir = sortAsc.value ? 1 : -1
  return [...filtered.value].sort((a, b) => {
    if (a.is_dir !== b.is_dir) return Number(b.is_dir) - Number(a.is_dir)
    let d = 0
    if (sortKey.value === 'size') d = a.size - b.size
    else if (sortKey.value === 'mtime') d = a.modified_at - b.modified_at
    if (d === 0) d = a.name.localeCompare(b.name, 'zh')
    return d * dir
  })
})

// ---- 时间线:文件按修改日期分组(新的在前),组头与磁贴同渲染在网格里 ----
type GridCell = Entry | { head: string; count: number }
function isHead(c: GridCell): c is { head: string; count: number } {
  return 'head' in c
}
const gridCells = computed<GridCell[]>(() => {
  if (viewMode.value !== 'timeline') return displayEntries.value
  const dateLabel = (ts: number) =>
    new Date(ts * 1000).toLocaleDateString('zh-CN', {
      year: 'numeric',
      month: 'long',
      day: 'numeric',
    })
  const files = filtered.value
    .filter((e) => !e.is_dir)
    .sort((a, b) => b.modified_at - a.modified_at)
  const out: GridCell[] = []
  let head: { head: string; count: number } | null = null
  for (const e of files) {
    const label = dateLabel(e.modified_at)
    if (!head || head.head !== label) {
      head = { head: label, count: 0 }
      out.push(head)
    }
    head.count++
    out.push(e)
  }
  return out
})

function isSelected(row: Entry) {
  return selected.value.some((e) => e.name === row.name)
}
function toggleSelect(row: Entry) {
  if (isSelected(row)) selected.value = selected.value.filter((e) => e.name !== row.name)
  else selected.value = [...selected.value, row]
}
function onTileCmd(cmd: string, row: Entry) {
  if (cmd === 'download') download(row)
  else if (cmd === 'rename') rename(row)
  else onRowCmd(cmd, row)
}

// ---- 全选(工具栏按钮 / Ctrl+A):时间线只有文件,其余模式含文件夹 ----
function selectableEntries(): Entry[] {
  return viewMode.value === 'timeline'
    ? filtered.value.filter((e) => !e.is_dir)
    : displayEntries.value
}
const allSelected = computed(() => {
  const all = selectableEntries()
  return all.length > 0 && selected.value.length >= all.length
})
function setAllSelected(on: boolean) {
  if (viewMode.value === 'list') {
    // 列表模式经表格 API 走,保证复选框状态同步(selection-change 会回写 selected)
    tableRef.value?.clearSelection()
    if (on) selectableEntries().forEach((r) => tableRef.value?.toggleRowSelection(r, true))
  } else {
    selected.value = on ? selectableEntries() : []
  }
}
function toggleSelectAll() {
  setAllSelected(!allSelected.value)
}

// ---- 右键菜单 ----
interface CtxItem {
  label: string
  action: () => void
  danger?: boolean
  /** 在该项上方画分隔线 */
  divided?: boolean
}
const ctx = ref<{ show: boolean; x: number; y: number; row: Entry | null }>({
  show: false,
  x: 0,
  y: 0,
  row: null,
})

function openCtxMenu(ev: MouseEvent, row: Entry | null) {
  ctx.value = {
    show: true,
    // 贴边时向内收,避免菜单溢出视口
    x: Math.min(ev.clientX, window.innerWidth - 200),
    y: Math.min(ev.clientY, window.innerHeight - 340),
    row,
  }
}
function onRowCtx(row: Entry, _col: unknown, ev: Event) {
  ev.preventDefault()
  ev.stopPropagation()
  openCtxMenu(ev as MouseEvent, row)
}
function onBlankCtx(ev: MouseEvent) {
  if (searchMode.value) return // 搜索结果里用浏览器原生菜单
  ev.preventDefault()
  openCtxMenu(ev, null)
}
function closeCtx() {
  ctx.value.show = false
}

const ctxItems = computed<CtxItem[]>(() => {
  const row = ctx.value.row
  if (!row) {
    return [
      { label: '上传文件', action: () => fileInput.value?.click() },
      { label: '上传文件夹', action: () => dirInput.value?.click() },
      { label: '新建文件夹', action: mkdir },
      { label: '刷新', action: refresh },
      { label: allSelected.value ? '取消全选' : '全选', action: toggleSelectAll, divided: true },
      {
        label: '当前文件夹属性',
        action: () =>
          void openPropertiesPath(
            cur.value,
            segments.value[segments.value.length - 1] || '根目录',
          ),
        divided: true,
      },
    ]
  }
  const items: CtxItem[] = []
  // 右键落在多选集合里 → 先给批量操作
  if (selected.value.length > 1 && isSelected(row)) {
    items.push({ label: `移动所选 ${selected.value.length} 项到 ...`, action: moveSelected })
    items.push({
      label: `删除所选 ${selected.value.length} 项`,
      action: removeSelected,
      danger: true,
    })
  }
  items.push({
    label: row.is_dir ? '打开' : '打开 / 预览',
    action: () => openEntry(row),
    divided: items.length > 0,
  })
  if (!row.is_dir) items.push({ label: '下载', action: () => download(row) })
  items.push({ label: '分享', action: () => onRowCmd('share', row) })
  items.push({ label: '重命名', action: () => rename(row) })
  items.push({ label: '移动到 ...', action: () => onRowCmd('move', row) })
  items.push({ label: '复制到 ...', action: () => onRowCmd('copy', row) })
  items.push({ label: '属性', action: () => void openProperties(row) })
  items.push({
    label: '删除(进回收站)',
    action: () => onRowCmd('delete', row),
    danger: true,
    divided: true,
  })
  return items
})

function runCtx(it: CtxItem) {
  closeCtx()
  it.action()
}

function onGlobalKey(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    closeCtx()
    return
  }
  const tag = (e.target as HTMLElement)?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA') return
  if ((e.ctrlKey || e.metaKey) && (e.key === 'a' || e.key === 'A')) {
    e.preventDefault()
    setAllSelected(true)
  }
}
onMounted(() => {
  document.addEventListener('click', closeCtx)
  document.addEventListener('keydown', onGlobalKey)
  window.addEventListener('scroll', closeCtx, true)
})
onUnmounted(() => {
  document.removeEventListener('click', closeCtx)
  document.removeEventListener('keydown', onGlobalKey)
  window.removeEventListener('scroll', closeCtx, true)
})

// ---- 图片浏览器:目录内所有图片可左右翻页/缩放 ----
const viewerShow = ref(false)
const viewerIndex = ref(0)
const images = computed(() => displayEntries.value.filter((e) => !e.is_dir && isImage(e.name)))
const imageUrls = computed(() => images.value.map((e) => fileUrl(e)))

const segments = computed(() => {
  const p = route.params.path
  const arr = Array.isArray(p) ? p : p ? [p] : []
  return arr.filter((s) => s !== '')
})
const cur = computed(() => '/' + segments.value.join('/'))
const davUrl = computed(() => `${location.origin}/dav`)
const tgTip = computed(() => {
  if (!info.value) return 'Telegram 存储'
  if (!info.value.configured) return 'Telegram 存储未配置,点击去设置'
  return `Telegram 存储正常 · ${info.value.official_api ? '官方' : '自建'} Bot API`
})

function joinPath(dir: string, name: string) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`
}

let propertyRequestSeq = 0
async function openPropertiesPath(path: string, fallbackName: string) {
  const request = ++propertyRequestSeq
  propertyFallbackName.value = fallbackName
  propertyData.value = null
  propertyLoading.value = true
  propertyVisible.value = true
  try {
    const response = await api.properties(path)
    if (request === propertyRequestSeq) propertyData.value = response.data.properties
  } catch (e) {
    if (request === propertyRequestSeq) {
      propertyVisible.value = false
      ElMessage.error(`读取属性失败：${errMsg(e)}`)
    }
  } finally {
    if (request === propertyRequestSeq) propertyLoading.value = false
  }
}

function openProperties(row: Entry) {
  return openPropertiesPath(joinPath(cur.value, row.name), row.name)
}

function pathOf(i: number) {
  return '/' + segments.value.slice(0, i + 1).join('/')
}
function goto(p: string) {
  router.push('/files' + p.split('/').map(encodeURIComponent).join('/'))
}

// ---- 左侧目录树:懒加载,只列目录 ----
interface DirNode {
  name: string
  path: string
}
const treeRef = ref<InstanceType<typeof ElTree>>()
const treeKey = ref(0)
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

watch(cur, () => {
  query.value = ''
  exitSearch()
  try {
    treeRef.value?.setCurrentKey(cur.value)
  } catch {
    /* 节点尚未懒加载出来时忽略 */
  }
})

async function refresh() {
  loading.value = true
  selected.value = []
  try {
    const r = await api.list(cur.value)
    entries.value = r.data.entries
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally {
    loading.value = false
  }
  api.info().then((r) => (info.value = r.data)).catch(() => {})
}

watch(cur, refresh)
onMounted(refresh)

function onTrashChanged() {
  treeKey.value++
  refresh()
}

function openEntry(row: Entry) {
  if (row.is_dir) {
    goto(joinPath(cur.value, row.name))
  } else if (isImage(row.name)) {
    viewerIndex.value = Math.max(0, images.value.findIndex((e) => e.name === row.name))
    viewerShow.value = true
  } else if (isVideo(row.name)) {
    // 播放器带上同目录其余视频,可上一个/下一个切换
    const list = displayEntries.value
      .filter((e) => !e.is_dir && isVideo(e.name))
      .map((e) => joinPath(cur.value, e.name))
    preview.value?.open(joinPath(cur.value, row.name), list)
  } else {
    preview.value?.open(joinPath(cur.value, row.name))
  }
}

function download(row: Entry) {
  window.open(api.downloadUrl(joinPath(cur.value, row.name)), '_blank')
}

async function mkdir() {
  try {
    const { value } = await ElMessageBox.prompt('输入文件夹名称', '新建文件夹', {
      inputPattern: /^[^/\\]+$/,
      inputErrorMessage: '名称不能为空或包含斜杠',
    })
    await api.mkdir(joinPath(cur.value, value.trim()))
    treeKey.value++
    refresh()
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

async function rename(row: Entry) {
  try {
    const { value } = await ElMessageBox.prompt('输入新名称', `重命名 ${row.name}`, {
      inputValue: row.name,
      inputPattern: /^[^/\\]+$/,
      inputErrorMessage: '名称不能为空或包含斜杠',
    })
    await api.rename(joinPath(cur.value, row.name), joinPath(cur.value, value.trim()))
    if (row.is_dir) treeKey.value++
    refresh()
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

/** "a.txt" → "a (副本).txt" */
function copyName(n: string) {
  const i = n.lastIndexOf('.')
  return i > 0 ? `${n.slice(0, i)} (副本)${n.slice(i)}` : `${n} (副本)`
}

async function onRowCmd(cmd: string, row: Entry) {
  const src = joinPath(cur.value, row.name)
  if (cmd === 'properties') {
    await openProperties(row)
    return
  }
  try {
    if (cmd === 'share') {
      shareDlg.value?.open(src, row.name)
    } else if (cmd === 'move') {
      const target = await dirPicker.value!.open(`移动「${row.name}」到`, cur.value)
      if (target === null || target === cur.value) return
      await api.rename(src, joinPath(target, row.name))
      if (row.is_dir) treeKey.value++
      ElMessage.success(`已移动到 ${target}`)
      refresh()
    } else if (cmd === 'copy') {
      const target = await dirPicker.value!.open(`复制「${row.name}」到`, cur.value)
      if (target === null) return
      const { value } = await ElMessageBox.prompt('目标文件名', '复制为', {
        inputValue: target === cur.value ? copyName(row.name) : row.name,
        inputPattern: /^[^/\\]+$/,
        inputErrorMessage: '名称不能为空或包含斜杠',
      })
      await api.copy(src, joinPath(target, value.trim()))
      ElMessage.success('复制完成(元数据级,不占额外流量)')
      if (row.is_dir) treeKey.value++
      refresh()
    } else if (cmd === 'delete') {
      await confirmDelete([row])
    }
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

async function moveSelected() {
  const rows = [...selected.value]
  if (!rows.length) return
  try {
    const target = await dirPicker.value!.open(`移动 ${rows.length} 项到`, cur.value)
    if (target === null || target === cur.value) return
    let fail = 0
    for (const r of rows) {
      try {
        await api.rename(joinPath(cur.value, r.name), joinPath(target, r.name))
      } catch {
        fail++
      }
    }
    if (fail) ElMessage.warning(`${fail} 项移动失败(目标可能已有同名)`)
    else ElMessage.success(`已移动到 ${target}`)
    if (rows.some((r) => r.is_dir)) treeKey.value++
    refresh()
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}

async function confirmDelete(rows: Entry[]) {
  await ElMessageBox.confirm(
    `把 ${rows.length} 项移入回收站?可随时恢复;超过保留期后自动彻底删除。`,
    '删除确认',
    { type: 'warning' },
  )
  const r = await api.del(rows.map((x) => joinPath(cur.value, x.name)))
  if (r.data.failed.length > 0) {
    ElMessage.warning(`有 ${r.data.failed.length} 项删除失败:${r.data.failed[0].error}`)
  } else {
    ElMessage.success('已移入回收站')
  }
  if (rows.some((x) => x.is_dir)) treeKey.value++
  refresh()
}

async function removeSelected() {
  try {
    await confirmDelete(selected.value)
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

// ---- 上传入口:选文件 / 选文件夹 / 拖拽(含文件夹) / 粘贴 ----

function onPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  if (input.files?.length) {
    drawer.value?.enqueue(cur.value, Array.from(input.files).map((f) => ({ file: f })))
    input.value = ''
  }
}

function onDirPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  const files = Array.from(input.files ?? [])
  if (files.length) {
    const ups: UploadFile[] = files.map((f) => {
      const rel = (f as File & { webkitRelativePath?: string }).webkitRelativePath ?? ''
      return { file: f, rel: rel.includes('/') ? rel.slice(0, rel.lastIndexOf('/')) : '' }
    })
    drawer.value?.enqueue(cur.value, ups)
    input.value = ''
  }
}

async function walkEntry(entry: FileSystemEntry, rel: string, out: UploadFile[]) {
  if (entry.isFile) {
    const f = await new Promise<File>((res, rej) => (entry as FileSystemFileEntry).file(res, rej))
    out.push({ file: f, rel })
  } else if (entry.isDirectory) {
    const dirRel = rel ? `${rel}/${entry.name}` : entry.name
    const reader = (entry as FileSystemDirectoryEntry).createReader()
    for (;;) {
      // readEntries 单次最多返回 100 项,要循环读到空为止
      const batch = await new Promise<FileSystemEntry[]>((res, rej) =>
        reader.readEntries(res, rej),
      )
      if (!batch.length) break
      for (const e of batch) await walkEntry(e, dirRel, out)
    }
  }
}

async function onDrop(ev: DragEvent) {
  const items = ev.dataTransfer?.items
  // 优先走 entry 递归(支持拖入整个文件夹);await 前必须同步取完 entry
  if (items?.length && typeof items[0].webkitGetAsEntry === 'function') {
    const roots: FileSystemEntry[] = []
    for (const it of items) {
      const e = it.webkitGetAsEntry()
      if (e) roots.push(e)
    }
    if (roots.length) {
      const collected: UploadFile[] = []
      try {
        for (const r of roots) await walkEntry(r, '', collected)
      } catch {
        /* 个别条目读取失败就用已收集到的 */
      }
      if (collected.length) {
        drawer.value?.enqueue(cur.value, collected)
        return
      }
    }
  }
  const files = ev.dataTransfer?.files
  if (files?.length) {
    drawer.value?.enqueue(cur.value, Array.from(files).map((f) => ({ file: f })))
  }
}

function pasteStamp() {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`
}

function onPaste(e: ClipboardEvent) {
  const files = Array.from(e.clipboardData?.files ?? [])
  if (!files.length) return // 纯文本粘贴(输入框等)不拦截
  const stamp = pasteStamp()
  const ups: UploadFile[] = files.map((f, i) => {
    const ext = extOf(f.name) || 'png'
    const name = `粘贴_${stamp}${files.length > 1 ? `_${i + 1}` : ''}.${ext}`
    return { file: new File([f], name, { type: f.type }) }
  })
  drawer.value?.enqueue(cur.value, ups)
}
onMounted(() => document.addEventListener('paste', onPaste))
onUnmounted(() => document.removeEventListener('paste', onPaste))

async function logout() {
  await api.logout().catch(() => {})
  router.push('/login')
}
</script>

<style scoped>
.page {
  margin: 0 auto;
  padding: 16px 24px 40px;
  min-height: 100vh;
  box-sizing: border-box;
}
.topbar {
  position: sticky;
  top: 12px;
  z-index: 20;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 9px 14px;
  margin-bottom: 14px;
  border-radius: 16px;
  flex-wrap: wrap;
}
.brand {
  font-size: 20px;
  font-weight: 800;
  letter-spacing: 0.5px;
  flex-shrink: 0;
}
.search {
  width: 230px;
  flex-shrink: 1;
}
.search :deep(.el-input__wrapper) {
  border-radius: 999px;
}
.btns {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.lights {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 2px;
  flex-shrink: 0;
}
.vsep {
  width: 1px;
  height: 22px;
  background: var(--glass-border);
  flex-shrink: 0;
}
.spacer {
  flex: 1;
}
.setup-hint {
  margin-bottom: 12px;
}

/* 面包屑胶囊条 */
.crumb-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 16px;
  margin-bottom: 14px;
  border-radius: 12px;
}
.crumb-ico {
  color: var(--el-color-primary);
}
.crumbs {
  flex-shrink: 1;
  overflow: hidden;
}
.crumb-count {
  font-size: 12px;
  color: var(--text-soft);
  flex-shrink: 0;
}

/* 侧栏 + 主面板 */
.layout {
  display: flex;
  gap: 14px;
  align-items: flex-start;
}
.sidebar {
  width: 224px;
  flex-shrink: 0;
  border-radius: 16px;
  padding: 12px 10px;
  position: sticky;
  top: 76px;
  max-height: calc(100vh - 100px);
  overflow: auto;
  box-sizing: border-box;
}
.side-head {
  display: flex;
  align-items: center;
  gap: 6px;
  font-weight: 700;
  font-size: 14px;
  padding: 0 6px 8px;
  border-bottom: 1px solid var(--glass-border);
  margin-bottom: 8px;
}
.side-root {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 7px 8px;
  border-radius: 8px;
  font-size: 14px;
  cursor: pointer;
  margin-bottom: 2px;
}
.side-root:hover {
  background: var(--tile-bg-hover);
}
.side-root.on {
  color: var(--el-color-primary);
  background: rgba(64, 158, 255, 0.12);
  font-weight: 600;
}
.side-count {
  font-size: 12px;
  color: var(--text-soft);
  background: var(--tile-bg);
  border-radius: 999px;
  padding: 1px 8px;
}
.side-sep {
  height: 1px;
  background: var(--glass-border);
  margin: 8px 4px;
}
.side-tree {
  font-size: 14px;
}
.tree-node {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}
.tree-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.panel {
  flex: 1;
  min-width: 0;
  border-radius: 16px;
  padding: 12px 14px;
}

/* 全局搜索结果 */
.search-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 2px 4px 10px;
  font-size: 14px;
}
.hit-path {
  color: var(--text-soft);
  font-size: 13px;
}
.hit-path:hover {
  color: var(--el-color-primary);
}

.table {
  width: 100%;
  border-radius: 10px;
  overflow: hidden;
}
.fico {
  font-size: 18px;
}
.thumb {
  width: 36px;
  height: 36px;
  border-radius: 8px;
  flex-shrink: 0;
  box-shadow: 0 1px 4px rgba(30, 40, 80, 0.18);
  background: var(--tile-media-bg);
}
.thumb-fallback {
  width: 100%;
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 18px;
}
/* 列表视图里的视频小缩略图:压缩角标与回退图标 */
.thumb:deep(.vthumb-fallback) {
  font-size: 18px;
}
.thumb:deep(.vthumb-play) {
  font-size: 15px;
}
/* 网格磁贴里的视频缩略图回退图标与普通图标同尺寸 */
.tile-media .vthumb {
  width: 100%;
  height: 100%;
}
.tile-media :deep(.vthumb-fallback) {
  font-size: 52px;
}
.grid.photo .tile-media :deep(.vthumb-fallback) {
  font-size: 76px;
}

/* 网格:K-Vault 式相框卡片 */
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(148px, 1fr));
  gap: 12px;
  padding: 4px 2px 10px;
  min-height: 120px;
}
.grid.photo {
  grid-template-columns: repeat(auto-fill, minmax(230px, 1fr));
  gap: 16px;
}
.grid.photo .tile-media {
  height: 200px;
}
.grid.photo .tile-media > .el-icon {
  font-size: 76px;
}
/* 时间线日期组头:横跨整行 */
.tl-head {
  grid-column: 1 / -1;
  font-size: 15px;
  font-weight: 700;
  padding: 10px 4px 0;
  display: flex;
  align-items: baseline;
  gap: 8px;
}
.tl-count {
  font-size: 12px;
  font-weight: 400;
  color: var(--text-soft);
}
/* 大图模式的图片磁贴:白色相框,名字条常显覆盖在图片底部 */
.grid.photo .tile.img {
  padding: 6px;
}
.grid.photo .tile.img .tile-media {
  height: 216px;
  margin: 0;
  border-radius: 9px;
}
.grid.photo .tile.img .tile-meta {
  display: none;
}
.grid.photo .tile.img .tile-name {
  position: absolute;
  left: 6px;
  right: 6px;
  bottom: 6px;
  z-index: 1;
  margin: 0;
  padding: 7px 10px;
  color: #fff;
  font-size: 12px;
  text-align: center;
  background: rgba(20, 24, 36, 0.55);
  backdrop-filter: blur(4px);
  -webkit-backdrop-filter: blur(4px);
  border-radius: 0 0 9px 9px;
  pointer-events: none;
}
/* 大图模式复选框常显(参考 K-Vault) */
.grid.photo .tile-check {
  opacity: 1;
}
.grid-empty {
  grid-column: 1 / -1;
}
.tile {
  position: relative;
  border-radius: 14px;
  padding: 10px 10px 8px;
  cursor: pointer;
  background: var(--tile-bg);
  border: 1px solid var(--tile-border);
  transition: background 0.15s, box-shadow 0.15s, border-color 0.15s, transform 0.15s;
}
.tile:hover {
  background: var(--tile-bg-hover);
  box-shadow: 0 6px 20px rgba(80, 90, 140, 0.18);
  transform: translateY(-2px);
}
.tile.sel {
  border-color: var(--el-color-primary);
  background: rgba(64, 158, 255, 0.14);
}
.tile-media {
  height: 100px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 10px;
  overflow: hidden;
  margin-bottom: 8px;
  background: var(--tile-media-bg);
}
.tile-media .el-image {
  width: 100%;
  height: 100%;
}
.tile-media > .el-icon {
  font-size: 52px;
}
.tile-name {
  font-size: 13px;
  text-align: center;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.tile-meta {
  font-size: 11px;
  color: var(--text-soft);
  text-align: center;
  margin-top: 2px;
}
/* 复选框在右上、更多按钮在左上(对齐 K-Vault 的角标布局) */
.tile-check,
.tile-more {
  position: absolute;
  top: 8px;
  z-index: 2;
  opacity: 0;
  transition: opacity 0.15s;
}
.tile-check {
  right: 10px;
  background: rgba(255, 255, 255, 0.82);
  border-radius: 6px;
  padding: 0 5px;
  line-height: 1;
  box-shadow: 0 1px 4px rgba(30, 40, 80, 0.2);
}
html.dark .tile-check {
  background: rgba(30, 36, 54, 0.85);
}
.tile-more {
  left: 8px;
}
.tile:hover .tile-check,
.tile:hover .tile-more,
.tile.sel .tile-check {
  opacity: 1;
}

/* 窄屏:收起侧栏与次要徽章 */
@media (max-width: 960px) {
  .sidebar {
    display: none;
  }
  .hide-sm {
    display: none;
  }
}

.property-panel {
  min-height: 260px;
}
.property-head {
  display: flex;
  align-items: center;
  gap: 14px;
  margin-bottom: 18px;
  padding: 4px 2px;
}
.property-icon {
  flex-shrink: 0;
  font-size: 44px;
}
.property-heading {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.property-heading strong {
  overflow-wrap: anywhere;
  font-size: 17px;
}
.property-heading span {
  color: var(--text-soft);
  font-size: 13px;
}
.property-value {
  user-select: text;
  overflow-wrap: anywhere;
}
.property-hash {
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-size: 12px;
}

/* 自定义右键菜单(teleport 到 body,fixed 定位) */
.ctx-menu {
  position: fixed;
  z-index: 3000;
  min-width: 168px;
  padding: 5px;
  border-radius: 10px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-light);
  box-shadow: var(--el-box-shadow-light);
  user-select: none;
}
.ctx-item {
  padding: 7px 14px;
  font-size: 13px;
  border-radius: 6px;
  cursor: pointer;
  white-space: nowrap;
}
.ctx-item:hover {
  background: var(--el-fill-color);
}
.ctx-item.danger {
  color: var(--el-color-danger);
}
.ctx-item.danger:hover {
  background: var(--el-color-danger-light-9);
}
.ctx-sep {
  height: 1px;
  margin: 4px 8px;
  background: var(--el-border-color-lighter);
}
</style>
