<template>
  <template v-if="extras">
    <div class="note">
      <el-icon><InfoFilled /></el-icon>
      <div>
        一次只生成一份 SQLite 一致性快照,并同时上传到所有已启用渠道;
        每个渠道独立滚动保留最近 7 份。
      </div>
    </div>

    <div class="fields">
      <SettingField
        label="回收站保留"
        inline
        :help="`单位天,0 = 永不自动清理。当前回收站 ${trashCount} 项,超期自动彻底删除并释放频道消息。`"
      >
        <el-input-number
          v-model="extras.trash_retention_days"
          :min="0"
          :max="3650"
          size="small"
          controls-position="right"
        />
      </SettingField>
      <SettingField
        label="每日自动备份"
        inline
        :help="`每 24 小时备份到所有启用渠道,失败渠道每 30 分钟单独重试。${backupLastTs ? `最近成功:${fmtTime(backupLastTs)}` : '还没备份过。'}`"
      >
        <el-switch v-model="extras.backup_auto" />
      </SettingField>
    </div>

    <div class="block">
      <div class="block-head">备份渠道</div>
      <div class="channels">
        <div class="channel" :class="{ on: extras.backup_telegram_enabled }">
          <div class="channel-head">
            <span class="channel-ico"><el-icon><Promotion /></el-icon></span>
            <div class="channel-name">
              <b>Telegram 存储频道</b>
              <span>
                {{
                  backupLastByTarget.telegram
                    ? `上次成功 ${fmtTime(backupLastByTarget.telegram)}`
                    : '尚未成功备份'
                }}
              </span>
            </div>
            <el-switch v-model="extras.backup_telegram_enabled" />
          </div>
          <div v-if="extras.backup_telegram_enabled" class="fields channel-fields">
            <SettingField label="备份分块大小" wide>
              <el-input v-model="backupChunkSizeInput" placeholder="如 18MB / 512MB" />
              <template #help>
                使用上方 Telegram 存储配置。官方端点不得超过
                {{ fmtSize(cfg?.official_chunk_max ?? 19_500_000) }},自建端点最大
                1900MB;多块备份按文件名顺序合并后恢复。
              </template>
            </SettingField>
          </div>
        </div>

        <div class="channel" :class="{ on: extras.backup_webdav_enabled }">
          <div class="channel-head">
            <span class="channel-ico"><el-icon><Link /></el-icon></span>
            <div class="channel-name">
              <b>WebDAV</b>
              <span>
                {{
                  backupLastByTarget.webdav
                    ? `上次成功 ${fmtTime(backupLastByTarget.webdav)}`
                    : '尚未成功备份'
                }}
              </span>
            </div>
            <el-switch v-model="extras.backup_webdav_enabled" />
          </div>
          <div v-if="extras.backup_webdav_enabled" class="fields channel-fields">
            <SettingField
              label="备份目录 URL"
              wide
              help="目录需预先存在并允许 PUT、DELETE;修改 URL 后旧目录中的备份不会被自动删除。"
            >
              <el-input
                v-model="extras.backup_webdav_url"
                placeholder="https://dav.example.com/remote.php/dav/files/user/tgdrive"
              />
            </SettingField>
            <SettingField label="用户名(可选)">
              <el-input v-model="extras.backup_webdav_username" autocomplete="username" />
            </SettingField>
            <SettingField label="密码(可选)" help="已保存的密码只显示固定占位符。">
              <el-input
                v-model="extras.backup_webdav_password"
                type="password"
                show-password
                autocomplete="new-password"
                placeholder="未修改则保留原密码"
              />
            </SettingField>
          </div>
        </div>

        <div class="channel" :class="{ on: extras.backup_s3_enabled }">
          <div class="channel-head">
            <span class="channel-ico"><el-icon><Money /></el-icon></span>
            <div class="channel-name">
              <b>S3 兼容对象存储</b>
              <span>
                {{
                  backupLastByTarget.s3
                    ? `上次成功 ${fmtTime(backupLastByTarget.s3)}`
                    : '尚未成功备份'
                }}
              </span>
            </div>
            <el-switch v-model="extras.backup_s3_enabled" />
          </div>
          <div v-if="extras.backup_s3_enabled" class="fields channel-fields">
            <SettingField label="服务端点" wide>
              <el-input
                v-model="extras.backup_s3_endpoint"
                placeholder="https://s3.amazonaws.com 或 http://minio:9000"
              />
            </SettingField>
            <SettingField label="区域">
              <el-input v-model="extras.backup_s3_region" placeholder="us-east-1 / auto" />
            </SettingField>
            <SettingField label="存储桶">
              <el-input v-model="extras.backup_s3_bucket" placeholder="my-backups" />
            </SettingField>
            <SettingField label="对象前缀(可选)" wide>
              <el-input v-model="extras.backup_s3_prefix" placeholder="tgdrive" />
            </SettingField>
            <SettingField label="Access Key">
              <el-input
                v-model="extras.backup_s3_access_key"
                type="password"
                show-password
                autocomplete="new-password"
                placeholder="未修改则保留原密钥"
              />
            </SettingField>
            <SettingField label="Secret Key">
              <el-input
                v-model="extras.backup_s3_secret_key"
                type="password"
                show-password
                autocomplete="new-password"
                placeholder="未修改则保留原密钥"
              />
            </SettingField>
            <SettingField label="Session Token(临时凭据可选)" wide>
              <el-input
                v-model="extras.backup_s3_session_token"
                type="password"
                show-password
                autocomplete="new-password"
              />
            </SettingField>
            <SettingField
              label="路径样式"
              inline
              wide
              help="endpoint/bucket/key 形式,MinIO、IP 端点通常需要开启。使用 AWS Signature V4;修改端点、桶、前缀或 Access Key 后,旧目标中的备份不会被自动删除。"
            >
              <el-switch v-model="extras.backup_s3_path_style" />
            </SettingField>
          </div>
        </div>
      </div>
    </div>

    <div class="row">
      <el-button :loading="backingUp" :icon="Upload" @click="backupNow">
        立即备份到已启用渠道
      </el-button>
      <el-button :icon="Download" @click="downloadBackup">下载数据库备份</el-button>
    </div>
    <div class="note">
      <el-icon><InfoFilled /></el-icon>
      <div>
        WebDAV 与 S3 保存完整 .db;Telegram 单块 .db 可直接恢复,多块需按 part
        编号顺序二进制合并后再替换 data/tgdrive.db。
      </div>
    </div>
  </template>
</template>

<script setup lang="ts">
import { Download, InfoFilled, Link, Money, Promotion, Upload } from '@element-plus/icons-vue'
import { fmtSize, fmtTime } from '../../api'
import SettingField from '../../components/SettingField.vue'
import { useAdminContext } from './useAdmin'

const {
  backingUp,
  backupChunkSizeInput,
  backupLastByTarget,
  backupLastTs,
  backupNow,
  cfg,
  downloadBackup,
  extras,
  loading,
  trashCount,
} = useAdminContext()
</script>
