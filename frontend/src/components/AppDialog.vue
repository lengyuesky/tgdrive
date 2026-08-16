<template>
  <Transition name="nd" appear @after-leave="emit('done')">
    <div v-if="visible" class="nd-mask" @mousedown.self="onMask">
      <div class="nd-card" role="dialog" aria-modal="true">
        <div class="nd-badge" :class="kind">
          <component :is="icon ?? fallbackIcon" class="nd-ico" />
        </div>
        <div class="nd-title">{{ title }}</div>
        <div v-if="message" class="nd-msg">{{ message }}</div>

        <div v-if="mode === 'prompt'" class="nd-field" :class="{ shake: shaking }">
          <input
            ref="inputRef"
            v-model="text"
            class="nd-input"
            :class="{ bad: !!err }"
            :placeholder="placeholder"
            spellcheck="false"
            @input="err = ''"
          />
          <div v-if="err" class="nd-err">{{ err }}</div>
        </div>

        <div class="nd-btns">
          <button v-if="mode !== 'alert'" type="button" class="nd-btn ghost" @click="onCancel">
            {{ cancelText }}
          </button>
          <button type="button" class="nd-btn solid" :class="kind" @click="onOk">
            {{ confirmText }}
          </button>
        </div>
      </div>
    </div>
  </Transition>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, type Component } from 'vue'
import { InfoFilled, WarningFilled } from '@element-plus/icons-vue'

const props = withDefaults(
  defineProps<{
    mode: 'confirm' | 'prompt' | 'alert'
    /** 徽章与确认按钮的配色 */
    kind?: 'primary' | 'danger' | 'warning'
    /** 徽章图标,不传按 kind 取默认 */
    icon?: Component
    title: string
    message?: string
    confirmText?: string
    cancelText?: string
    value?: string
    placeholder?: string
    pattern?: RegExp
    errorMessage?: string
  }>(),
  {
    kind: 'primary',
    confirmText: '确定',
    cancelText: '取消',
    value: '',
    placeholder: '',
    errorMessage: '输入不合法',
  },
)

const emit = defineEmits<{ resolve: [value: string]; reject: []; done: [] }>()

const visible = ref(true)
const text = ref(props.value)
const err = ref('')
const shaking = ref(false)
const inputRef = ref<HTMLInputElement>()
let closing = false

const fallbackIcon = computed(() => (props.kind === 'primary' ? InfoFilled : WarningFilled))

function finish(ok: boolean) {
  if (closing) return
  closing = true
  visible.value = false
  if (ok) emit('resolve', text.value)
  else emit('reject')
}

function onOk() {
  if (closing) return
  if (props.mode === 'prompt' && props.pattern && !props.pattern.test(text.value.trim())) {
    err.value = props.errorMessage
    // 先摘掉动画类并强制 reflow,连续出错时抖动才能重新播放
    shaking.value = false
    void nextTick(() => {
      void inputRef.value?.offsetWidth
      shaking.value = true
    })
    inputRef.value?.focus()
    return
  }
  finish(true)
}

function onCancel() {
  // alert 只是通知,任何关闭方式都视为已读确认
  if (props.mode === 'alert') finish(true)
  else finish(false)
}

function onMask() {
  onCancel()
}

function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    e.stopPropagation()
    onCancel()
  } else if (e.key === 'Enter') {
    e.stopPropagation()
    onOk()
  }
}

// 捕获阶段监听:比页面里其他快捷键先拿到,且不透传给下层 el-dialog
const prevOverflow = document.body.style.overflow
onMounted(() => {
  document.addEventListener('keydown', onKey, true)
  document.body.style.overflow = 'hidden'
  if (props.mode === 'prompt') {
    void nextTick(() => {
      inputRef.value?.focus()
      inputRef.value?.select()
    })
  }
})
onUnmounted(() => {
  document.removeEventListener('keydown', onKey, true)
  document.body.style.overflow = prevOverflow
})
</script>

<style scoped>
.nd-mask {
  position: fixed;
  inset: 0;
  z-index: 4000;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  box-sizing: border-box;
  background: rgba(15, 23, 42, 0.32);
  backdrop-filter: blur(10px) saturate(1.2);
  -webkit-backdrop-filter: blur(10px) saturate(1.2);
}
html.dark .nd-mask {
  background: rgba(2, 6, 18, 0.55);
}

.nd-card {
  width: min(400px, 100%);
  box-sizing: border-box;
  padding: 30px 28px 24px;
  border-radius: 22px;
  text-align: center;
  background: var(--glass-bg-heavy);
  backdrop-filter: blur(28px) saturate(1.7);
  -webkit-backdrop-filter: blur(28px) saturate(1.7);
  border: 1px solid var(--glass-border);
  box-shadow:
    0 24px 80px rgba(15, 23, 42, 0.28),
    0 4px 24px rgba(15, 23, 42, 0.12);
}
html.dark .nd-card {
  box-shadow:
    0 24px 80px rgba(0, 0, 0, 0.6),
    0 4px 24px rgba(0, 0, 0, 0.4);
}

