import { test, expect, type Page } from '@playwright/test'
import { CONTRACT_VERSION, PAGE_CSP, SELF_API_BASE, APP_CENTER_BASE, ASSET_BASE, UI_CHANNEL, LIMITS,
  type InstalledApp, type Manifest, type ScopeInfo, type PageBootstrap, type DirectoryGrant, type RuntimeLimits,
  type AppResourceLimits, type AppResourceLimitsUpdateRequest } from '../src/apps/contract'

const manifest: Manifest = { id: 'browser-page', name: '浏览器测试应用', version: '1.0.0', description: '测试内构造的纯页面应用', author: '测试', contract: CONTRACT_VERSION,
  files: { read: true, write: false }, page: { entry: 'page/index.html' }, required_capabilities: [], open_with: { extensions: ['bin'], directories: true } }
const sha = 'a'.repeat(64)
const root = { id: '1', parent_id: null, name: '', path: '/', kind: 'directory' as const, size: '0', created_at: 0, modified_at: 0, revision: 'root-revision', content_version: null, children_revision: 'children-revision', has_thumbnail: false }
const fileNode = { ...root, id: '2', parent_id: '1', name: 'item.bin', path: '/item.bin', kind: 'file' as const, size: '7', content_version: 'content-1', children_revision: null }
const script = `
const bootstrap = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(location.hash.slice('#tgdrive='.length).replaceAll('-','+').replaceAll('_','/')),c=>c.charCodeAt(0))));
history.replaceState(null,'',location.pathname+location.search);
document.body.dataset.opened=bootstrap.launch.action==='open'?bootstrap.launch.file.id:'';
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

const pageLimits: RuntimeLimits = {
  json_request_bytes: String(LIMITS.MAX_JSON_REQUEST_BYTES), json_response_bytes: String(LIMITS.MAX_JSON_RESPONSE_BYTES), json_page_bytes: String(LIMITS.MAX_JSON_PAGE_BYTES),
  file_page_limit: LIMITS.MAX_PAGE_LIMIT, query_root_limit: LIMITS.MAX_QUERY_ROOTS, extension_limit: LIMITS.MAX_QUERY_EXTENSIONS, mutation_operation_limit: LIMITS.MAX_MUTATION_OPERATIONS, mutation_assertion_limit: LIMITS.MAX_MUTATION_ASSERTIONS,
  random_limit: LIMITS.MAX_RANDOM_LIMIT, random_exclusion_limit: LIMITS.MAX_RANDOM_EXCLUSIONS, kv_key_bytes: LIMITS.MAX_KV_KEY_BYTES, kv_value_bytes: String(LIMITS.MAX_KV_VALUE_BYTES), kv_entry_limit: LIMITS.DEFAULT_KV_ENTRIES, kv_byte_limit: String(LIMITS.DEFAULT_KV_BYTES),
  cache_object_bytes: String(LIMITS.MAX_CACHE_OBJECT_BYTES), cache_metadata_bytes: String(LIMITS.MAX_CACHE_METADATA_BYTES), cache_lookup_limit: LIMITS.MAX_CACHE_LOOKUP_KEYS, cache_byte_limit: String(LIMITS.DEFAULT_CACHE_BYTES),
  control_concurrency: LIMITS.PAGE_CONTROL_CONCURRENCY, byte_stream_concurrency: LIMITS.PAGE_BYTE_CONCURRENCY, control_requests_per_second: LIMITS.PAGE_CONTROL_RATE, control_burst: LIMITS.PAGE_CONTROL_BURST,
  read_lease_limit: LIMITS.MAX_READ_LEASES, media_ticket_limit: LIMITS.MAX_MEDIA_TICKETS, upload_limit: LIMITS.MAX_UPLOADS, backend: null,
}

async function mock(page: Page, withBackend = false) {
  let scope: ScopeInfo = { revision: 'scope-1', scope: { mode: 'none' }, roots: [] }
  const app = (): InstalledApp => ({ manifest: withBackend ? { ...manifest, backend: { entry: 'backend/app', run_mode: 'resident', external_service: true } } : manifest, generation: 'generation-1', state_revision: 'state-1', enabled: true, pinned: false, scope, package: { target: withBackend ? 'linux-arm64' : 'web', sha256: sha, size: '100' }, backend: withBackend ? { state: 'ready', run_mode: 'resident', run_id: 'run-1', error_code: null } : null, installed_at: 1, updated_at: 1 })
  const requests: { path: string; method: string; authorization?: string; origin?: string }[] = [], outside: string[] = [], order: string[] = []
  let closed = false
  let limits: AppResourceLimits = { memory_bytes: String(LIMITS.DEFAULT_SHARED_BACKEND_MEMORY_BYTES), cpu_millis: LIMITS.DEFAULT_SHARED_BACKEND_CPU_MILLIS, memory_enforced: false, cpu_enforced: false }
  const limitUpdates: AppResourceLimitsUpdateRequest[] = []
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
    if (url.pathname === '/admin/api/info') return route.fulfill({ json: { username: '测试', files: 1, dirs: 0, total_size: 7, configured: true, official_api: false, api_base: '', chunk_size: 100 } })
    if (url.pathname === '/admin/api/fs/entries') return route.fulfill({ json: { path: '/', entries: url.searchParams.get('kind') === 'dir' ? [] : [{ id: 2, name: 'item.bin', is_dir: false, size: 7, created_at: 1, modified_at: 1, sha256: null, favorite: false, has_thumbnail: false }], next_cursor: null, has_more: false, total: 1 } })
    if (url.pathname === `${APP_CENTER_BASE}/nodes/stat`) {
      requests.push({ path: url.pathname, method: route.request().method() })
      const reference = route.request().postDataJSON()
      return route.fulfill({ json: reference.path === '/item.bin' || reference.id === '2' ? fileNode : root })
    }
    if (url.pathname === `${APP_CENTER_BASE}/apps/${manifest.id}/data/usage`) {
      requests.push({ path: url.pathname, method: route.request().method() })
      return route.fulfill({ json: { kv_entries: 2, kv_bytes: '1024', kv_entry_limit: LIMITS.DEFAULT_KV_ENTRIES, kv_byte_limit: String(LIMITS.DEFAULT_KV_BYTES), cache_bytes: '2048', cache_byte_limit: String(LIMITS.DEFAULT_CACHE_BYTES), database_bytes: withBackend ? '8589934592' : null } })
    }
    if (url.pathname === `${APP_CENTER_BASE}/apps`) return route.fulfill({ json: { apps: [app()], root_app: { app_id: null, revision: 'root-1' } } })
    if (url.pathname === `${APP_CENTER_BASE}/resource-limits`) {
      if (route.request().method() === 'PUT') {
        const body = route.request().postDataJSON() as AppResourceLimitsUpdateRequest
        limitUpdates.push(body)
        // 模拟已委派 cgroup 的宿主：设了上限的项即被强制。
        limits = { ...body, memory_enforced: body.memory_bytes !== '0', cpu_enforced: body.cpu_millis !== 0 }
      }
      return route.fulfill({ json: limits })
    }
    if (url.pathname === `${APP_CENTER_BASE}/catalog`) return route.fulfill({ json: { status: { state: 'disabled', repository: '', last_success_at: null, error: null }, result: { catalog: { format: CONTRACT_VERSION, repository: '', entries: [] }, incompatible: [] } } })
    if (url.pathname === `${APP_CENTER_BASE}/apps/${manifest.id}`) return route.fulfill({ json: app() })
    if (url.pathname.endsWith('/scope') && url.pathname.startsWith(APP_CENTER_BASE)) {
      const body = route.request().postDataJSON(); expect(body.expected_revision).toBe(scope.revision)
      scope = { revision: `scope-${Number(scope.revision.split('-')[1]) + 1}`, scope: body.scope, roots: body.scope.mode === 'all' ? [{ directory_id: '1', access: body.scope.access, node: root }] : body.scope.mode === 'selected' ? body.scope.directories.map((grant: DirectoryGrant) => ({ ...grant, node: root })) : [] }
      return route.fulfill({ json: scope })
    }
    if (url.pathname.endsWith('/launch')) {
      const request = route.request().postDataJSON(); closed = false
      if (request.open_node_id && scope.scope.mode === 'none') return route.fulfill({ status: 403, json: { error: { code: 'scope_denied', message: '尚未授权', request_id: 'request' } } })
      const bootstrap: PageBootstrap = { contract: CONTRACT_VERSION, app: { id: manifest.id, name: manifest.name, version: manifest.version, generation: 'generation-1' }, session_token: 'test-page-token', expires_at: Date.now() + 6 * 3600 * 1000, host_origin: url.origin, api_base: `${url.origin}${SELF_API_BASE}`, backend_base: null, ui_nonce: 'test-page-nonce', theme: 'light', launch: request.open_node_id ? { action: 'open', file: fileNode } : { action: 'browse' } }
      return route.fulfill({ json: { frame_url: `${ASSET_BASE}/${manifest.id}/${sha}/page/index.html?policy=0123456789abcdef`, bootstrap } })
    }
    if (url.pathname.startsWith(SELF_API_BASE)) {
      const headers = await route.request().allHeaders(); requests.push({ path: url.pathname, method: route.request().method(), authorization: headers.authorization, origin: headers.origin })
      const cors = { 'Access-Control-Allow-Origin': 'null', 'Access-Control-Allow-Headers': 'Authorization, Content-Type', 'Access-Control-Allow-Methods': 'GET, POST, PUT, DELETE' }
      if (route.request().method() === 'OPTIONS') return route.fulfill({ status: 204, headers: cors })
      expect(headers.authorization).toBe('Bearer test-page-token')
      if (url.pathname === `${SELF_API_BASE}/session`) { closed = true; order.push('close'); return route.fulfill({ status: 204, headers: cors }) }
      if (url.pathname.endsWith('/kv/progress')) { order.push('save'); return route.fulfill({ headers: cors, json: { key: 'progress', value: { saved: true }, revision: 'revision-1', updated_at: Date.now() } }) }
      if (closed) return route.fulfill({ status: 401, headers: cors, json: { error: { code: 'session_expired', message: '已关闭', request_id: 'request' } } })
      return route.fulfill({ headers: cors, json: { contract: CONTRACT_VERSION, app: { id: manifest.id, name: manifest.name, version: manifest.version, generation: 'generation-1' }, caller: 'page', files: manifest.files, scope, capabilities: ['runtime.info', 'cache.objects'], limits: pageLimits, backend: null } })
    }
    return route.fulfill({ json: { ok: true } })
  })
  return { requests, outside, order, limitUpdates }
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
  await app.locator('#pick').click(); await expect(page.getByRole('dialog', { name: '选择文件夹', exact: true })).toBeVisible(); await page.getByRole('dialog').getByRole('button', { name: '取消', exact: true }).click(); await expect(app.locator('#picked')).toHaveText('已取消选择')
  await app.locator('#scope').click(); const scope = page.getByRole('dialog', { name: '目录授权' }); await expect(scope).toBeVisible(); await scope.getByText('全部目录', { exact: true }).click(); await scope.getByRole('button', { name: '确认授权' }).click(); await expect(app.locator('#granted')).toHaveText('all')
  await page.getByRole('button', { name: '请求取消', exact: true }).click(); await expect(app.locator('#task')).toHaveText('已请求取消')
  await page.emulateMedia({ colorScheme: 'dark' }); await expect(app.locator('#theme')).toHaveText('dark')
  await app.locator('#unknown').click(); await expect(app.locator('#unknown-result')).toHaveText('capability_unavailable')
  await app.locator('#network').click(); await expect(app.locator('#network-result')).toHaveText('外站网络已阻止'); expect(state.outside).toEqual([])
  await page.screenshot({ path: '/tmp/tgdrive-app-sandbox.png', fullPage: true })
  await app.locator('#close').click(); await expect(page).toHaveURL(/\/admin\/apps$/); await expect(page.locator('iframe')).toHaveCount(0); expect(state.order).toEqual(['save', 'close'])
  await expect(page.locator('.mobile-tabs')).toHaveCount(1)
})

test('主人确认具体目录、查看占用并从文件打开应用', async ({ page }) => {
  const state = await mock(page)
  await page.goto('/admin/apps')
  await expect(page.locator('.app-card').getByRole('button', { name: '资源限制', exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: '目录授权', exact: true }).click()
  const scope = page.getByRole('dialog', { name: '目录授权' })
  await scope.getByText('指定目录', { exact: true }).click()
  await expect(scope.locator('.scope-tree')).toHaveAttribute('open', '')
  await scope.getByRole('button', { name: '选择根目录', exact: true }).click()
  await expect(scope.locator('.grant-list li')).toHaveCount(1)
  await scope.getByRole('button', { name: '确认授权' }).click()
  await expect(page.locator('.app-card .scope-summary')).toContainText('指定 1 个目录')
  await page.getByRole('button', { name: '占用', exact: true }).click()
  const occupancy = page.getByRole('dialog', { name: '应用占用' })
  await expect(occupancy).toContainText('1.0 KiB')
  await expect(occupancy.locator('dd').nth(2)).toHaveText('2.0 KiB / 1.0 GiB')
  await expect(occupancy.getByText('后端数据库', { exact: true })).toHaveCount(0)
  expect(state.requests.some((request) => request.path.endsWith('/data/usage') && request.method === 'GET')).toBe(true)
  await page.keyboard.press('Escape')
  await expect(occupancy).not.toBeVisible()
  await page.getByRole('button', { name: '打开', exact: true }).click()
  const app = page.frameLocator('iframe')
  await expect(app.locator('#status')).toHaveText('握手完成')
  await app.locator('#pick').click()
  const picker = page.getByRole('dialog', { name: '选择文件夹', exact: true })
  await picker.getByRole('button', { name: '确定', exact: true }).click()
  await expect(app.locator('#picked')).toHaveText('已选择')
  await page.getByRole('button', { name: '关闭应用', exact: true }).click()
  await expect(page).toHaveURL(/\/admin\/apps$/)
  await page.goto('/admin/files/')
  const row = page.locator('.panel [data-entry-id="2"]')
  await expect(row).toBeVisible()
  await row.click({ button: 'right' })
  await page.locator('.ctx-menu').getByText('打开方式：浏览器测试应用', { exact: true }).click()
  await expect(page).toHaveURL(/\/admin\/apps\/browser-page\?open=2/)
  const opened = page.frameLocator('iframe')
  await expect(opened.locator('#status')).toHaveText('握手完成')
  expect(await opened.locator('body').getAttribute('data-opened')).toBe('2')
  await page.screenshot({ path: '/tmp/tgdrive-app-owner-internal.png', fullPage: true })
})

test('后端占用显示对象缓存与数据库用量，数据库没有上限', async ({ page }) => {
  const state = await mock(page, true)
  await page.goto('/admin/apps')
  await page.getByRole('button', { name: '占用', exact: true }).click()
  const occupancy = page.getByRole('dialog', { name: '应用占用' })
  await expect(occupancy.locator('dt')).toHaveText(['键值记录', '键值字节', '可再生缓存', '后端数据库'])
  await expect(occupancy.locator('dd').nth(2)).toHaveText('2.0 KiB / 1.0 GiB')
  await expect(occupancy.locator('dd').nth(3)).toHaveText('8.0 GiB')
  await expect(occupancy).not.toContainText('数据库上限')
  expect(state.requests.some((request) => request.path.endsWith('/data/usage') && request.method === 'GET')).toBe(true)
})

test('应用资源限制只有一组总体设置，后端卡片无单独限制入口，保存后立即显示当前值', async ({ page }, testInfo) => {
  const state = await mock(page, true)
  await page.goto('/admin/apps')
  const card = page.locator('.app-card')
  await expect(card).toHaveCount(1)
  await expect(card.getByRole('button', { name: '资源限制', exact: true })).toHaveCount(0)
  await expect(card.getByRole('button', { name: '重启后端', exact: true })).toBeVisible()
  await expect(card.getByRole('button', { name: '后端日志', exact: true })).toBeVisible()
  await expect(page.getByRole('dialog', { name: '后端资源限制', exact: true })).toHaveCount(0)
  const section = page.locator('.resource-limits')
  const memory = section.getByLabel('应用内存上限'), cpu = section.getByLabel('应用 CPU 上限')
  await expect(memory).toHaveValue('2048'); await expect(cpu).toHaveValue('0')
  await expect(section.locator('.limit-state')).toHaveText('当前：内存 2.0 GiB · 未强制（没有可写的 cgroup）；CPU 不限制')
  const save = section.getByRole('button', { name: '保存限制', exact: true })
  await expect(save).toBeDisabled()
  await memory.fill('4096'); await cpu.fill('1.5'); await save.click()
  await expect(section.locator('.limit-state')).toHaveText('当前：内存 4.0 GiB · 已强制；CPU 1.5 核 · 已强制')
  expect(state.limitUpdates).toEqual([{ memory_bytes: '4294967296', cpu_millis: 1500 }])
  await expect(save).toBeDisabled()
  await page.screenshot({ path: testInfo.outputPath('resource-limits.png'), fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  await page.screenshot({ path: testInfo.outputPath('resource-limits-mobile.png'), fullPage: true })
  await memory.fill('0'); await cpu.fill('0'); await save.click()
  await expect(section.locator('.limit-state')).toHaveText('当前：内存 不限制；CPU 不限制')
  expect(state.limitUpdates[1]).toEqual({ memory_bytes: '0', cpu_millis: 0 })
})

test('应用中心一键刷新更新、单项失败继续和重试保留逐项结果', async ({ page }, testInfo) => {
  await mock(page)
  let apps: InstalledApp[] = ['browser-page', 'second-page'].map((id, index) => ({
    manifest: { ...manifest, id, name: index ? '第二个应用' : '第一个应用' }, generation: `generation-${id}`, state_revision: `state-${id}`,
    enabled: index === 0, pinned: index === 0, scope: { revision: `scope-${id}`, scope: { mode: 'all', access: 'read_only' }, roots: [] },
    package: { target: 'web', sha256: sha, size: '100' }, backend: null, installed_at: 1, updated_at: 1,
  }))
  let refreshed = false, failFirst = true
  const previewSources: Array<{ app_id: string; version: string; target: string; sha256: string }> = []
  const latest = apps.map(app => ({ manifest: { ...app.manifest, version: '1.0.1' }, packages: { web: { url: `https://github.com/test/apps/releases/download/${app.manifest.id}-v1.0.1/${app.manifest.id}-1.0.1-web.tgapp`, sha256: 'b'.repeat(64), size: '200' } } }))
  await page.route(`**${APP_CENTER_BASE}/**`, async route => {
    const path = new URL(route.request().url()).pathname
    if (path === `${APP_CENTER_BASE}/apps`) return route.fulfill({ json: { apps, root_app: { app_id: null, revision: 'root-1' } } })
    if (path === `${APP_CENTER_BASE}/catalog` || path === `${APP_CENTER_BASE}/catalog/refresh`) {
      if (route.request().method() === 'POST') refreshed = true
      return route.fulfill({ json: { status: { state: 'ready', repository: 'test/apps', last_success_at: 1, error: null }, result: { catalog: { format: CONTRACT_VERSION, repository: 'test/apps', entries: refreshed ? latest : [] }, incompatible: [] } } })
    }
    if (path === `${APP_CENTER_BASE}/install/preview`) {
      const { source } = route.request().postDataJSON()
      previewSources.push(source)
      const app = apps.find(app => app.manifest.id === source.app_id)!
      return route.fulfill({ json: { preview_id: app.manifest.id, expires_at: Date.now() + 600000, manifest: { ...app.manifest, version: source.version }, package: { target: source.target, sha256: source.sha256, size: '200' }, previous_state_revision: app.state_revision, added_file_read: false, added_file_write: false, starts_backend: false, external_network: false } })
    }
    if (path === `${APP_CENTER_BASE}/install`) {
      const { preview_id } = route.request().postDataJSON()
      if (preview_id === 'browser-page' && failFirst) { failFirst = false; return route.fulfill({ status: 503, json: { error: { code: 'upstream_unavailable', message: '测试包下载中断', request_id: 'request' } } }) }
      const app = apps.find(app => app.manifest.id === preview_id)!
      const updated: InstalledApp = { ...app, manifest: { ...app.manifest, version: '1.0.1' }, generation: `updated-${preview_id}`, state_revision: `updated-state-${preview_id}`, package: { target: 'web', sha256: 'b'.repeat(64), size: '200' } }
      apps = apps.map(app => app.manifest.id === preview_id ? updated : app)
      return route.fulfill({ json: updated })
    }
    return route.fallback()
  })
  await page.goto('/admin/apps')
  await page.getByRole('button', { name: '一键更新', exact: true }).click()
  const results = page.locator('.update-results li')
  await expect(results).toHaveCount(2)
  await expect(results.nth(0)).toContainText('测试包下载中断')
  await expect(results.nth(1)).toContainText('更新完成')
  expect(previewSources.map(source => [source.app_id, source.version, source.target])).toEqual([['browser-page', '1.0.1', 'web'], ['second-page', '1.0.1', 'web']])
  await page.getByRole('button', { name: '重试更新', exact: true }).click()
  await expect(results).toHaveCount(2)
  await expect(results.filter({ hasText: '更新完成' })).toHaveCount(2)
  expect(previewSources.map(source => source.app_id)).toEqual(['browser-page', 'second-page', 'browser-page'])
  expect(apps[0].pinned).toBe(true)
  expect(apps[1].enabled).toBe(false)
  expect(apps.every(app => app.scope.scope.mode === 'all')).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('updated-apps.png'), fullPage: true })
})
