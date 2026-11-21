<template>
  <div class="directory-media" :aria-label="video ? '视频缩略图' : '图片缩略图'"
    :title="!url && !video ? '缩略图加载中或不可用，点击查看原图' : undefined">
    <img v-if="url" :src="url" alt="" decoding="async" @error="url = null" />
    <el-icon v-else :color="video ? '#8b5cf6' : '#10b981'"><Film v-if="video" /><Picture v-else /></el-icon>
    <el-icon v-if="video && url" class="media-play"><VideoPlay /></el-icon>
  </div>
</template>

<script setup lang="ts">
import { onUnmounted, ref, watch } from 'vue'
import { Film, Picture, VideoPlay } from '@element-plus/icons-vue'
import { api, type DirectoryEntry } from '../api'
import { mediaLoader, type MediaLease } from '../media/loader'
import { contentFailure, contentVersion } from '../media/source'

const props = defineProps<{ entry: DirectoryEntry; path: string; video: boolean; generation: number }>()
const url = ref<string | null>(null)
let lease: MediaLease | undefined
let sequence = 0
watch(() => [props.path, contentVersion(props.entry), props.entry.has_thumbnail, props.video, props.generation], () => {
  const current = ++sequence
  url.value = null
  lease?.release()
  const version = contentVersion(props.entry)
  lease = mediaLoader.acquire({
    path: props.path, version, size: props.entry.size,
    thumbnailUrl: props.entry.has_thumbnail ? api.thumbnailUrl(props.path) : null,
    originalUrl: props.video ? null : api.downloadUrl(props.path, true),
    failure: contentFailure(props.entry, props.path, version),
  })
  void lease.result.then((result) => { if (current === sequence) url.value = result })
// 同一轮路径、代次和条目变化合并一次，避免依次更新 props 时反复领取并取消任务。
}, { immediate: true })
onUnmounted(() => { sequence++; lease?.release() })
</script>

<style scoped>
.directory-media { width: 100%; height: 100%; position: relative; display: flex; align-items: center; justify-content: center; overflow: hidden; }
.directory-media img { width: 100%; height: 100%; object-fit: cover; display: block; }
.media-play { position: absolute; color: white; filter: drop-shadow(0 1px 4px #000); }
</style>
