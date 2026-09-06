<template>
  <div
    class="page"
    @dragenter.prevent="onDragEnter"
    @dragover.prevent
    @dragleave.prevent="onDragLeave"
    @drop.prevent="onDrop"
  >
    <div v-if="dragging" class="drop-overlay">
      <el-icon><UploadFilled /></el-icon>
      <strong>松开后分批上传文件夹</strong>
      <span>文件会惰性读取，不会一次性塞入浏览器内存</span>
    </div>
    <!-- 顶栏:Logo · 搜索 · 统计 · 操作按钮 · 状态灯 · 设置/退出 -->
    <div class="topbar glass">
      <div class="logo-badge"><el-icon><Promotion /></el-icon></div>
      <div class="brand brand-text">tgdrive</div>

      <el-input
        v-model="query"
        class="search"
        placeholder="过滤当前目录，回车全局搜索"
        :prefix-icon="Search"
        clearable
        @keyup.enter="doGlobalSearch"
      />
      <el-tooltip content="高级搜索">
        <el-button class="icon-btn" :icon="Filter" @click="advancedVisible = true" />
      </el-tooltip>

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
              <el-dropdown-item @click="pickDirectory">
                {{ isFirefoxBrowser ? '上传文件夹（请拖拽）' : '上传文件夹' }}
              </el-dropdown-item>
              <el-dropdown-item disabled>Ctrl+V 可直接粘贴截图</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
        <el-tooltip content="上传队列">
          <el-button class="icon-btn" :icon="UploadFilled" @click="drawer?.open()" />
        </el-tooltip>
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
          <el-tooltip content="删除所选">
            <el-button class="icon-btn" type="danger" :icon="Delete" @click="removeSelected" />
          </el-tooltip>
        </el-badge>

        <span class="vsep" />

        <el-dropdown trigger="click" :disabled="viewMode === 'timeline'" @command="onSortCmd">
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
            aria-label="列表视图"
            :class="{ active: viewMode === 'list' }"
            :icon="List"
            @click="viewMode = 'list'"
          />
        </el-tooltip>
        <el-tooltip content="网格视图">
          <el-button
            class="icon-btn"
            aria-label="网格视图"
            :class="{ active: viewMode === 'grid' }"
            :icon="Grid"
            @click="viewMode = 'grid'"
          />
        </el-tooltip>
        <el-tooltip content="大图视图">
          <el-button
            class="icon-btn"
            aria-label="大图视图"
            :class="{ active: viewMode === 'photo' }"
            :icon="Picture"
            @click="viewMode = 'photo'"
          />
        </el-tooltip>
        <el-tooltip content="时间线(按日期看图)">
          <el-button
            class="icon-btn"
            aria-label="时间线(按日期看图)"
            :class="{ active: viewMode === 'timeline' }"
            :icon="Calendar"
            @click="viewMode = 'timeline'"
          />
        </el-tooltip>

        <el-tooltip content="短视频模式(随机刷文件夹里的视频)">
          <el-button class="icon-btn" :icon="VideoPlay" @click="openShorts" />
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
        <VirtualDirectoryTree :key="treeKey" :current="cur" @select="goto" />
        <div class="side-sep" />
        <div class="side-root" :class="{ on: searchMode && searchView === 'favorites' }" @click="showFavorites">
          <el-icon color="#f59e0b"><StarFilled /></el-icon>
          <span>我的收藏</span>
        </div>
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
              {{ searchTitle }}：{{ searchHits.length }} 个结果
              <template v-if="searchHits.length >= searchLimit">（仅显示前 {{ searchLimit }} 条）</template>
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
            <el-table-column label="收藏" width="70" align="center">
              <template #default="{ row }">
                <el-button
                  text
                  circle
                  :icon="row.favorite ? StarFilled : Star"
                  :class="{ 'favorite-on': row.favorite }"
                  @click="toggleSearchFavorite(row as SearchHit)"
                />
              </template>
            </el-table-column>
            <template #empty><el-empty description="没有找到匹配的内容" /></template>
          </el-table>
        </div>

        <template v-else>
          <div v-if="uploadDirty" class="directory-notice">
            上传已更新目录，当前浏览和选择已保留。<el-button text @click="refresh">刷新查看</el-button>
          </div>
          <div v-if="batchProgress" class="directory-notice" role="status">{{ batchProgress }}</div>
          <DirectoryViewport :entries="entries" :count="entries.length" :mode="viewMode" :generation="generation"
            :revision="revision" :index-of="directory.indexOf" @near-end="loadNextBatch"
            @scroll="(offset) => scrollOffset = offset">
            <template #header>
              <div class="list-row list-heading">
                <input type="checkbox" aria-label="全选已加载项目" :checked="allSelected" @change="toggleSelectAll" />
                <button @click="headerSort('name')">名称 {{ sortIndicator('name') }}</button>
                <button @click="headerSort('size')">大小 {{ sortIndicator('size') }}</button>
                <button @click="headerSort('mtime')">修改时间 {{ sortIndicator('mtime') }}</button>
                <span>操作</span>
              </div>
            </template>
            <template #default="{ entry: row, mode, mediaKey, generation: mediaGeneration }">
              <div :class="mode === 'list' ? 'list-row' : ['tile', 'tile-' + mode, { sel: isSelected(row), img: !row.is_dir && (isImage(row.name) || isVideo(row.name)) }]"
                :data-media-generation="mediaGeneration" :data-media-key="mediaKey"
                @contextmenu.prevent.stop="openCtxMenu($event, row)">
                <input class="entry-check" type="checkbox" :aria-label="`选择 ${row.name}`"
                  :checked="isSelected(row)" @click.stop @change="toggleSelect(row)" />
                <div :class="mode === 'list' ? 'name-cell clickable' : 'tile-content'" @click="openEntry(row)">
                  <div :class="mode === 'list' ? 'thumb' : 'tile-media'">
                    <el-icon v-if="row.is_dir" color="#f6a821"><Folder /></el-icon>
                    <DirectoryMedia v-else-if="isImage(row.name) || isVideo(row.name)" :key="mediaKey"
                      :entry="row" :path="joinPath(cur, row.name)" :video="isVideo(row.name)" :generation="mediaGeneration" />
                    <el-icon v-else :color="iconOf(row.name).color"><component :is="iconOf(row.name).icon" /></el-icon>
                  </div>
                  <span :class="mode === 'list' ? 'entry-name' : 'tile-name'" :title="row.name">{{ row.name }}</span>
                  <el-icon v-if="row.favorite" class="favorite-mark"><StarFilled /></el-icon>
                </div>
                <span :class="mode === 'list' ? 'entry-size' : 'tile-meta'">{{ row.is_dir ? '文件夹' : fmtSize(row.size) }}</span>
                <span v-if="mode === 'list'" class="entry-time">{{ fmtTime(row.modified_at) }}</span>
                <div :class="mode === 'list' ? 'entry-actions' : 'tile-actions'" @click.stop>
                  <el-button v-if="mode === 'list'" text size="small" :class="{ 'favorite-on': row.favorite }" @click="toggleEntryFavorite(row)">{{ row.favorite ? '已收藏' : '收藏' }}</el-button>
                  <el-button v-if="mode === 'list' && !row.is_dir" text size="small" @click="download(row)">下载</el-button>
                  <el-button v-if="mode === 'list'" text size="small" @click="rename(row)">重命名</el-button>
                  <el-dropdown trigger="click" @command="(cmd: string) => onTileCmd(cmd, row)">
                    <el-button text size="small" :icon="MoreFilled" aria-label="更多操作" />
                    <template #dropdown><el-dropdown-menu>
                      <el-dropdown-item v-if="!row.is_dir" command="download">下载</el-dropdown-item>
                      <el-dropdown-item command="favorite">{{ row.favorite ? '取消收藏' : '收藏' }}</el-dropdown-item>
                      <el-dropdown-item command="share">分享</el-dropdown-item>
                      <el-dropdown-item command="rename">重命名</el-dropdown-item>
                      <el-dropdown-item command="move">移动到 ...</el-dropdown-item>
                      <el-dropdown-item command="copy">复制到 ...</el-dropdown-item>
                      <el-dropdown-item command="properties">属性</el-dropdown-item>
                      <el-dropdown-item command="delete" divided>删除…</el-dropdown-item>
                    </el-dropdown-menu></template>
                  </el-dropdown>
                </div>
              </div>
            </template>
            <template #footer>
              <div class="directory-status" role="status">
                <span v-if="loading">加载中…</span>
                <template v-else-if="directoryError">加载失败：{{ errMsg(directoryError) }} <el-button text @click="directory.loadMore(true)">重试</el-button></template>
                <span v-else-if="!entries.length">{{ query ? '没有匹配的文件' : '空目录，拖拽文件到此处上传' }}</span>
                <span v-else>{{ hasMore ? '继续向下滚动加载' : '已加载全部匹配项目' }}</span>
                <span v-if="viewMode === 'timeline'"> · 按修改时间倒序，仅组织已加载文件</span>
              </div>
            </template>
          </DirectoryViewport>
        </template>
      </div>
    </div>

    <input ref="fileInput" type="file" multiple hidden @change="onPick" />
    <input ref="dirInput" type="file" webkitdirectory hidden @change="onDirPick" />

    <BoundedImagePreview v-if="viewerShow" v-model:index="viewerIndex"
      :entries="displayEntries" :count="displayEntries.length" :base="cur" @close="viewerShow = false" />

    <UploadDrawer
      ref="drawer"
      @uploaded="scheduleUploadRefresh"
      @cancelled-all="cancelDirectoryScan"
    />
    <PreviewDialog ref="preview" />
    <DirPicker ref="dirPicker" />
    <ShareDialog ref="shareDlg" />
    <ShareManager ref="shareMgr" />
    <TrashPanel ref="trashPanel" @changed="onTrashChanged" />

    <el-dialog
      v-model="advancedVisible"
      title="高级搜索"
      width="640px"
      align-center
      append-to-body
    >
      <el-form label-width="92px" class="advanced-search-form">
        <el-form-item label="名称包含">
          <el-input v-model="advanced.keyword" clearable placeholder="可留空，只按下面条件筛选" />
        </el-form-item>
        <el-form-item label="对象类型">
          <el-segmented
            v-model="advanced.kind"
            :options="[
              { label: '全部', value: 'all' },
              { label: '文件', value: 'file' },
              { label: '文件夹', value: 'dir' },
            ]"
          />
        </el-form-item>
        <el-form-item v-if="advanced.kind !== 'dir'" label="文件类别">
          <el-select v-model="advanced.category" style="width: 100%">
            <el-option label="全部类别" value="all" />
            <el-option label="图片" value="image" />
            <el-option label="视频" value="video" />
            <el-option label="音频" value="audio" />
            <el-option label="文档" value="document" />
            <el-option label="压缩包" value="archive" />
            <el-option label="自定义后缀" value="custom" />
          </el-select>
        </el-form-item>
        <el-form-item
          v-if="advanced.kind !== 'dir' && advanced.category === 'custom'"
          label="文件后缀"
        >
          <el-input v-model="advanced.extensions" clearable placeholder="例如 pdf,docx,tar.gz" />
        </el-form-item>
        <el-form-item v-if="advanced.kind !== 'dir'" label="文件大小">
          <div class="size-range">
            <el-input-number
              v-model="advanced.minMb"
              :min="0"
              :precision="1"
              :step="10"
              controls-position="right"
              placeholder="最小"
            />
            <span>至</span>
            <el-input-number
              v-model="advanced.maxMb"
              :min="0"
              :precision="1"
              :step="10"
              controls-position="right"
              placeholder="最大"
            />
            <span>MB</span>
          </div>
        </el-form-item>
        <el-form-item label="修改时间">
          <el-date-picker
            v-model="advanced.modifiedRange"
            type="datetimerange"
            start-placeholder="开始时间"
            end-placeholder="结束时间"
            range-separator="至"
            style="width: 100%"
          />
        </el-form-item>
        <el-form-item label="仅看收藏">
          <el-switch v-model="advanced.favorite" />
        </el-form-item>
        <el-form-item label="结果排序">
          <div class="sort-range">
            <el-select v-model="advanced.sort">
              <el-option label="名称" value="name" />
              <el-option label="大小" value="size" />
              <el-option label="修改时间" value="modified" />
              <el-option label="创建时间" value="created" />
            </el-select>
            <el-select v-model="advanced.order">
              <el-option label="升序" value="asc" />
              <el-option label="降序" value="desc" />
            </el-select>
          </div>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="resetAdvanced">重置</el-button>
        <el-button type="primary" :loading="advancedLoading" @click="runAdvancedSearch">
          搜索
        </el-button>
      </template>
    </el-dialog>

    <el-dialog
      v-model="propertyVisible"
      :title="`属性：${propertyDisplayName}`"
      width="600px"
      align-center
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
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import {
  Box, Calendar, CopyDocument, Delete, DeleteFilled, Document, EditPen, Film, Finished,
  Filter, Folder, FolderAdd, FolderOpened, Grid, Headset, List, Moon, MoreFilled, Picture, Position,
  Promotion, Rank, Refresh, Search, Setting, Share, Sort, Star, StarFilled, Sunny, SwitchButton, Tickets,
  TrendCharts, Upload, UploadFilled, VideoPlay,
} from '@element-plus/icons-vue'
import { appAlert, appChoice, appPrompt } from '../dialog'
import {
  api,
  errMsg,
  fmtSize,
  fmtTime,
  isVideo,
  VIDEO_EXT,
  type Entry,
  type DirectoryEntry,
  type EntryProperties,
  type SearchHit,
  type SearchParams,
  type ServerInfo,
} from '../api'
import { isDark, toggleDark } from '../theme'
import UploadDrawer, {
  type UploadCandidate,
  type UploadFile,
} from '../components/UploadDrawer.vue'
import PreviewDialog from '../components/PreviewDialog.vue'
import DirectoryMedia from '../components/DirectoryMedia.vue'
import BoundedImagePreview from '../components/BoundedImagePreview.vue'
import DirectoryViewport from '../components/DirectoryViewport.vue'
import VirtualDirectoryTree from '../components/VirtualDirectoryTree.vue'
import { createDirectoryController } from '../directory/controller'
import { deleteBatches, freezeTargets, moveSerial, type OperationProgress } from '../directory/operations'
import DirPicker from '../components/DirPicker.vue'
import ShareDialog from '../components/ShareDialog.vue'
import ShareManager from '../components/ShareManager.vue'
import TrashPanel from '../components/TrashPanel.vue'

