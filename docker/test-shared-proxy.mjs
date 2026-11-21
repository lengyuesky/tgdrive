// 构建两个项目镜像后运行：node docker/test-shared-proxy.mjs [宿主镜像] [Bot API 镜像]
// 使用本地镜像、独立内部网络、空数据库和测试凭据，不连接 Telegram 或生产实例。
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'

const prefix = `tgdrive-proxy-test-${randomUUID().slice(0, 8)}`
const app = `${prefix}-app`
const bot = `${prefix}-bot`
const upstream = `${prefix}-upstream`
const appImage = process.argv[2] || 'tgdrive:latest'
const botImage = process.argv[3] || 'tgdrive-telegram-bot-api:latest'
const nodeImage = process.argv[4] || 'node:22-bookworm-slim'
const docker = (...args) => execFileSync('docker', args, {
  encoding: 'utf8', timeout: 45000, stdio: ['pipe', 'pipe', 'pipe'],
}).trim()
const delay = ms => new Promise(resolve => setTimeout(resolve, ms))
async function until(check) {
  for (let attempt = 0; attempt < 60; attempt++) {
    try { if (check()) return } catch {}
    await delay(300)
  }
  throw new Error('容器验证等待超时')
}
function node(script) {
  return docker('run', '--rm', '--pull=never', '--network', prefix, nodeImage, 'node', '-e', script)
}
function configure(patch, reset = false) {
  node(`(async()=>{
    const login=await fetch('http://tgdrive:8080/admin/api/auth/login',{
      method:'POST',headers:{'content-type':'application/json'},
      body:JSON.stringify({username:'test',password:'integration-test-only'})
    });
    if(!login.ok) throw new Error('测试登录失败');
    const response=await fetch('http://tgdrive:8080/admin/api/admin/config${reset ? '/reset' : ''}',{
      method:'POST',headers:{'content-type':'application/json',cookie:login.headers.get('set-cookie').split(';')[0]},
      body:JSON.stringify(${JSON.stringify(patch)})
    });
    if(!response.ok) throw new Error('测试代理设置失败');
  })().catch(()=>{process.exitCode=1})`)
}
function probe() {
  return docker('exec', bot, 'sh', '-c',
    'cfg=$(ls /tmp/tgdrive-proxy.*); proxychains4 -q -f "$cfg" wget -T 5 -qO- http://tgdrive:8080/admin/healthz')
}
function connections(path = '') {
  return Number(docker('exec', upstream, 'node', '-e',
    `fetch('http://localhost:9090/${path}').then(r=>r.text()).then(console.log)`))
}

