<template>
  <template v-if="extras">
    <div v-if="status?.inbox_failures" class="note warn">
      <el-icon><WarnTriangleFilled /></el-icon>
      <div>
        有 {{ status.inbox_failures }} 条消息连续处理失败并进入死信,后续收件未被阻塞。
      </div>
      <el-button size="small" @click="openInboxFailures">查看与清理</el-button>
    </div>
    <div class="note">
      <el-icon><InfoFilled /></el-icon>
      <div>
        <div>
          <b>Bot 私聊收件</b>和<b>频道收件</b>可独立开启或关闭，
          文件、视频、图片或音频按原类型零流量入库，两种入口共用下方分类目录。
        </div>
        <div>官方 Bot API 下载单文件上限 20MB:更大的文件也会入库,但需要自建端点才能下载。</div>
      </div>
    </div>

    <div class="fields">
      <SettingField
        label="Bot 私聊收件"
        help="将白名单用户私聊发给主 Bot 的文件转存到存储频道。"
        inline
      >
        <el-switch v-model="extras.inbox_bot_enabled" />
      </SettingField>
      <SettingField
        label="频道收件"
        help="将直接发送到存储频道的文件登记进网盘。"
        inline
      >
        <el-switch v-model="extras.inbox_channel_enabled" />
      </SettingField>

      <SettingField
        v-for="d in INBOX_DIRS"
        :key="d.key"
        :label="d.label"
        :help="d.help"
      >
        <div class="ctl-row">
          <el-input v-model="extras[d.key]" placeholder="/收件箱" />
          <el-button @click="chooseInboxDir(d.key)">选择</el-button>
        </div>
      </SettingField>

      <SettingField
        label="私聊白名单"
        wide
        help="数字为 Telegram 用户 ID:用对方账号私聊 bot 发条消息,点「检测 ID」就能看到;不在名单里的私聊消息一律忽略。"
      >
        <div class="ctl-row">
          <el-input
            v-model="extras.inbox_allowed_chats"
            placeholder="如 123456789,987654321；留空则不接收私聊文件"
          />
          <el-button @click="openDetect">检测 ID</el-button>
        </div>
      </SettingField>
    </div>

    <div class="block">
      <div class="block-head">
        自定义类型路由
        <el-button link type="primary" @click="addInboxRule">添加</el-button>
      </div>
      <div class="note">
        <el-icon><InfoFilled /></el-icon>
        <div>
          自定义规则优先于内置类型;按文件名后缀匹配,忽略大小写,并支持
          <code>tar.gz</code> 这类多段后缀,重叠时更长的后缀优先。
          同一个后缀不能出现在两条自定义规则中。
        </div>
      </div>
      <div v-if="extras.inbox_rules.length" class="rules">
        <div v-for="(rule, index) in extras.inbox_rules" :key="index" class="rule">
          <div class="rule-head">
            <b>规则 {{ index + 1 }}{{ rule.name.trim() ? ` · ${rule.name.trim()}` : '' }}</b>
            <el-button type="danger" link @click="removeInboxRule(index)">删除</el-button>
          </div>
          <div class="fields channel-fields">
            <SettingField label="类型名称">
              <el-input
                v-model="rule.name"
                maxlength="40"
                placeholder="例如：音乐、压缩包、项目源码"
              />
            </SettingField>
            <SettingField label="存储目录">
              <div class="ctl-row">
                <el-input v-model="rule.dir" placeholder="/收件箱/自定义" />
                <el-button @click="chooseRuleDir(rule)">选择</el-button>
              </div>
            </SettingField>
            <SettingField
              label="文件后缀"
              wide
              help="可用逗号、分号或空格一次添加多个后缀,回车即添加。"
            >
              <div class="ext-editor">
                <div v-if="rule.extensions.length" class="tag-row">
                  <el-tag
                    v-for="(extension, extensionIndex) in rule.extensions"
                    :key="`${extension}-${extensionIndex}`"
                    closable
                    round
                    @close="removeRuleExtension(rule, extensionIndex)"
                  >
                    {{ extension.startsWith('.') ? extension : `.${extension}` }}
                  </el-tag>
                </div>
                <div class="ctl-row">
                  <el-input
                    v-model="inboxRuleExtensionDrafts[index]"
                    placeholder="输入 mp3、flac 或 tar.gz"
                    @keydown.enter="!isComposingKey($event) && ($event.preventDefault(), addRuleExtensions(rule, index))"
                  />
                  <el-button @click="addRuleExtensions(rule, index)">添加</el-button>
                </div>
              </div>
            </SettingField>
          </div>
        </div>
      </div>
      <button v-else class="rule-empty" @click="addInboxRule">
        <el-icon><Plus /></el-icon>
        <span class="quick-text">
          <b>添加自定义类型</b>
          <em>未匹配的文件会使用内置类型或默认目录</em>
        </span>
      </button>
    </div>
  </template>
</template>

<script setup lang="ts">
import { isComposingKey } from '../../mobile/keyboard'
import { InfoFilled, Plus, WarnTriangleFilled } from '@element-plus/icons-vue'
import SettingField from '../../components/SettingField.vue'
import { INBOX_DIRS } from './useAdminExtras'
import { useAdminContext } from './useAdmin'

const {
  addInboxRule,
  addRuleExtensions,
  chooseInboxDir,
  chooseRuleDir,
  extras,
  inboxRuleExtensionDrafts,
  openDetect,
  openInboxFailures,
  removeInboxRule,
  removeRuleExtension,
  status,
} = useAdminContext()
</script>