const route = useRoute()
const router = useRouter()
const directory = createDirectoryController(async (params, signal) => (await api.entries(params, signal)).data)
const { entries, selected, selectedIds, loading, total, hasMore, generation, revision, error: directoryError } = directory
const scrollOffset = ref(0)
const uploadDirty = ref(false)
const batchProgress = ref('')
let batchBusy = false
const info = ref<ServerInfo | null>(null)
const fileInput = ref<HTMLInputElement>()
const dirInput = ref<HTMLInputElement>()
const drawer = ref<InstanceType<typeof UploadDrawer>>()
const preview = ref<InstanceType<typeof PreviewDialog>>()
const dirPicker = ref<InstanceType<typeof DirPicker>>()
const shareDlg = ref<InstanceType<typeof ShareDialog>>()
const shareMgr = ref<InstanceType<typeof ShareManager>>()
const trashPanel = ref<InstanceType<typeof TrashPanel>>()
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
const AUDIO_EXT = ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus']
const DOCUMENT_EXT = [
  'pdf', 'txt', 'md', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'csv', 'epub', 'mobi',
]
const ARCHIVE_EXT = ['zip', 'rar', '7z', 'tar', 'tar.gz', 'gz', 'xz', 'zst', 'bz2', 'iso']
function extOf(name: string) {
  return name.split('.').pop()?.toLowerCase() ?? ''
}
function isImage(name: string) {
  return IMG_EXT.includes(extOf(name))
}

