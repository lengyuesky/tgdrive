import { test, expect, type Page } from '@playwright/test'
import { CONTRACT_VERSION, PAGE_CSP, SELF_API_BASE, APP_CENTER_BASE, ASSET_BASE, UI_CHANNEL, LIMITS,
  type InstalledApp, type Manifest, type ScopeInfo, type PageBootstrap } from '../src/apps/contract'

const manifest: Manifest = { id: 'browser-page', name: '浏览器测试应用', version: '1.0.0', description: '测试内构造的纯页面应用', author: '测试', contract: CONTRACT_VERSION,
  files: { read: true, write: false }, page: { entry: 'page/index.html' }, required_capabilities: [], open_with: { extensions: ['bin'], directories: true } }
const sha = 'a'.repeat(64)
const root = { id: '1', parent_id: null, name: '', path: '/', kind: 'directory' as const, size: '0', created_at: 0, modified_at: 0, revision: 'root-revision', content_version: null, children_revision: 'children-revision', has_thumbnail: false }
const script = `
const bootstrap = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(location.hash.slice('#tgdrive='.length).replaceAll('-','+').replaceAll('_','/')),c=>c.charCodeAt(0))));
history.replaceState(null,'',location.pathname+location.search);
let serial=0;const waiting=new Map();
function ui(method,params){return new Promise((resolve)=>{const id='request-'+(++serial);waiting.set(id,resolve);parent.postMessage({type:'request',channel:${JSON.stringify(UI_CHANNEL)},nonce:bootstrap.ui_nonce,id,request:{method,...(params===undefined?{}:{params})}},bootstrap.host_origin)})}
function api(path,init={}){return fetch(bootstrap.api_base+path,{...init,credentials:'omit',redirect:'error',headers:{Authorization:'Bearer '+bootstrap.session_token,...(init.headers||{})}})}
window.addEventListener('message',async(event)=>{
 if(event.source!==parent||event.origin!==bootstrap.host_origin||event.data.nonce!==bootstrap.ui_nonce)return;
 const data=event.data;
 if(data.type==='response'){waiting.get(data.id)?.(data.result);waiting.delete(data.id)}
 if(data.type==='notice'){
  if(data.notice.event==='ui.theme_changed')document.querySelector('#theme').textContent=data.notice.data.theme;
  if(data.notice.event==='ui.task_cancel'){document.querySelector('#task').textContent='已请求取消';await ui('ui.task',{id:'demo-task',title:'演示任务',completed:3,total:10,state:'cancelled',cancellable:false})}
  if(data.notice.event==='ui.before_close'){await api('/kv/progress',{method:'PUT',headers:{'Content-Type':'application/json'},body:JSON.stringify({value:{saved:true},condition:{type:'absent'}})});await ui('ui.closed',{close_id:data.notice.data.close_id})}
 }
});
document.querySelector('#immersive').onclick=async()=>{await ui('ui.set_immersive',{active:true,background:'#ffffff'});document.querySelector('#status').textContent='已沉浸'};
document.querySelector('#normal').onclick=()=>ui('ui.set_immersive',{active:false});
document.querySelector('#pick').onclick=async()=>{const result=await ui('ui.pick_directory',{});document.querySelector('#picked').textContent=result.value.directory===null?'已取消选择':'已选择'};
document.querySelector('#scope').onclick=async()=>{const result=await ui('ui.request_scope',{reason:'由主人选择读取范围',preferred_access:'read_only'});document.querySelector('#granted').textContent=result.value.scope===null?'未授权':result.value.scope.scope.mode};
document.querySelector('#unknown').onclick=async()=>{const result=await ui('files.bytes',{url:'https://outside.invalid',body:'不能走消息通道'});document.querySelector('#unknown-result').textContent=result.error.code};
document.querySelector('#network').onclick=async()=>{try{await fetch('https://blocked.invalid/request');document.querySelector('#network-result').textContent='错误'}catch{document.querySelector('#network-result').textContent='外站网络已阻止'}};
document.querySelector('#close').onclick=()=>ui('ui.close');
(async()=>{try{parent.document.body;document.querySelector('#isolation').textContent='错误'}catch{document.querySelector('#isolation').textContent='宿主 DOM 不可访问'}const info=await(await api('/info')).json();document.querySelector('#identity').textContent=info.app.id;await ui('ui.set_title',{title:'测试页面标题'});await ui('ui.task',{id:'demo-task',title:'演示任务',completed:3,total:10,state:'running',cancellable:true});document.querySelector('#status').textContent='握手完成'})();
`
const html = `<!doctype html><html><head><meta charset="UTF-8"><title>测试页面</title><style>body{font:16px system-ui;padding:24px;color:#172026;background:#fff}button{padding:12px;margin:8px;border:1px solid #777;background:#fafafa;border-radius:8px}p{line-height:1.8}</style></head><body><h1>最小纯页面应用</h1><p id="status">正在握手</p><p id="identity"></p><p id="isolation"></p><p id="theme"></p><button id="immersive">进入沉浸</button><button id="normal">退出沉浸</button><button id="pick">选择目录</button><p id="picked"></p><button id="scope">请求授权</button><p id="granted"></p><p id="task">演示任务</p><button id="unknown">尝试数据消息</button><p id="unknown-result"></p><button id="network">测试网络限制</button><p id="network-result"></p><button id="close">保存并关闭</button><script src="app.js"></script></body></html>`

