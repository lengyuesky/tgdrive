<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      <div>
        改「存储频道」只影响之后上传的文件:已存文件的分块仍在原频道,
        删除时可能留下无法自动清理的消息。
      </div>
      <div>从自建端点切回官方端点前请确认历史分块不超过 20MB,否则那些文件将下载不了。</div>
    </div>
  </div>

  <div class="fields">
    <SettingField
      label="接口端点"
      :source="cfg?.sources.api_base"
      wide
      @reset="resetKey('api_base', '接口端点')"
    >
      <div class="ctl-col">
        <el-segmented v-model="apiMode" :options="API_MODES" />
        <el-input
          v-if="apiMode === 'local'"
          v-model="form.api_base"
          placeholder="http://telegram-bot-api:8081"
        />
      </div>
      <template #help>
        官方端点分块 ≤18MB、限流较紧;自建端点(compose 已随项目启动)分块可到 GB 级。
      </template>
    </SettingField>

    <SettingField
      label="Bot Token"
      :source="cfg?.sources.bot_token"
      help="主 Bot：负责收件箱、数据库备份，也参与上传池；显示的是脱敏值。"
      @reset="resetKey('bot_token', 'Bot Token')"
    >
      <el-input v-model="form.bot_token" placeholder="不修改就保持原样" />
    </SettingField>

    <SettingField
      label="额外上传 Bot"
      :source="cfg?.sources.upload_bot_tokens"
      wide
      @reset="resetKey('upload_bot_tokens', '额外上传 Bot')"
    >
      <el-input
        v-model="form.upload_bot_tokens"
        type="textarea"
        :autosize="{ minRows: 3, maxRows: 8 }"
        placeholder="每行一个 Bot Token；留空只使用主 Bot"
      />
      <template #help>
        所有 Bot 必须加入同一个存储频道，并授予发消息和删消息权限。单块文件会按冷却状态和轮询分配给其中一个 Bot；需要多个分块的文件统一交给默认 Bot，并串行上传。最多 20 个 Bot。
      </template>
    </SettingField>

    <SettingField
      label="存储频道"
      :source="cfg?.sources.chat_id"
      help="不知道频道 ID?填好 Bot Token 后点「检测 ID」,按提示操作即可。"
      @reset="resetKey('chat_id', '存储频道')"
    >
      <div class="ctl-row">
        <el-input v-model="form.chat_id" placeholder="-100xxxxxxxxxx 或 @channelname" />
        <el-button @click="openDetect">检测 ID</el-button>
      </div>
    </SettingField>

    <SettingField
      label="分块大小"
      :source="cfg?.sources.chunk_size"
      @reset="resetKey('chunk_size', '分块大小')"
    >
      <el-input v-model="form.chunk_size" placeholder="如 18MB / 512MB" />
      <template #help>
        {{
          isOfficial
            ? `官方端点的 getFile 下载上限 20MB,分块不能超过 ${fmtSize(cfg?.official_chunk_max ?? 0)}`
            : '自建端点单文件上限 2000MB；建议使用 256MB，过大会让单次请求持续数分钟，网络中断后整块需要重传'
        }}。只影响之后上传的文件。
      </template>
    </SettingField>

    <SettingField
      label="上传并发"
      :source="cfg?.sources.upload_concurrency"
      inline
      help="每个 Bot API 实例的并发数；总并发 = 此值 × 上传 Bot 数量。例如 3 个 Bot、此处为 2 时总并发为 6。"
      @reset="resetKey('upload_concurrency', '上传并发')"
    >
      <el-input-number
        v-model="form.upload_concurrency"
        :min="1"
        :max="16"
        size="small"
        controls-position="right"
      />
    </SettingField>

    <SettingField
      label="视频消息模式"
      :source="cfg?.sources.send_video"
      inline
      wide
      @reset="resetKey('send_video', '视频消息模式')"
    >
      <el-switch v-model="form.send_video" />
      <template #help>
        开启后,不超过分块大小的 mp4 / mov / m4v 会以「视频消息」发到频道,在 Telegram
        里可直接播放(文件内容不变,下载不受影响)。mkv / webm 仍走文档 —— Telegram
        播放器不支持其常见编码,发成视频消息只有声音没画面。超过分块大小的视频按文档分块存储。
        自建端点默认开启,官方端点默认关闭。
      </template>
    </SettingField>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled } from '@element-plus/icons-vue'
import { api, fmtSize } from '../../api'
import SettingField from '../../components/SettingField.vue'
import { API_MODES } from './useAdminConfig'
import { useAdminContext } from './useAdmin'

const {
  apiMode,
  cfg,
  form,
  isOfficial,
  openDetect,
  resetKey,
} = useAdminContext()
</script>