/** 按扩展名给非图片文件配图标与颜色 */
const ICON_RULES: [string[], typeof Document, string][] = [
  [['mp4', 'webm', 'mov', 'm4v', 'mkv', 'avi', 'ogv', 'ts', 'flv'], Film, '#8b5cf6'],
  [AUDIO_EXT, Headset, '#f59e0b'],
  [ARCHIVE_EXT, Box, '#b45309'],
  [DOCUMENT_EXT, Tickets, '#ef4444'],
]
function iconOf(name: string) {
  const ext = extOf(name)
  for (const [list, icon, color] of ICON_RULES) if (list.includes(ext)) return { icon, color }
  return { icon: Document, color: '#94a3b8' }
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
})

// ---- 搜索：输入即过滤当前目录，回车全局搜索，高级条件由弹窗提交 ----
const query = ref('')
const searchMode = ref(false)
const searchHits = ref<SearchHit[]>([])
type SearchView = 'simple' | 'advanced' | 'favorites'
const searchView = ref<SearchView>('simple')
const searchTitle = ref('全局搜索')
const searchLimit = ref(200)
const advancedVisible = ref(false)
const advancedLoading = ref(false)

interface AdvancedSearchForm {
  keyword: string
  kind: 'all' | 'file' | 'dir'
  category: 'all' | 'image' | 'video' | 'audio' | 'document' | 'archive' | 'custom'
  extensions: string
  minMb: number | undefined
  maxMb: number | undefined
  modifiedRange: [Date, Date] | null
  favorite: boolean
  sort: 'name' | 'size' | 'modified' | 'created'
  order: 'asc' | 'desc'
}

const advanced = reactive<AdvancedSearchForm>({
  keyword: '',
  kind: 'all',
  category: 'all',
  extensions: '',
  minMb: undefined,
  maxMb: undefined,
  modifiedRange: null,
  favorite: false,
  sort: 'modified',
  order: 'desc',
})

