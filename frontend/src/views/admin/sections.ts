import type { Component } from 'vue'
import {
  Cpu,
  FolderOpened,
  Link,
  Message,
  Notebook,
  Odometer,
  Promotion,
  Tools,
  User,
  VideoPlay,
} from '@element-plus/icons-vue'

// ---- 分区导航:一次只显示一块,避免整页长滚动 ----
export type SectionKey =
  | 'overview'
  | 'queues'
  | 'logs'
  | 'telegram'
  | 'proxy'
  | 'stream'
  | 'account'
  | 'backup'
  | 'inbox'
  | 'advanced'

export const SECTIONS: { key: SectionKey; label: string; desc: string; icon: Component }[] = [
  { key: 'overview', label: '概览', desc: '运行状态与快捷操作', icon: Odometer },
  { key: 'queues', label: '后台任务', desc: 'Telegram 消息删除', icon: Cpu },
  { key: 'logs', label: '后台日志', desc: '操作进度与失败重试', icon: Notebook },
  { key: 'telegram', label: 'Telegram 存储', desc: 'Bot、频道与分块', icon: Promotion },
  { key: 'proxy', label: '网络代理', desc: '自定义代理 / usque WARP', icon: Link },
  { key: 'stream', label: '流式播放', desc: 'MTProto / 自建 Bot API', icon: VideoPlay },
  { key: 'account', label: '账号安全', desc: '密码、两步验证与 WebDAV', icon: User },
  { key: 'backup', label: '备份与回收站', desc: '数据库快照与保留期', icon: FolderOpened },
  { key: 'inbox', label: 'Bot 收件', desc: '自动入库与分类目录', icon: Message },
  { key: 'advanced', label: '高级', desc: '临时目录、只读项与维护', icon: Tools },
]

export const SECTION_STORE = 'tgdrive.adminSection'
export const isSectionKey = (k: string): k is SectionKey => SECTIONS.some((s) => s.key === k)

/** 配置项 → 所属分区,用于在导航上标出哪块有改动 */
export const CFG_SECTION: Record<string, SectionKey> = {
  api_base: 'telegram',
  proxy_enabled: 'proxy',
  proxy_provider: 'proxy',
  proxy_url: 'proxy',
  bot_token: 'telegram',
  upload_bot_tokens: 'telegram',
  chat_id: 'telegram',
  chunk_size: 'telegram',
  upload_concurrency: 'telegram',
  download_concurrency: 'stream',
  send_video: 'telegram',
  telegram_api_id: 'stream',
  telegram_api_hash: 'stream',
  username: 'account',
  password: 'account',
  tmp_dir: 'advanced',
}

/** 字节数 → "18MB" 这类可直接回填的写法 */
export function sizeInput(n: number): string {
  const units: [number, string][] = [
    [1024 ** 3, 'GB'],
    [1024 ** 2, 'MB'],
    [1024, 'KB'],
  ]
  for (const [base, unit] of units) {
    if (n >= base && n % base === 0) return `${n / base}${unit}`
  }
  return String(n)
}
