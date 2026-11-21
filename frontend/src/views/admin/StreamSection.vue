<template>
  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      <div>
        tgdrive 使用并行下载和有限预读改善视频播放，支持拖动进度；关闭预览后停止后续请求。
        自建 telegram-bot-api 负责大文件上传和本地缓存直读。
      </div>
      <div>
        下面两项在
        <el-link type="primary" href="https://my.telegram.org" target="_blank"
          >my.telegram.org</el-link
        >
        申请,由 tgdrive 和 telegram-bot-api 共同读取。
      </div>
    </div>
  </div>

  <div class="fields">
    <SettingField
      label="api_id"
      :source="cfg?.sources.telegram_api_id"
      @reset="resetKey('telegram_api_id', 'api_id')"
    >
      <el-input v-model="form.telegram_api_id" placeholder="纯数字,如 21293566" />
    </SettingField>
    <SettingField
      label="api_hash"
      :source="cfg?.sources.telegram_api_hash"
      @reset="resetKey('telegram_api_hash', 'api_hash')"
    >
      <el-input
        v-model="form.telegram_api_hash"
        placeholder="32 位十六进制;不修改就保持原样"
      />
    </SettingField>
  </div>

  <div class="block">
    <div class="block-head">下载速度</div>
    <div class="fields">
      <SettingField
        label="下载并发"
        :source="cfg?.sources.download_concurrency"
        inline
        help="可调 1～8 路，默认 4 路。同一 Bot 最多同时处理 8 路下载请求。保存后对新打开的播放和下载生效。"
        @reset="resetKey('download_concurrency', '下载并发')"
      >
        <el-input-number
          v-model="form.download_concurrency"
          :min="1"
          :max="8"
          :precision="0"
          size="small"
          controls-position="right"
        />
      </SettingField>
    </div>
  </div>

  <div class="block">
    <div class="block-head">应用凭据启用步骤</div>
    <div class="steps">
      <div class="step"><span>1</span><div>填好上面两项,点右下角「保存并生效」</div></div>
      <div class="step">
        <span>2</span>
        <div>点顶栏的<b>导出 .env</b>,把内容覆盖到项目目录的 <code>.env</code></div>
      </div>
      <div class="step">
        <span>3</span>
        <div>在宿主机执行 <code>{{ RESTART_CMD }}</code>,重启这两个服务</div>
      </div>
    </div>
  </div>

  <div class="note">
    <el-icon><InfoFilled /></el-icon>
    <div>
      如需 GB 级分块上传,再回「Telegram 存储」把接口端点切到「自建 Bot API」并保存。
    </div>
  </div>
</template>

<script setup lang="ts">
import { InfoFilled } from '@element-plus/icons-vue'
import { api } from '../../api'
import SettingField from '../../components/SettingField.vue'
import { RESTART_CMD } from './useAdminConfig'
import { useAdminContext } from './useAdmin'

const {
  cfg,
  form,
  resetKey,
} = useAdminContext()
</script>
