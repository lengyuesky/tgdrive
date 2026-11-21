<template>
  <SessionShell v-if="ready" />
  <main v-else class="session-loading" role="status">
    <p>{{ error || '正在打开网盘…' }}</p><el-button v-if="error" @click="verify">重试</el-button>
  </main>
</template>
<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref } from 'vue'
import { api, errMsg } from '../api'
import SessionShell from './SessionShell.vue'
import { onSessionInvalidated } from '../session'
const ready = ref(false)
const error = ref('')
let generation = 0
const stopInvalidation = onSessionInvalidated(() => { generation++; ready.value = false })
async function verify() {
  const request = ++generation
  error.value = ''
  try { await api.me(); if (request === generation) ready.value = true }
  catch (e) { if (request === generation) error.value = errMsg(e) }
}
onMounted(verify)
onBeforeUnmount(() => { generation++; stopInvalidation() })
</script>
<style scoped>.session-loading { padding: 80px 24px; text-align: center; }</style>
