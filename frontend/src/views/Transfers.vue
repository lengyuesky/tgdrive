<template>
  <main class="mobile-page transfers-page">
    <header class="page-heading"><router-link to="/files/">返回文件</router-link><h1>传输</h1></header>
    <div id="transfers-panel" class="group-card"><UploadDrawer inline /></div>
  </main>
</template>
<script setup lang="ts">
import UploadDrawer from '../components/UploadDrawer.vue'
</script>
