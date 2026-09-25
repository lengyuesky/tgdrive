<template>
  <div class="fields">
    <SettingField
      label="临时目录"
      :source="cfg?.sources.tmp_dir"
      wide
      help="超过 32MB 的分块会先落到这里再上传,需要有足够剩余空间。"
      @reset="resetKey('tmp_dir', '临时目录')"
    >
      <el-input v-model="form.tmp_dir" />
    </SettingField>
  </div>

  <div class="block">
    <div class="block-head">
      只读项 <span class="muted">改 .env 后重启容器才生效</span>
    </div>
    <div class="ro-grid">
      <div v-for="(v, k) in cfg?.readonly ?? {}" :key="k" class="ro-item">
        <span class="ro-key">{{ k }}</span>
        <span class="ro-val">{{ v }}</span>
      </div>
    </div>
    <div class="note">
      <el-icon><InfoFilled /></el-icon>
      <div>端口在 compose 里映射,数据库路径决定后台设置存在哪。</div>
    </div>
  </div>

  <div class="block">
    <div class="block-head">维护</div>
    <div class="row">
      <el-button :icon="Brush" @click="cleanup">清理残留上传</el-button>
      <el-button type="danger" plain :icon="SwitchButton" @click="logoutAll">
        登出所有会话
      </el-button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { Brush, InfoFilled, SwitchButton } from '@element-plus/icons-vue'
import SettingField from '../../components/SettingField.vue'
import { useAdminContext } from './useAdmin'

const {
  cfg,
  cleanup,
  form,
  logoutAll,
  resetKey,
} = useAdminContext()
</script>