async function mock(page: Page) {
  let scope: ScopeInfo = { revision: 'scope-1', scope: { mode: 'none' }, roots: [] }
  const app = (): InstalledApp => ({ manifest, generation: 'generation-1', state_revision: 'state-1', enabled: true, pinned: false, scope, package: { target: 'web', sha256: sha, size: '100' }, backend: null, installed_at: 1, updated_at: 1 })
  const requests: { path: string; method: string; authorization?: string; origin?: string }[] = [], outside: string[] = [], order: string[] = []
  let closed = false
  page.on('request', (request) => { if (!request.url().startsWith('http://127.0.0.1:4178/')) outside.push(request.url()) })
  await page.route('**/*', async (route) => {
    const url = new URL(route.request().url())
    if (url.origin !== 'http://127.0.0.1:4178') return route.abort()
    if (url.pathname.startsWith(`${ASSET_BASE}/`)) {
      requests.push({ path: url.pathname, method: route.request().method() })
      return route.fulfill({ contentType: url.pathname.endsWith('.js') ? 'application/javascript' : 'text/html', headers: { 'Content-Security-Policy': PAGE_CSP, 'Access-Control-Allow-Origin': '*', 'Cross-Origin-Resource-Policy': 'cross-origin', 'X-Content-Type-Options': 'nosniff' }, body: url.pathname.endsWith('.js') ? script : html })
    }
    if (!url.pathname.startsWith('/admin/api/')) return route.continue()
    if (url.pathname === '/admin/api/auth/me') return route.fulfill({ json: { username: '测试' } })
    if (url.pathname === '/admin/api/uploads') return route.fulfill({ json: { sessions: [] } })
    if (url.pathname === '/admin/api/fs/entries') return route.fulfill({ json: { path: '/', entries: [], next_cursor: null, has_more: false, total: 0 } })
    if (url.pathname === `${APP_CENTER_BASE}/apps`) return route.fulfill({ json: { apps: [app()], root_app: { app_id: null, revision: 'root-1' } } })
    if (url.pathname === `${APP_CENTER_BASE}/catalog`) return route.fulfill({ json: { status: { state: 'disabled', repository: '', last_success_at: null, error: null }, result: { catalog: { format: CONTRACT_VERSION, repository: '', entries: [] }, incompatible: [] } } })
    if (url.pathname === `${APP_CENTER_BASE}/apps/${manifest.id}`) return route.fulfill({ json: app() })
    if (url.pathname.endsWith('/scope') && url.pathname.startsWith(APP_CENTER_BASE)) {
      const body = route.request().postDataJSON(); expect(body.expected_revision).toBe(scope.revision)
      scope = { revision: 'scope-2', scope: body.scope, roots: body.scope.mode === 'all' ? [{ directory_id: '1', access: body.scope.access, node: root }] : [] }
      return route.fulfill({ json: scope })
    }
    if (url.pathname.endsWith('/launch')) {
      const bootstrap: PageBootstrap = { contract: CONTRACT_VERSION, app: { id: manifest.id, name: manifest.name, version: manifest.version, generation: 'generation-1' }, session_token: 'test-page-token', expires_at: Date.now() + 6 * 3600 * 1000, host_origin: url.origin, api_base: `${url.origin}${SELF_API_BASE}`, backend_base: null, ui_nonce: 'test-page-nonce', theme: 'light', launch: { action: 'browse' } }
      return route.fulfill({ json: { frame_url: `${ASSET_BASE}/${manifest.id}/${sha}/page/index.html`, bootstrap } })
    }
    if (url.pathname.startsWith(SELF_API_BASE)) {
      const headers = await route.request().allHeaders(); requests.push({ path: url.pathname, method: route.request().method(), authorization: headers.authorization, origin: headers.origin })
      const cors = { 'Access-Control-Allow-Origin': 'null', 'Access-Control-Allow-Headers': 'Authorization, Content-Type', 'Access-Control-Allow-Methods': 'GET, POST, PUT, DELETE' }
      if (route.request().method() === 'OPTIONS') return route.fulfill({ status: 204, headers: cors })
      expect(headers.authorization).toBe('Bearer test-page-token')
      if (url.pathname === `${SELF_API_BASE}/session`) { closed = true; order.push('close'); return route.fulfill({ status: 204, headers: cors }) }
      if (url.pathname.endsWith('/kv/progress')) { order.push('save'); return route.fulfill({ headers: cors, json: { key: 'progress', value: { saved: true }, revision: 'revision-1', updated_at: Date.now() } }) }
      if (closed) return route.fulfill({ status: 401, headers: cors, json: { error: { code: 'session_expired', message: '已关闭', request_id: 'request' } } })
      return route.fulfill({ headers: cors, json: { contract: CONTRACT_VERSION, app: { id: manifest.id, name: manifest.name, version: manifest.version, generation: 'generation-1' }, caller: 'page', files: manifest.files, scope, capabilities: ['runtime.info', 'cache.objects'], limits: { json_request_bytes: String(LIMITS.MAX_JSON_REQUEST_BYTES), json_response_bytes: String(LIMITS.MAX_JSON_RESPONSE_BYTES), json_page_bytes: String(LIMITS.MAX_JSON_PAGE_BYTES) }, backend: null } })
    }
    return route.fulfill({ json: { ok: true } })
  })
  return { requests, outside, order }
}