const countText = computed(() => searchMode.value
  ? `全局 ${searchHits.value.length} 个结果`
  : `已加载 ${entries.value.length} / 匹配 ${total.value ?? '…'}`)

async function doGlobalSearch() {
  const q = query.value.trim()
  if (!q) return
  try {
    const r = await api.search(q)
    searchHits.value = r.data.results
    searchTitle.value = `全局搜索“${q}”`
    searchLimit.value = 200
    searchView.value = 'simple'
    searchMode.value = true
  } catch (e) {
    ElMessage.error(errMsg(e))
  }
}
function exitSearch() {
  searchMode.value = false
  searchHits.value = []
  searchView.value = 'simple'
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

function resetAdvanced() {
  Object.assign(advanced, {
    keyword: '',
    kind: 'all',
    category: 'all',
    extensions: '',
    minMb: undefined,
    maxMb: undefined,
    modifiedRange: null,
    favorite: false,
    sort: 'modified',
    order: 'desc',
  } satisfies AdvancedSearchForm)
}

async function runAdvancedSearch() {
  if (
    advanced.minMb !== undefined &&
    advanced.maxMb !== undefined &&
    advanced.minMb > advanced.maxMb
  ) {
    ElMessage.warning('最小文件大小不能大于最大文件大小')
    return
  }
  const categoryExtensions: Record<AdvancedSearchForm['category'], string[]> = {
    all: [],
    image: IMG_EXT,
    video: VIDEO_EXT,
    audio: AUDIO_EXT,
    document: DOCUMENT_EXT,
    archive: ARCHIVE_EXT,
    custom: advanced.extensions
      .split(',')
      .map((extension) => extension.trim().replace(/^\.+/, ''))
      .filter(Boolean),
  }
  const params: SearchParams = {
    q: advanced.keyword.trim() || undefined,
    kind: advanced.kind,
    extensions:
      advanced.kind === 'dir'
        ? undefined
        : categoryExtensions[advanced.category].join(',') || undefined,
    min_size:
      advanced.kind === 'dir' || advanced.minMb === undefined
        ? undefined
        : Math.round(advanced.minMb * 1024 * 1024),
    max_size:
      advanced.kind === 'dir' || advanced.maxMb === undefined
        ? undefined
        : Math.round(advanced.maxMb * 1024 * 1024),
    modified_after: advanced.modifiedRange
      ? Math.floor(advanced.modifiedRange[0].getTime() / 1000)
      : undefined,
    modified_before: advanced.modifiedRange
      ? Math.floor(advanced.modifiedRange[1].getTime() / 1000)
      : undefined,
    favorite: advanced.favorite,
    sort: advanced.sort,
    order: advanced.order,
    limit: 500,
  }
  advancedLoading.value = true
  try {
    const response = await api.search(params)
    searchHits.value = response.data.results
    searchTitle.value = '高级搜索结果'
    searchLimit.value = 500
    searchView.value = 'advanced'
    searchMode.value = true
    advancedVisible.value = false
  } catch (error) {
    ElMessage.error(errMsg(error))
  } finally {
    advancedLoading.value = false
  }
}

async function showFavorites() {
  try {
    const response = await api.search({
      favorite: true,
      sort: 'modified',
      order: 'desc',
      limit: 500,
    })
    searchHits.value = response.data.results
    searchTitle.value = '我的收藏'
    searchLimit.value = 500
    searchView.value = 'favorites'
    searchMode.value = true
  } catch (error) {
    ElMessage.error(errMsg(error))
  }
}

async function setFavorite(path: string, row: { favorite: boolean }) {
  const next = !row.favorite
  try {
    await api.favorite(path, next)
    row.favorite = next
    const local = entries.value.find((entry) => joinPath(cur.value, entry.name) === path)
    if (local) directory.updateFavorite(local.id, next)
    const hit = searchHits.value.find((candidate) => candidate.path === path)
    if (hit) hit.favorite = next
    if (
      !next &&
      (searchView.value === 'favorites' ||
        (searchView.value === 'advanced' && advanced.favorite))
    ) {
      searchHits.value = searchHits.value.filter((candidate) => candidate.path !== path)
    }
  } catch (error) {
    ElMessage.error(errMsg(error))
  }
}

function toggleEntryFavorite(row: Entry) {
  void setFavorite(joinPath(cur.value, row.name), row)
}

function toggleSearchFavorite(row: SearchHit) {
  void setFavorite(row.path, row)
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
const displayEntries = entries
function headerSort(key: SortKey) {
  if (sortKey.value === key) onSortCmd(sortAsc.value ? 'desc' : 'asc')
  else onSortCmd(key)
}
function sortIndicator(key: SortKey) {
  return sortKey.value === key ? sortAsc.value ? '↑' : '↓' : ''
}
function isSelected(row: Entry) {
  return 'id' in row && selectedIds.value.has(row.id as number)
}
function toggleSelect(row: DirectoryEntry) {
  directory.toggle(row.id)
}
function onTileCmd(cmd: string, row: Entry) {
  if (cmd === 'download') download(row)
  else if (cmd === 'rename') rename(row)
  else onRowCmd(cmd, row)
}

// ---- 全选(工具栏按钮 / Ctrl+A):时间线只有文件,其余模式含文件夹 ----
const allSelected = computed(() => entries.value.length > 0 && selectedIds.value.size === entries.value.length)
function setAllSelected(on: boolean) {
  if (!searchMode.value) directory.selectAll(on)
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
  items.push({
    label: row.favorite ? '取消收藏' : '收藏',
    action: () => void setFavorite(joinPath(cur.value, row.name), row),
  })
  items.push({ label: '分享', action: () => onRowCmd('share', row) })
  items.push({ label: '重命名', action: () => rename(row) })
  items.push({ label: '移动到 ...', action: () => onRowCmd('move', row) })
  items.push({ label: '复制到 ...', action: () => onRowCmd('copy', row) })
  items.push({ label: '属性', action: () => void openProperties(row) })
  items.push({
    label: '删除…',
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

// ---- 图片预览：只在已加载元数据内导航，最多挂载三张原图。 ----
const viewerShow = ref(false)
const viewerIndex = ref(0)

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

// ---- 远端目录控制器：查询输入立即隔离旧代，250ms 后才发新请求。 ----
const treeKey = ref(0)
let queryTimer: ReturnType<typeof setTimeout> | undefined
function conditions() {
  return { path: cur.value, q: query.value, kind: viewMode.value === 'timeline' ? 'file' as const : 'all' as const,
    sort: viewMode.value === 'timeline' ? 'mtime' as const : sortKey.value,
    order: viewMode.value === 'timeline' || !sortAsc.value ? 'desc' as const : 'asc' as const }
}
function loadNextBatch() {
  if (queryTimer === undefined && !searchMode.value) void directory.loadMore()
}
function resetDirectory(start = true) {
  if (queryTimer !== undefined) clearTimeout(queryTimer)
  queryTimer = undefined
  uploadDirty.value = false
  viewerShow.value = false
  closeCtx()
  return directory.reset(conditions(), start)
}
async function refresh() {
  await resetDirectory()
  api.info().then((r) => (info.value = r.data)).catch(() => {})
}
watch(query, () => {
  resetDirectory(false)
  queryTimer = setTimeout(() => { queryTimer = undefined; void directory.loadMore() }, 250)
}, { flush: 'sync' })
watch(cur, () => {
  query.value = ''
  exitSearch()
  void refresh()
}, { flush: 'sync' })
watch([sortKey, sortAsc, () => viewMode.value === 'timeline'], () => resetDirectory(), { flush: 'sync' })
onMounted(refresh)
onUnmounted(() => {
  if (queryTimer !== undefined) clearTimeout(queryTimer)
  directory.dispose()
})

function onTrashChanged() {
  treeKey.value++
  refresh()
}

function openEntry(row: Entry) {
  if (row.is_dir) {
    goto(joinPath(cur.value, row.name))
  } else if (isImage(row.name)) {
    viewerIndex.value = Math.max(0, displayEntries.value.findIndex((e) => e.name === row.name))
    viewerShow.value = true
  } else if (isVideo(row.name)) {
    ElMessage.info('上一个/下一个仅限当前已加载的视频')
    // 播放器只接收已加载范围，不扫描目录。
    const list = displayEntries.value
      .filter((e) => !e.is_dir && isVideo(e.name))
      .map((e) => joinPath(cur.value, e.name))
    preview.value?.open(joinPath(cur.value, row.name), list)
  } else {
    preview.value?.open(joinPath(cur.value, row.name))
  }
}

/** 进入短视频模式;取材范围在后台「流式播放」里设,这里只带上返回目录 */
function openShorts() {
  router.push({ path: '/shorts', query: { from: cur.value } })
}

function download(row: Entry) {
  window.open(api.downloadUrl(joinPath(cur.value, row.name)), '_blank')
}

async function mkdir() {
  const base = cur.value
  const version = generation.value
  try {
    const { value } = await appPrompt({
      title: '新建文件夹',
      message: base === '/' ? '将创建在根目录下' : `将创建在 ${base} 下`,
      placeholder: '输入文件夹名称',
      icon: FolderAdd,
      confirmText: '创建',
      pattern: /^[^/\\]+$/,
      errorMessage: '名称不能为空,且不能包含斜杠',
    })
    await api.mkdir(joinPath(base, value.trim()))
    treeKey.value++
    refreshAfterMutation(base, version)
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

async function rename(row: Entry) {
  const base = cur.value
  const version = generation.value
  row = { ...row }
  try {
    const { value } = await appPrompt({
      title: '重命名',
      message: row.name,
      value: row.name,
      placeholder: '输入新名称',
      icon: EditPen,
      confirmText: '重命名',
      pattern: /^[^/\\]+$/,
      errorMessage: '名称不能为空,且不能包含斜杠',
    })
    await api.rename(joinPath(base, row.name), joinPath(base, value.trim()))
    if (row.is_dir) treeKey.value++
    refreshAfterMutation(base, version)
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
  const base = cur.value
  const version = generation.value
  row = { ...row }
  const src = joinPath(base, row.name)
  if (cmd === 'properties') {
    await openProperties(row)
    return
  }
  if (cmd === 'favorite') {
    await setFavorite(src, row)
    return
  }
  try {
    if (cmd === 'share') {
      shareDlg.value?.open(src, row.name)
    } else if (cmd === 'move') {
      const target = await dirPicker.value!.open(`移动「${row.name}」到`, base)
      if (target === null || target === base) return
      await api.rename(src, joinPath(target, row.name))
      if (row.is_dir) treeKey.value++
      ElMessage.success(`已移动到 ${target}`)
      refreshAfterMutation(base, version)
    } else if (cmd === 'copy') {
      const target = await dirPicker.value!.open(`复制「${row.name}」到`, base)
      if (target === null) return
      const { value } = await appPrompt({
        title: '复制为',
        message: `复制到 ${target}`,
        value: target === base ? copyName(row.name) : row.name,
        placeholder: '目标文件名',
        icon: CopyDocument,
        confirmText: '复制',
        pattern: /^[^/\\]+$/,
        errorMessage: '名称不能为空,且不能包含斜杠',
      })
      await api.copy(src, joinPath(target, value.trim()))
      ElMessage.success('复制完成(元数据级,不占额外流量)')
      if (row.is_dir) treeKey.value++
      refreshAfterMutation(base, version)
    } else if (cmd === 'delete') {
      await confirmDelete([row])
    }
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

function showBatchProgress(state: OperationProgress) {
  batchProgress.value = `已处理 ${state.completed} / ${state.total} 项，失败 ${state.failed} 项`
}
function refreshAfterMutation(base: string, version: number) {
  if (cur.value === base && generation.value === version) void refresh()
}
async function moveSelected() {
  if (batchBusy) return
  const base = cur.value
  const version = generation.value
  const targets = freezeTargets(base, selected.value)
  if (!targets.length) return
  batchBusy = true
  try {
    const target = await dirPicker.value!.open(`移动 ${targets.length} 项到`, base)
    if (target === null || target === base) return
    showBatchProgress({ completed: 0, total: targets.length, failed: 0 })
    const failed = await moveSerial(targets, target, api.rename, showBatchProgress)
    if (failed.length) ElMessage.warning(`${failed.length} 项移动失败：${failed[0]!.path}，${failed[0]!.error}`)
    else ElMessage.success(`已移动到 ${target}`)
    if (targets.some((r) => r.entry.is_dir)) treeKey.value++
    refreshAfterMutation(base, version)
  } catch (e) {
    ElMessage.error(errMsg(e))
  } finally { batchBusy = false }
}

async function confirmDelete(rows: readonly Entry[]) {
  if (batchBusy || !rows.length) return
  const base = cur.value
  const version = generation.value
  const targets = freezeTargets(base, rows)
  const label = targets.length === 1 ? `「${targets[0]!.entry.name}」` : `选中的 ${targets.length} 项`
  batchBusy = true
  try {
    const choice = await appChoice({
      title: '删除项目',
      message: `${label}\n移入回收站后可以恢复；直接删除将立即移除且无法恢复，对应的 Telegram 消息也会进入清理队列。`,
      kind: 'primary', icon: Delete, confirmText: '移入回收站',
      alternateText: '直接删除', alternateKind: 'danger',
    })
    const permanent = choice === 'alternate'
    showBatchProgress({ completed: 0, total: targets.length, failed: 0 })
    const failed = await deleteBatches(targets, async (paths) => (await api.del(paths, permanent)).data, showBatchProgress)
    if (failed.length) ElMessage.warning(`有 ${failed.length} 项删除失败：${failed[0]!.path}，${failed[0]!.error}`)
    else ElMessage.success(permanent ? '已直接删除' : '已移入回收站')
    if (targets.some((x) => x.entry.is_dir)) treeKey.value++
    refreshAfterMutation(base, version)
  } finally { batchBusy = false }
}

async function removeSelected() {
  try {
    await confirmDelete(selected.value)
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errMsg(e))
  }
}

// ---- 上传入口:选文件 / 选文件夹 / 拖拽(含文件夹) / 粘贴 ----

type StreamConflictAction = 'overwrite' | 'skip'
interface PickerFileHandle {
  kind: 'file'
  name: string
  getFile(): Promise<File>
}
interface PickerDirectoryHandle {
  kind: 'directory'
  name: string
  values(): AsyncIterableIterator<PickerFileHandle | PickerDirectoryHandle>
}

const isFirefoxBrowser = /Firefox\//i.test(navigator.userAgent)
const DIRECTORY_BATCH_SIZE = isFirefoxBrowser ? 25 : 200
const MAX_SAFE_FIREFOX_DROP_ITEMS = 1000
let directoryScanGeneration = 0
const dragging = ref(false)
let dragDepth = 0

function onDragEnter() {
  dragDepth += 1
  dragging.value = true
}

function onDragLeave() {
  dragDepth = Math.max(0, dragDepth - 1)
  if (dragDepth === 0) dragging.value = false
}

function cancelDirectoryScan() {
  directoryScanGeneration += 1
  drawer.value?.clearDeferredFiles()
}

function beginDirectoryScan() {
  cancelDirectoryScan()
  return directoryScanGeneration
}

/**
 * 把惰性文件源按固定窗口送入上传队列。只有队列释放容量后才读取下一批 File，
 * 因此几万个文件不会同时变成前端任务和浏览器文件句柄。
 */
async function consumeUploadStream(
  target: string,
  generation: number,
  produce: (push: (file: File, rel: string) => Promise<boolean>) => Promise<void>,
  initialConflictAction: StreamConflictAction,
  fromDeferredSource = false,
) {
  let batch: UploadFile[] = []
  let conflictAction: StreamConflictAction = initialConflictAction

  const flush = async () => {
    if (!batch.length) return generation === directoryScanGeneration
    const current = batch
    batch = []
    const selected = await drawer.value?.enqueue(
      target,
      current,
      conflictAction,
      fromDeferredSource,
    )
    if (selected === 'cancel') {
      cancelDirectoryScan()
      return false
    }
    if (selected === 'overwrite' || selected === 'skip') conflictAction = selected
    return generation === directoryScanGeneration
  }

  const push = async (file: File, rel: string) => {
    if (generation !== directoryScanGeneration) return false
    if (batch.length === 0) {
      const available = await drawer.value?.waitForCapacity(DIRECTORY_BATCH_SIZE)
      if (!available || generation !== directoryScanGeneration) return false
    }
    batch.push({ file, rel })
    return batch.length < DIRECTORY_BATCH_SIZE || flush()
  }

  await produce(push)
  if (generation === directoryScanGeneration) await flush()
}

async function walkDirectoryHandle(
  handle: PickerDirectoryHandle,
  rel: string,
  push: (file: File, rel: string) => Promise<boolean>,
): Promise<boolean> {
  for await (const child of handle.values()) {
    if (child.kind === 'file') {
      if (!(await push(await child.getFile(), rel))) return false
      continue
    }
    const childRel = rel ? `${rel}/${child.name}` : child.name
    if (!(await walkDirectoryHandle(child, childRel, push))) return false
  }
  return true
}

async function scanDirectoryHandle(
  handle: PickerDirectoryHandle,
  rel: string,
  candidates: UploadCandidate[],
  isCurrent: () => boolean,
): Promise<boolean> {
  for await (const child of handle.values()) {
    if (!isCurrent()) return false
    if (child.kind === 'file') {
      candidates.push({ name: child.name, rel })
      continue
    }
    const childRel = rel ? `${rel}/${child.name}` : child.name
    if (!(await scanDirectoryHandle(child, childRel, candidates, isCurrent))) return false
  }
  return true
}

function fileRelativeDir(file: File) {
  const path = (file as File & { webkitRelativePath?: string }).webkitRelativePath ?? ''
  return path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : ''
}

async function candidatesFromFileList(files: FileList, withRelativePath: boolean) {
  const candidates: UploadCandidate[] = []
  for (let index = 0; index < files.length; index += 1) {
    const file = files[index]!
    candidates.push({ name: file.name, rel: withRelativePath ? fileRelativeDir(file) : '' })
    if (index > 0 && index % 500 === 0) {
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
    }
  }
  return candidates
}

async function chooseConflictPolicy(
  target: string,
  candidates: UploadCandidate[],
  generation: number,
): Promise<StreamConflictAction | null> {
  if (generation !== directoryScanGeneration) return null
  const selected = await drawer.value?.resolveUploadConflicts(target, candidates)
  if (!selected || selected === 'cancel' || generation !== directoryScanGeneration) {
    cancelDirectoryScan()
    return null
  }
  return selected
}

/** Chromium 优先使用惰性目录句柄；不支持时退回 webkitdirectory。 */
async function pickDirectory() {
  const showDirectoryPicker = (
    window as Window & { showDirectoryPicker?: () => Promise<PickerDirectoryHandle> }
  ).showDirectoryPicker
  if (!showDirectoryPicker) {
    if (isFirefoxBrowser) {
      await appAlert({
        title: 'Firefox 请拖拽文件夹上传',
        message:
          'Firefox 的文件夹选择器会在网页接管前一次性载入全部文件，几万个文件可能直接导致浏览器退出。请关闭此提示后，把文件夹从系统文件管理器拖到当前页面；拖拽入口会按批次惰性读取。',
        kind: 'warning',
        confirmText: '知道了',
      })
      return
    }
    dirInput.value?.click()
    return
  }

  try {
    // 必须在菜单点击事件内直接调用，浏览器才会认可用户手势。
    const root = await showDirectoryPicker.call(window)
    const target = cur.value
    const generation = beginDirectoryScan()
    const candidates: UploadCandidate[] = []
    await scanDirectoryHandle(
      root,
      root.name,
      candidates,
      () => generation === directoryScanGeneration,
    )
    if (generation !== directoryScanGeneration) return
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) return
    drawer.value?.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          await walkDirectoryHandle(root, root.name, push)
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) drawer.value?.clearDeferredFiles()
    }
  } catch (error) {
    if ((error as { name?: string }).name !== 'AbortError') {
      ElMessage.error(`读取文件夹失败：${errMsg(error)}`)
    }
  }
}

async function onPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  const files = input.files
  if (files?.length) {
    const target = cur.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, false)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) {
      input.value = ''
      return
    }
    drawer.value?.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            if (!(await push(files[index]!, ''))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) drawer.value?.clearDeferredFiles()
    }
    input.value = ''
  }
}

async function onDirPick(ev: globalThis.Event) {
  const input = ev.target as HTMLInputElement
  const files = input.files
  if (files?.length) {
    const target = cur.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, true)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) {
      input.value = ''
      return
    }
    drawer.value?.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            const file = files[index]!
            if (!(await push(file, fileRelativeDir(file)))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) drawer.value?.clearDeferredFiles()
    }
    input.value = ''
  }
}