.nd-badge {
  width: 60px;
  height: 60px;
  margin: 0 auto 16px;
  border-radius: 19px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #fff;
}
.nd-badge.primary {
  background: linear-gradient(135deg, #6366f1, #0ea5e9);
  box-shadow: 0 10px 26px rgba(99, 102, 241, 0.45);
}
.nd-badge.danger {
  background: linear-gradient(135deg, #fb7185, #ef4444);
  box-shadow: 0 10px 26px rgba(239, 68, 68, 0.42);
}
.nd-badge.warning {
  background: linear-gradient(135deg, #fbbf24, #f59e0b);
  box-shadow: 0 10px 26px rgba(245, 158, 11, 0.42);
}
.nd-ico {
  width: 30px;
  height: 30px;
}

.nd-title {
  font-size: 17px;
  font-weight: 700;
  letter-spacing: 0.3px;
  color: var(--el-text-color-primary);
}
.nd-msg {
  margin-top: 10px;
  font-size: 13px;
  line-height: 1.75;
  color: var(--text-soft);
  white-space: pre-line;
  overflow-wrap: anywhere;
}

.nd-field {
  margin-top: 18px;
}
.nd-input {
  width: 100%;
  box-sizing: border-box;
  height: 44px;
  padding: 0 14px;
  border-radius: 12px;
  border: 1.5px solid var(--glass-border);
  background: var(--tile-bg);
  font-size: 14px;
  font-family: inherit;
  color: var(--el-text-color-primary);
  text-align: center;
  outline: none;
  transition:
    border-color 0.18s,
    box-shadow 0.18s,
    background 0.18s;
}
.nd-input::placeholder {
  color: var(--text-soft);
}
.nd-input:focus {
  border-color: #6366f1;
  box-shadow: 0 0 0 4px rgba(99, 102, 241, 0.16);
  background: var(--tile-bg-hover);
}
.nd-input.bad {
  border-color: #ef4444;
  box-shadow: 0 0 0 4px rgba(239, 68, 68, 0.14);
}
.nd-err {
  margin-top: 8px;
  font-size: 12px;
  color: #ef4444;
}
.nd-field.shake {
  animation: nd-shake 0.4s;
}
@keyframes nd-shake {
  20% {
    transform: translateX(-7px);
  }
  40% {
    transform: translateX(6px);
  }
  60% {
    transform: translateX(-4px);
  }
  80% {
    transform: translateX(3px);
  }
}

.nd-btns {
  display: flex;
  gap: 10px;
  margin-top: 22px;
}
.nd-btn {
  flex: 1;
  height: 42px;
  border-radius: 12px;
  border: none;
  font-size: 14px;
  font-weight: 600;
  font-family: inherit;
  cursor: pointer;
  transition:
    transform 0.15s,
    box-shadow 0.15s,
    background 0.15s,
    filter 0.15s;
}
.nd-btn:active {
  transform: scale(0.97);
}
.nd-btn.ghost {
  background: var(--tile-bg);
  border: 1px solid var(--glass-border);
  color: var(--el-text-color-regular);
}
.nd-btn.ghost:hover {
  background: var(--tile-bg-hover);
}
.nd-btn.solid {
  color: #fff;
}
.nd-btn.solid.primary {
  background: linear-gradient(135deg, #6366f1, #0ea5e9);
  box-shadow: 0 8px 20px rgba(99, 102, 241, 0.35);
}
.nd-btn.solid.danger {
  background: linear-gradient(135deg, #fb7185, #ef4444);
  box-shadow: 0 8px 20px rgba(239, 68, 68, 0.32);
}
.nd-btn.solid.warning {
  background: linear-gradient(135deg, #fbbf24, #f59e0b);
  box-shadow: 0 8px 20px rgba(245, 158, 11, 0.32);
}
.nd-btn.solid:hover {
  filter: brightness(1.07);
  transform: translateY(-1px);
}

/* 进出场:遮罩淡入,卡片带一点回弹 */
.nd-enter-active {
  transition: opacity 0.22s ease;
}
.nd-leave-active {
  transition: opacity 0.18s ease;
}
.nd-enter-from,
.nd-leave-to {
  opacity: 0;
}
.nd-enter-active .nd-card {
  transition: transform 0.34s cubic-bezier(0.34, 1.56, 0.64, 1);
}
.nd-leave-active .nd-card {
  transition: transform 0.18s ease;
}
.nd-enter-from .nd-card {
  transform: scale(0.9) translateY(16px);
}
.nd-leave-to .nd-card {
  transform: scale(0.96);
}

@media (prefers-reduced-motion: reduce) {
  .nd-enter-active,
  .nd-leave-active,
  .nd-enter-active .nd-card,
  .nd-leave-active .nd-card {
    transition: none;
  }
  .nd-field.shake {
    animation: none;
  }
}
</style>
