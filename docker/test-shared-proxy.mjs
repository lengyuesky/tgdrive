// 构建两个项目镜像后运行：node docker/test-shared-proxy.mjs
// 使用独立网络、空数据库和测试凭据，不访问 Telegram 或生产实例。
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'

const prefix = `tgdrive-proxy-test-${randomUUID().slice(0, 8)}`
const app = `${prefix}-app`
const bot = `${prefix}-bot`
const upstream = `${prefix}-upstream`
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
  return docker('run', '--rm', '--network', prefix, 'node:22-bookworm-slim', 'node', '-e', script)
}
function configure(patch, reset = false) {
  node(`(async()=>{
    const login=await fetch('http://tgdrive:8080/api/auth/login',{
      method:'POST',headers:{'content-type':'application/json'},
      body:JSON.stringify({username:'test',password:'integration-test-only'})
    });
    if(!login.ok) throw new Error('测试登录失败');
    const response=await fetch('http://tgdrive:8080/api/admin/config${reset ? '/reset' : ''}',{
      method:'POST',headers:{'content-type':'application/json',cookie:login.headers.get('set-cookie').split(';')[0]},
      body:JSON.stringify(${JSON.stringify(patch)})
    });
    if(!response.ok) throw new Error('测试代理设置失败');
  })().catch(()=>{process.exitCode=1})`)
}
function probe() {
  return docker('exec', bot, 'sh', '-c',
    'cfg=$(ls /tmp/tgdrive-proxy.*); proxychains4 -q -f "$cfg" wget -T 5 -qO- http://tgdrive:8080/healthz')
}
function connections() {
  return Number(docker('exec', upstream, 'node', '-e',
    "fetch('http://localhost:9090').then(r=>r.text()).then(console.log)"))
}

try {
  docker('network', 'create', '--internal', prefix)
  docker('volume', 'create', prefix)
  docker('run', '-d', '--name', app, '--network', prefix, '--network-alias', 'tgdrive',
    '--user', '0:0', '-v', `${prefix}:/bot-api-control`,
    '-e', 'TGDRIVE_USERNAME=test', '-e', 'TGDRIVE_PASSWORD=integration-test-only',
    '-e', 'TGDRIVE_APP_CATALOG_REMOTE_ENABLED=false',
    '-e', 'TGDRIVE_BOT_API_PROXY_LISTEN=0.0.0.0:1082',
    '-e', 'TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf', 'tgdrive:latest')
  docker('run', '-d', '--name', bot, '--network', prefix,
    '-v', `${prefix}:/bot-api-control`, '-e', 'TELEGRAM_API_ID=1',
    '-e', 'TELEGRAM_API_HASH=00000000000000000000000000000000',
    '-e', 'TELEGRAM_LOCAL=1', '-e', 'TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf',
    'tgdrive-telegram-bot-api:latest')
  await until(() => probe() === 'ok')
  const botPid = docker('exec', bot, 'pidof', 'telegram-bot-api')
  assert.match(botPid, /^\d+$/)
  console.log('通过：Bot API 启动正常，内部认证及直连转发正常')

  docker('run', '-d', '--name', upstream, '--network', prefix, '--network-alias', 'upstream',
    'node:22-bookworm-slim', 'node', '-e', `
    const http=require('http'),net=require('net');let connections=0;
    const server=http.createServer((req,res)=>res.end(String(connections)));
    server.on('connect',(req,socket,head)=>{
      connections++;
      const index=req.url.lastIndexOf(':');
      const remote=net.connect(Number(req.url.slice(index+1)),req.url.slice(0,index),()=>{
        socket.write('HTTP/1.1 200 Connection established\\r\\n\\r\\n');
        if(head.length) remote.write(head);
        socket.pipe(remote);remote.pipe(socket);
      });
      remote.on('error',()=>socket.destroy());socket.on('error',()=>remote.destroy());
    });server.listen(9090,'0.0.0.0');`)
  await until(() => connections() === 0)
  configure({ proxy_enabled: true, proxy_url: 'http://upstream:9090' })
  assert.equal(probe(), 'ok')
  assert.ok(connections() > 0)
  assert.equal(docker('exec', bot, 'pidof', 'telegram-bot-api'), botPid)
  console.log('通过：后台保存后自建 Bot API 共用 HTTP 代理，无需重启进程')

  configure({ proxy_url: 'http://upstream:1' })
  assert.throws(probe)
  configure({ keys: ['proxy_enabled', 'proxy_url'] }, true)
  assert.equal(probe(), 'ok')
  console.log('通过：错误代理不回退直连，恢复设置后重新直连')

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