let uploadRefreshTimer: number | undefined
function scheduleUploadRefresh() {
  if (uploadRefreshTimer !== undefined) window.clearTimeout(uploadRefreshTimer)
  uploadRefreshTimer = window.setTimeout(() => {
    uploadRefreshTimer = undefined
    if (scrollOffset.value > 0 || selectedIds.value.size || searchMode.value || batchBusy) uploadDirty.value = true
    else void refresh()
  }, 1000)
}

async function walkEntry(
  entry: FileSystemEntry,
  rel: string,
  push: (file: File, rel: string) => Promise<boolean>,
): Promise<boolean> {
  if (entry.isFile) {
    const f = await new Promise<File>((res, rej) => (entry as FileSystemFileEntry).file(res, rej))
    return push(f, rel)
  } else if (entry.isDirectory) {
    const dirRel = rel ? `${rel}/${entry.name}` : entry.name
    const reader = (entry as FileSystemDirectoryEntry).createReader()
    for (;;) {
      // readEntries 单次最多返回 100 项,要循环读到空为止
      const batch = await new Promise<FileSystemEntry[]>((res, rej) =>
        reader.readEntries(res, rej),
      )
      if (!batch.length) break
      for (const e of batch) if (!(await walkEntry(e, dirRel, push))) return false
    }
  }
  return true
}

