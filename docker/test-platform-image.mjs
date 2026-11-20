import { execFileSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'

// 只使用指定的本地镜像、临时卷和内部网络；不读取 .env 或连接外站。
const image = process.argv[2]
const httpImage = process.argv[3] || 'node:22-bookworm-slim'
if (!image) throw new Error('用法：node docker/test-platform-image.mjs <本地宿主镜像> [本地 Node 22 镜像]')
const suffix = `${process.pid}-${randomBytes(4).toString('hex')}`
const name = `tgdrive-image-smoke-${suffix}`
const network = `${name}-network`
const volume = `${name}-data`
const docker = (...args) => execFileSync('docker', args, { encoding: 'utf8', maxBuffer: 2 * 1024 * 1024 }).trim()
const quietly = (...args) => { try { execFileSync('docker', args, { stdio: 'ignore' }) } catch {} }
let networkCreated = false
let volumeCreated = false
let containerCreated = false
try {
  docker('image', 'inspect', image)
  docker('image', 'inspect', httpImage)
  docker('network', 'create', '--internal', network)
  networkCreated = true
  docker('volume', 'create', volume)
  volumeCreated = true
  docker('run', '--detach', '--pull=never', '--init', '--name', name, '--network', network,
    '--user', '0:0', '--mount', `type=volume,source=${volume},target=/data`,
    '--env', 'TGDRIVE_USERNAME=image-smoke', '--env', 'TGDRIVE_PASSWORD=image-smoke-password',
    '--env', 'TGDRIVE_BOT_TOKEN=', '--env', 'TGDRIVE_CHAT_ID=', '--env', 'TGDRIVE_APP_CATALOG_REPOSITORY=', image)
  containerCreated = true
  // 内部网络不开放外部路由，也不发布端口；通过同一网络的临时客户端访问真实 HTTP 服务。
  const checkHttp = async base => {
    let admin
    const deadline = Date.now() + 45_000
    while (Date.now() < deadline) {
      try {
        admin = await fetch(`${base}/admin/`, { redirect: 'manual', signal: AbortSignal.timeout(1_000) })
        if (admin.status === 200) break
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 200))
    }
    if (admin?.status !== 200 || !/<!doctype html/i.test(await admin.text())) throw new Error('/admin/ 未返回管理页面 HTML')
    for (const [method, expected] of [['GET', 302], ['HEAD', 302], ['POST', 303]]) {
      const response = await fetch(`${base}/`, { method, redirect: 'manual', signal: AbortSignal.timeout(3_000) })
      if (response.status !== expected || response.headers.get('location') !== '/admin/') throw new Error(`${method} / 跳转不符合契约`)
    }
    const health = await fetch(`${base}/admin/healthz`, { signal: AbortSignal.timeout(3_000) })
    if (health.status !== 200) throw new Error('存活检查失败')
  }
  docker('run', '--rm', '--pull=never', '--network', network, '--entrypoint', 'node', httpImage,
    '--eval', `(${checkHttp.toString()})(${JSON.stringify(`http://${name}:8080`)}).catch(error => { console.error(error); process.exit(1) })`)
  const probe = docker('exec', name, '/usr/local/bin/ffprobe', '-version').split('\n')[0]
  const encoder = docker('exec', name, '/usr/local/bin/ffmpeg', '-version').split('\n')[0]
  docker('exec', name, '/usr/local/bin/tgdrive', 'healthcheck')
  console.log('正式镜像冒烟通过：/admin/ 200；GET/HEAD / 302、POST / 303 → /admin/；健康检查通过。')
  console.log(`媒体组件通过：${probe}；${encoder}`)
} catch (error) {
  if (containerCreated) {
    try { console.error(docker('logs', '--tail', '15', name)) } catch {}
  }
  throw error
} finally {
  if (containerCreated) quietly('rm', '--force', name)
  if (volumeCreated) quietly('volume', 'rm', volume)
  if (networkCreated) quietly('network', 'rm', network)
}