test('纯页面真实沙箱握手、全部界面方法与关闭保存', async ({ page }) => {
  const state = await mock(page)
  await page.goto('/admin/apps')
  await page.getByRole('button', { name: '打开', exact: true }).click()
  const app = page.frameLocator('iframe')
  await expect(app.locator('#status')).toHaveText('握手完成')
  await expect(app.locator('#isolation')).toHaveText('宿主 DOM 不可访问')
  expect(await page.locator('iframe').getAttribute('sandbox')).toBe('allow-scripts allow-downloads')
  await expect(page).toHaveTitle('测试页面标题')
  expect(state.requests.some((request) => request.path === `${SELF_API_BASE}/info` && request.origin === 'null')).toBe(true)
  expect(state.requests.filter((request) => request.path.startsWith(ASSET_BASE)).every((request) => !request.path.includes('test-page-token'))).toBe(true)
  expect(await app.locator('body').evaluate(() => location.hash)).toBe('')
  await app.locator('#immersive').click(); await expect(page.locator('.app-container')).toHaveClass(/immersive/); await expect(page.locator('.mobile-tabs')).toHaveCount(0)
  await app.locator('#normal').click(); await expect(page.locator('.app-container')).not.toHaveClass(/immersive/)
  await app.locator('#pick').click(); await expect(page.getByRole('dialog', { name: '选择目录（选择不会授予权限）' })).toBeVisible(); await page.getByRole('dialog').getByRole('button', { name: '取消', exact: true }).click(); await expect(app.locator('#picked')).toHaveText('已取消选择')
  await app.locator('#scope').click(); const scope = page.getByRole('dialog', { name: '目录授权' }); await expect(scope).toBeVisible(); await scope.getByText('全部目录', { exact: true }).click(); await scope.getByRole('button', { name: '确认授权' }).click(); await expect(app.locator('#granted')).toHaveText('all')
  await page.getByRole('button', { name: '请求取消', exact: true }).click(); await expect(app.locator('#task')).toHaveText('已请求取消')
  await page.emulateMedia({ colorScheme: 'dark' }); await expect(app.locator('#theme')).toHaveText('dark')
  await app.locator('#unknown').click(); await expect(app.locator('#unknown-result')).toHaveText('capability_unavailable')
  await app.locator('#network').click(); await expect(app.locator('#network-result')).toHaveText('外站网络已阻止'); expect(state.outside).toEqual([])
  await page.screenshot({ path: '/tmp/tgdrive-app-sandbox.png', fullPage: true })
  await app.locator('#close').click(); await expect(page).toHaveURL(/\/admin\/apps$/); await expect(page.locator('iframe')).toHaveCount(0); expect(state.order).toEqual(['save', 'close'])
  await expect(page.locator('.mobile-tabs')).toHaveCount(1)
})