async function scanEntry(
  entry: FileSystemEntry,
  rel: string,
  candidates: UploadCandidate[],
  isCurrent: () => boolean,
): Promise<boolean> {
  if (!isCurrent()) return false
  if (entry.isFile) {
    candidates.push({ name: entry.name, rel })
    return true
  }
  if (!entry.isDirectory) return true
  const dirRel = rel ? `${rel}/${entry.name}` : entry.name
  const reader = (entry as FileSystemDirectoryEntry).createReader()
  for (;;) {
    if (!isCurrent()) return false
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
      reader.readEntries(resolve, reject),
    )
    if (!batch.length) return true
    for (const child of batch) {
      if (!(await scanEntry(child, dirRel, candidates, isCurrent))) return false
    }
  }
}

async function onDrop(ev: DragEvent) {
  dragDepth = 0
  dragging.value = false
  const items = ev.dataTransfer?.items
  if (isFirefoxBrowser && items && items.length > MAX_SAFE_FIREFOX_DROP_ITEMS) {
    await appAlert({
      title: 'Firefox 无法安全处理此次拖拽',
      message: `Firefox 已把文件夹展开为 ${items.length} 个顶层文件项；网页无法在不复制这些文件句柄的情况下继续，强行处理会再次导致浏览器退出。请改用 Chrome/Edge 上传这个超大目录，或把目录拆分后再用 Firefox。`,
      kind: 'warning',
      confirmText: '知道了',
    })
    return
  }
  // 优先走 entry 递归(支持拖入整个文件夹);await 前必须同步取完 entry
  const firstItem = items?.[0] as
    | (DataTransferItem & { getAsEntry?: () => FileSystemEntry | null })
    | undefined
  const firstEntryGetter = firstItem?.webkitGetAsEntry ?? firstItem?.getAsEntry
  if (items?.length && typeof firstEntryGetter === 'function') {
    const roots: FileSystemEntry[] = []
    for (const it of items) {
      const extended = it as DataTransferItem & { getAsEntry?: () => FileSystemEntry | null }
      const getEntry = extended.webkitGetAsEntry ?? extended.getAsEntry
      const e = getEntry?.call(extended)
      if (e) roots.push(e)
    }
    if (roots.length) {
      const target = cur.value
      const generation = beginDirectoryScan()
      try {
        const candidates: UploadCandidate[] = []
        for (const root of roots) {
          if (
            !(await scanEntry(
              root,
              '',
              candidates,
              () => generation === directoryScanGeneration,
            ))
          ) {
            return
          }
        }
        if (generation !== directoryScanGeneration) return
        const conflictAction = await chooseConflictPolicy(target, candidates, generation)
        if (!conflictAction) return
        drawer.value?.setDeferredFileCount(candidates.length)
        await consumeUploadStream(
          target,
          generation,
          async (push) => {
            for (const root of roots) if (!(await walkEntry(root, '', push))) return
          },
          conflictAction,
          true,
        )
      } catch {
        /* 个别条目读取失败时保留已经进入队列的任务。 */
      } finally {
        if (generation === directoryScanGeneration) drawer.value?.clearDeferredFiles()
      }
      return
    }
  }
  const files = ev.dataTransfer?.files
  if (files?.length) {
    const target = cur.value
    const generation = beginDirectoryScan()
    const candidates = await candidatesFromFileList(files, false)
    const conflictAction = await chooseConflictPolicy(target, candidates, generation)
    if (!conflictAction) return
    drawer.value?.setDeferredFileCount(candidates.length)
    try {
      await consumeUploadStream(
        target,
        generation,
        async (push) => {
          for (let index = 0; index < files.length; index += 1) {
            if (!(await push(files[index]!, ''))) return
          }
        },
        conflictAction,
        true,
      )
    } finally {
      if (generation === directoryScanGeneration) drawer.value?.clearDeferredFiles()
    }
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
onUnmounted(() => {
  cancelDirectoryScan()
  dragging.value = false
  document.removeEventListener('paste', onPaste)
  if (uploadRefreshTimer !== undefined) window.clearTimeout(uploadRefreshTimer)
})

async function logout() {
  await api.logout().catch(() => {})
  router.push('/login')
}
</script>

<style scoped>
.directory-notice, .directory-status { padding: 10px; font-size: 13px; color: var(--el-text-color-secondary); }
.list-row { display: grid; grid-template-columns: 28px minmax(160px, 1fr) 96px 162px 255px; align-items: center; gap: 8px; height: 56px; box-sizing: border-box; padding: 0 6px; border-bottom: 1px solid var(--el-border-color-lighter); font-size: 13px; }
.list-heading { height: 36px; font-weight: 600; }
.list-heading button { cursor: pointer; text-align: left; border: 0; background: none; color: inherit; font: inherit; }
.list-row .name-cell { min-width: 0; overflow: hidden; }
.entry-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.entry-check { cursor: pointer; }
.entry-actions { display: flex; justify-content: flex-end; align-items: center; }
.list-row .thumb { display: flex; justify-content: center; align-items: center; }
.list-row .thumb .directory-media { width: 100%; height: 100%; }
.tile { height: 100%; box-sizing: border-box; }
.tile .entry-check { position: absolute; right: 8px; top: 8px; z-index: 2; }
.tile-actions { position: absolute; left: 8px; top: 8px; z-index: 2; background: var(--el-bg-color); border-radius: 8px; }
.tile-content { cursor: pointer; }
.tile-name { display: block; }
.tile.tile-photo .tile-media, .tile.tile-timeline .tile-media { height: 200px; }
@media (max-width: 1100px) { .list-row { grid-template-columns: 24px minmax(110px, 1fr) 76px 120px 36px; } .entry-actions > .el-button { display: none; } }
@media (max-width: 600px) { .list-row { grid-template-columns: 24px minmax(100px, 1fr) 68px 36px; } .entry-time, .list-heading > button:nth-of-type(3) { display: none; } }

.page {
  margin: 0 auto;
  padding: 16px 24px 40px;
  min-height: 100vh;
  box-sizing: border-box;
}
.drop-overlay {
  position: fixed;
  inset: 18px;
  z-index: 4000;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  border: 2px dashed var(--el-color-primary);
  border-radius: 20px;
  background: color-mix(in srgb, var(--el-bg-color) 88%, transparent);
  color: var(--el-text-color-primary);
  pointer-events: none;
  backdrop-filter: blur(8px);
}
.drop-overlay .el-icon {
  color: var(--el-color-primary);
  font-size: 48px;
}
.drop-overlay strong {
  font-size: 20px;
}
.drop-overlay span {
  color: var(--el-text-color-secondary);
  font-size: 13px;
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
.favorite-on,
.favorite-mark {
  color: #f59e0b !important;
}
.favorite-mark {
  flex-shrink: 0;
  font-size: 14px;
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
.tile-more,
.tile-favorite {
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
.tile-favorite {
  left: 44px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  color: #64748b;
  cursor: pointer;
  border-radius: 50%;
  background: rgba(255, 255, 255, 0.86);
  box-shadow: 0 1px 4px rgba(30, 40, 80, 0.2);
}
html.dark .tile-favorite {
  background: rgba(30, 36, 54, 0.88);
}
.tile-favorite.on {
  color: #f59e0b;
  opacity: 1;
}
.tile:hover .tile-check,
.tile:hover .tile-more,
.tile:hover .tile-favorite,
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

.advanced-search-form {
  padding: 4px 8px 0;
}
.size-range,
.sort-range {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
}
.size-range .el-input-number,
.sort-range .el-select {
  flex: 1;
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