try {
  for (const image of [appImage, botImage, nodeImage]) docker('image', 'inspect', image)
  docker('network', 'create', '--internal', prefix)
  docker('volume', 'create', prefix)
  docker('run', '-d', '--pull=never', '--init', '--name', app, '--network', prefix, '--network-alias', 'tgdrive',
    '--user', '0:0', '-v', `${prefix}:/bot-api-control`,
    '-e', 'TGDRIVE_USERNAME=test', '-e', 'TGDRIVE_PASSWORD=integration-test-only',
    '-e', 'TGDRIVE_APP_CATALOG_REPOSITORY=',
    '-e', 'TGDRIVE_BOT_API_PROXY_LISTEN=0.0.0.0:1082',
    '-e', 'TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf', appImage)
  docker('run', '-d', '--pull=never', '--name', bot, '--network', prefix,
    '-v', `${prefix}:/bot-api-control`, '-e', 'TELEGRAM_API_ID=1',
    '-e', 'TELEGRAM_API_HASH=00000000000000000000000000000000',
    '-e', 'TELEGRAM_LOCAL=1', '-e', 'TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf',
    botImage)
  await until(() => probe() === 'ok')
  let botPid = docker('exec', bot, 'pidof', 'telegram-bot-api')
  assert.match(botPid, /^\d+$/)
  console.log('通过：Bot API 启动正常，内部认证及直连转发正常')

  docker('run', '-d', '--pull=never', '--name', upstream, '--network', prefix, '--network-alias', 'upstream',
    nodeImage, 'node', '-e', `
    const http=require('http'),net=require('net');let connections=0,telegramConnections=0;
    const server=http.createServer((req,res)=>res.end(String(req.url==='/telegram'?telegramConnections:connections)));
    server.on('connect',(req,socket,head)=>{
      connections++;
      const index=req.url.lastIndexOf(':');
      const port=Number(req.url.slice(index+1));
      if(port===443||port===80) telegramConnections++;
      const remote=net.connect(port,req.url.slice(0,index),()=>{
        socket.write('HTTP/1.1 200 Connection established\\r\\n\\r\\n');
        if(head.length) remote.write(head);
        socket.pipe(remote);remote.pipe(socket);
      });
      remote.setTimeout(3000,()=>remote.destroy());
      remote.on('close',()=>socket.destroy());socket.on('close',()=>remote.destroy());
      remote.on('error',()=>socket.destroy());socket.on('error',()=>remote.destroy());
    });server.listen(9090,'0.0.0.0');`)
  await until(() => connections() === 0)
  configure({ proxy_enabled: true, proxy_url: 'http://upstream:9090' })
  assert.equal(probe(), 'ok')
  assert.ok(connections() > 0)
  assert.equal(docker('exec', bot, 'pidof', 'telegram-bot-api'), botPid)
  console.log('通过：后台保存后自建 Bot API 共用 HTTP 代理，无需重启进程')

  // 内部网络没有外网路由，无效 Token 只用于触发真实 TDLib 的 DC 连接。
  node(`fetch('http://${bot}:8081/bot123456789:00000000000000000000000000000000000/getMe',{
    signal:AbortSignal.timeout(2000),
  }).catch(()=>{})`)
  await until(() => connections('telegram') > 0)
  console.log('通过：真实 Bot API 的 Telegram 连接经过共享代理')

  configure({ proxy_url: 'http://upstream:1' })
  assert.throws(probe)
  configure({ keys: ['proxy_enabled', 'proxy_url'] }, true)
  assert.equal(probe(), 'ok')
  console.log('通过：错误代理不回退直连，恢复设置后重新直连')

  const oldIp = docker('inspect', '--format', `{{(index .NetworkSettings.Networks "${prefix}").IPAddress}}`, app)
  // DNS 短暂不可用时保留进程，地址恢复后继续使用原入口。
  docker('network', 'disconnect', prefix, app)
  await until(() => docker('logs', bot).includes('暂时无法解析 tgdrive 容器地址'))
  assert.equal(docker('exec', bot, 'pidof', 'telegram-bot-api'), botPid)
  docker('network', 'connect', '--ip', oldIp, '--alias', 'tgdrive', prefix, app)
  await until(() => probe() === 'ok')
  assert.equal(docker('exec', bot, 'pidof', 'telegram-bot-api'), botPid)
  console.log('通过：Docker DNS 短暂不可用后恢复，不反复重启 Bot API')

  // 保持入口配置与密钥不变，只重接测试网络，复现 Docker 地址晚于配置更新的场景。
  const newIp = `${oldIp.slice(0, oldIp.lastIndexOf('.'))}.240`
  const revision = docker('exec', bot, 'sh', '-c', 'cksum < /bot-api-control/proxychains.conf')
  docker('network', 'disconnect', prefix, app)
  docker('network', 'connect', '--ip', newIp, '--alias', 'tgdrive', prefix, app)
  await until(() => docker('exec', bot, 'pidof', 'telegram-bot-api') !== botPid && probe() === 'ok')
  assert.equal(docker('exec', bot, 'sh', '-c', 'cksum < /bot-api-control/proxychains.conf'), revision)
  botPid = docker('exec', bot, 'pidof', 'telegram-bot-api')
  console.log('通过：tgdrive 地址变化后 Bot API 自动重新解析并恢复连接')

  docker('restart', app)
  await until(() => docker('exec', bot, 'pidof', 'telegram-bot-api') !== botPid && probe() === 'ok')
  console.log('通过：tgdrive 重启后 Bot API 自动更新入口凭据并重新连接')
} catch (error) {
  // 这里的容器只有固定测试凭据，日志不包含生产数据。
  console.error(error.message)
  for (const name of [app, bot]) {
    try { console.error(docker('logs', '--tail', '12', name)) } catch {}
  }
  process.exitCode = 1
} finally {
  for (const name of [bot, app, upstream]) {
    try { docker('rm', '-fv', name) } catch {}
  }
  try { docker('network', 'rm', prefix) } catch {}
  try { docker('volume', 'rm', prefix) } catch {}
}
