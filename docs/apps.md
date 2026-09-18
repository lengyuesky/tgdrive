# 页面应用开发说明

应用中心管理可独立发布的页面应用。主程序负责安装、权限、登录、设置和文件访问；应用包只包含
编译好的 HTML、JavaScript、CSS 等资源，不在服务器上执行脚本。
官方插件（短视频、图书、漫画、影视）在独立仓库 `lengyuesky/tgdrive-app` 中开发与发布；宿主默认通过 HTTPS 获取 GitHub 目录（schema_version: 2），在用户确认后校验安装包的 SHA256、长度和完整清单。支持原子持久缓存（`app-catalog-cache/`）与精确摘要安装，也可通过 `TGDRIVE_APP_CATALOG_DIR` 指定本地包目录，或手动导入 `.tgapp` 插件包。插件页面不依赖主站的 Vue 页面或内部组件。
图书、漫画只读取网盘文件，不接外部书源，也不修改文件页原有预览与菜单。

## 使用与生命周期

在文件页侧边栏「分享管理」下方的应用区块打开「应用中心」，默认展示「已安装」。未安装任何应用时也保留此标签，
可通过「浏览可安装应用」切换；整页重载重新选择已安装，页面内刷新保留当前选择。可以安装应用目录中的包，或选择「导入插件包」。确认安装前会展示作者、
版本及全部权限；更新时另外标出新增权限。任何来源的作者字段都是包内声明；SHA256 仅证明内容完整性，不表示作者身份认证。

新安装的应用立即启用，默认不固定入口。可在更多菜单中固定到侧边栏的应用区块，或停用、
卸载。停用、更新和卸载都会废止旧页面的运行凭据；同一浏览器的其他标签页立即检查状态，
其他设备上的已打开页面每 15 秒检查一次。已经传输到客户端的数据无法撤回。

更新包必须具有相同 ID、更高的语义版本和受支持的接口版本。主程序在确认摘要与旧安装版本后，
通过数据库事务切换版本，保留设置、固定入口和启停状态。安装包校验或写入失败不会替换旧版本。
卸载默认保留应用设置、私有数据、阅读进度和书签，勾选「同时清除这个应用的数据」才会删除。
卸载不会删除网盘文件或收藏。图书和漫画也不会自动安装或固定入口。

所有设备共用当前网盘实例的安装状态、设置和应用私有数据。应用包原件、配置和阅读记录保存在 SQLite 中，
因此随现有数据库备份恢复。数据库旁的 `apps/<应用 ID>/<包摘要>/` 是可重新生成的缓存，不需要单独备份。
私有数据迁移为 `migrations/0015_app_data.sql`，使用主程序现有迁移流程。

## 插件包格式

`.tgapp` 是标准 ZIP，也接受 `.zip` 后缀。包根目录必须包含 `app.json`：

```json
{
  "id": "my-viewer",
  "name": "我的查看器",
  "version": "1.0.0",
  "api_version": 2,
  "min_host_version": "0.1.0",
  "description": "查看选定目录中的文件。",
  "author": "开发者名称",
  "entry": "index.html",
  "icon": "icon.svg",
  "permissions": ["files.read", "media.read"],
  "settings": [
    {
      "key": "source_dir",
      "label": "文件夹",
      "description": "根目录表示整库。",
      "type": "directory",
      "default": "/"
    }
  ]
}
```

- ID 以小写字母开头，仅含小写字母、数字、短横线，最多 64 字节。
- `version` 和 `min_host_version` 使用语义版本；当前宿主支持接口版本 `1`、`2`。
- 当前短视频、图书、漫画和影视均声明 v2，已有第三方 v1 方法和握手格式保持兼容。只支持 v1 的旧宿主会在安装时拒绝 v2 包，需先升级宿主。
- 入口必须是存在于包内的 `.html` 文件。
- 可选 `icon` 声明包内 SVG 图标：路径规则与资源一致、必须以 `.svg` 结尾且存在于包内、不超过 256 KiB；
  声明 `icon` 的包需要支持该字段的宿主（提高 `min_host_version`）。图标仅在安装后展示，宿主以 `<img>` 方式加载（不执行脚本）；
  目录中未安装的应用与加载失败时回退字母图标。
- 设置支持 `directory`、`string`、`boolean`、`number`，最多 24 项。设置只能写入清单声明的键。
- 包大小最多 16 MiB，解压后最多 32 MiB，单个文件最多 8 MiB，最多 256 个文件和目录。
- 仅接受 ZIP 的存储或 Deflate 压缩，不接受加密包、符号链接、重复路径及越界路径。
- 资源路径每段使用英文字母、数字、点、短横线、下划线，不以点开头。使用相对路径引用资源。

在 `index.html` 中加载外部脚本，不写内联脚本：

```html
<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <link rel="stylesheet" href="./style.css" />
  <script defer src="./tgdrive-sdk.js"></script>
  <script defer src="./app.js"></script>
</head>
<body><main id="content"></main></body>
</html>
```

Vue、React 等框架可自行编译为静态资源；运行时不从 CDN 加载依赖，不使用 `eval` 或内联事件。

## 打包与分发

宿主平台位于 `src/apps/` 与 `frontend/src/apps/`，前端别名 `@apps` 只指向宿主应用中心代码；
数据库迁移仍位于 `migrations/`。宿主构建和 Docker 镜像不依赖、编译、复制或下载插件源码与安装包。
插件、SDK、打包工具和业务浏览器测试由独立仓库 [lengyuesky/tgdrive-app](https://github.com/lengyuesky/tgdrive-app) 维护，
具有自己的 `package.json`、锁文件和依赖目录，不共享宿主 `node_modules`。

在 **tgdrive-app 仓库根目录**使用 Node.js 22.22 或更高版本：

```bash
npm ci
npm run build
# 对已编译的第三方静态应用单独打包：
node package.mjs /绝对路径/我的应用/dist /绝对路径/输出目录
```

工具加入插件仓库的 `sdk/tgdrive-sdk.js` 并生成 `<ID>-<版本>.tgapp`；源目录不要再放同名 SDK 文件。
默认插件构建完成 TypeScript 编译、复制第三方许可及 PDF CMap/标准字体资源，再输出到 `.build/` 与 `apps/`，并在插件仓库根目录生成 `catalog.json` 与 `SHA256SUMS`。
不依赖服务器上的 PDF 转换服务或系统字体。构建包不等于安装包，任何部署都需要用户主动选择安装或升级，不自动固定入口。

### 目录配置与缓存

- 默认仓库为 `TGDRIVE_APP_GITHUB_REPO=lengyuesky/tgdrive-app`，只接受合法 GitHub `owner/repo`，不接受任意 URL、空白或 `.`/`..` 路径。
- 默认入口为 `https://raw.githubusercontent.com/lengyuesky/tgdrive-app/main/catalog.json`。非空 `TGDRIVE_APP_CATALOG_DIR` 优先选择可信的本地 `.tgapp`/`.zip` 目录；空值视为未配置，不扫描当前工作目录。
- 未指定本地目录时，`TGDRIVE_APP_CATALOG_REMOTE_ENABLED` 默认 `true`；设为 `false` 才依次发现工作目录中的 `app-catalog/`、`apps/catalog/`，均不存在则禁用目录。Docker 不预装这些目录；本地模式需自行挂载安装包目录。
- 同一 ID 展示最高兼容语义版本，本地与远程目录都支持按确认摘要找到仍存在的历史包；找不到时返回 409，绝不自动换成新版。
- 打开应用中心先展示已安装列表，再进行普通后台检查；手动刷新为强制检查。远程目录缓存有效期 5 分钟，联网至少间隔 30 秒并合并并发检查。失败保留上次成功快照（空目录也属于成功快照），标为 `stale`；没有好快照时为 `unavailable`，不阻止已安装应用运行。
- 数据库旁的 `app-catalog-cache/` 按仓库隔离，原子保存目录与已验证包。重启后过期目录仍显示过期，损坏缓存不可信；包缓存采用 128 MiB LRU，清理中断写入的临时文件，不触碰数据库、已安装包或运行数据。
- 请求使用后台当前出站代理，不读取系统代理或使用 GitHub token。目录总超时 10 秒、包总超时 120 秒（包括所有重定向与响应体），连接超时 10 秒。

### 稳定主分支目录协议（schema_version: 2）

目录为 `{schema_version:2, repository, entries}`；顶层仅包含 `schema_version`、`repository` 和 `entries`，entry 仅包含完整 `manifest`、64 位小写十六进制 `sha256`、正整数压缩包 `size` 与 `url`。不再使用 `release_tag`，不增加 `branch`/`ref` 字段。目录最多 1 MiB、512 条，每个应用 ID 最多一条最新版记录，压缩包不超过 16 MiB。
应用 `version` 必须是无前导零、预发布或构建后缀的稳定 SemVer `X.Y.Z`。
安装包下载 URL 必须精确为 `https://raw.githubusercontent.com/<repository>/main/apps/<id>-<version>.tgapp`；来源仓库链接 `source_url` 使用 `https://github.com/<repository>/tree/main/apps`。

目录仅包含每个 ID 的最新兼容版本，不再要求保留所有历史。更新时允许新版本替换旧版本，也允许目录移除插件，但严格禁止同 ID 倒退版本。对快照中相同 id/version 的条目，保留内容不可篡改的保护，严格校验 digest/manifest/size。
用户确认安装或更新时绑定对应条目的完整清单与摘要；若在此期间目录刷新导致该摘要不再存在，安装接口返回 409 Conflict，绝不因分支可变而跳过摘要校验。旧缓存若为 schema 1 协议会被安全忽略并刷新为新协议。
宿主先验证每条记录的 ID、稳定版本、兼容性标记、摘要、大小和 URL 格式；明确要求未来 API 或更高宿主版本的条目不参与安装，其未知清单扩展不会使兼容应用不可用。兼容条目仍严格拒绝未知清单字段、非法权限与设置；畸形目录不会替换上次好快照。
下载和缓存命中都校验实际 SHA256、长度、完整清单与 ZIP 安全限制，再调用原安装事务，安装锁不覆盖网络等待。

初始 URL 与每跳重定向严格限制在 `raw.githubusercontent.com` 的同一仓库主分支合法路径（`/<repo>/main/catalog.json` 或 `/<repo>/main/apps/<id>-<version>.tgapp`），重定向最多 5 次；必须为 HTTPS/443、无用户信息且禁止携带任何查询参数或片段。
错误与日志不回显下载 URL 或代理凭据。
插件版本及发布流程以[插件发布说明](https://github.com/lengyuesky/tgdrive-app/blob/main/docs/release.md)为准。

## 权限与隔离

| 权限 | 允许的能力 |
|---|---|
| `files.read` | 搜索、列目录、读取名称、路径、大小、节点 ID 和内容版本等元数据 |
| `media.read` | 读取单个文件内容，用于播放、缩略图和下载 |
| `favorites.write` | 添加或取消收藏 |

自身设置、应用私有数据、包内资源读取、目录选择窗口和应用导航无需额外权限。图书、漫画只申请
`files.read`、`media.read`，书签不是网盘收藏，不需要 `favorites.write`。目录选择由宿主界面交互完成，应用仅获得用户
最终选中的路径。当前文件权限以整个网盘实例为范围，取材目录是应用筛选设置，不是目录权限限制。

应用页面使用没有 `allow-same-origin` 的沙箱 iframe，响应头也包含强制沙箱约束。页面拿不到
登录 Cookie、管理员接口或宿主持有的运行凭据。所有能力请求经过明确的方法白名单和后端权限检查，
不通过消息体传入任意主站 URL。应用不能直接联网、加载外部页面、打开弹窗或注册主站服务工作线程。

媒体 URL 只授权一个文件、一个用途和当前应用修订版，最多有效 6 小时；每次请求仍检查启用状态、
文件可见性及当前签名密钥。播放使用既有 Range 流式读取，应用切换视频或退出时应清空播放器 `src`
并调用 `load()` 取消未完成请求。不要把大文件读成 Blob 再播放。

### 运行时资源与会话

- 资源 URL 按修订定址、内容按包摘要物化后不可变：响应携带 `Cache-Control: private, max-age=31536000, immutable`、
  `Vary: Accept-Encoding` 与强 ETag（`"摘要:路径"`），支持 If-None-Match 304 再验证；停用、更新、卸载都会轮换修订，
  不存在陈旧命中。已缓存旧资源拿不到有效会话令牌，无法调用任何能力。
- 资源路由按 Accept-Encoding 提供 brotli/gzip 压缩（小响应跳过；SVG 作为文本例外仍压缩）。Chromium 对沙箱
  不透明源的缓存分区会导致重开时重取入口与子资源，压缩使重开流量约为原文 1/3；WebKit/Firefox 分区策略下可
  直接命中缓存。应用中心、侧边栏和宿主工具栏的图标请求在主框架分区，重开命中磁盘缓存。
- 桌面端悬停已启用应用的图标会预取 launch 响应（60 秒内有效）；launch 只读不写，预取无副作用，移动端不预取。
- 运行会话默认 6 小时：宿主在临期 15 分钟内自动续签，原地替换会话令牌且不重建 iframe；休眠唤醒等过期场景，
  RPC 遇 `session_expired` 时宿主静默续签并重放一次。修订轮换（更新、停用）返回 `app_changed`（409），
  宿主提示重新打开而不是误判为网络故障。续签前已签发的媒体票据仍按原过期时间失效，重新请求 `media.url` 即获得新票据。
- 安装更新成功或失败后，数据库旁 `apps/<应用 ID>/` 内的旧摘要与孤儿缓存目录自动清理，只保留当前安装摘要；
  卸载仍删除整个应用目录。

## SDK

加载 SDK 后通过 `window.tgdrive` 调用；能力函数返回 Promise，`on` 返回取消订阅函数。等待 `tgdrive.ready`
获得 `{ id, name, version, api_version, dark, capabilities? }`。`capabilities` 为可选能力名称列表，旧宿主可能不提供；
不能仅凭 SDK 方法存在就假定宿主支持该能力。宿主和沙箱各自完成首次绘制后才开放可交互内容，
后台标签页不会因动画帧暂停而阻塞握手。API 错误为 Error；RPC 错误可包含 `code` 和 HTTP `status`。
单请求取消返回 `name === 'AbortError'`，不应显示为文件损坏。

### 兼容 v1 的基础方法

```js
const drive = window.tgdrive
await drive.ready
const settings = await drive.settings.get()
const { results } = await drive.files.search({
  under: settings.source_dir || '/',
  kind: 'file',
  extensions: 'mp4,webm',
  limit: 2000,
})

if (results.length) {
  const video = document.createElement('video')
  video.controls = true
  video.crossOrigin = 'anonymous'
  video.src = await drive.media.url(results[0].path)
  document.getElementById('content').append(video)
}
```

| 调用 | 返回或行为 |
|---|---|
| `files.search(params)` | `{ results }`，参数与文件搜索 API 一致，最多 2000 项 |
| `media.url(pathOrRef, kind = 'preview')` | 文件 URL；用途为 `preview`、`thumbnail`、`download`；路径字符串兼容 v1，v2 可传 `{ id, content_version }` 稳定引用 |
| `favorites.set(path, favorite)` | `{ ok, favorite }` |
| `settings.get()` | 当前设置，含默认值 |
| `settings.patch(values)` | 合并部分设置并返回最新设置 |
| `settings.open()` | 打开宿主的应用设置窗口 |
| `ui.pickDirectory(initial = '/')` | 选中的目录，取消返回 `null` |
| `ui.download(path)` | 由宿主触发浏览器下载 |
| `ui.close()` | 返回进入应用前的网盘目录 |
| `on('settings.changed', callback)` | 宿主设置窗口保存后的通知；返回取消订阅函数 |
| `on('theme.changed', callback)` | 主站主题变化通知，参数 `{ dark }` |

每个页面最多 16 个同时处理的调用、每 10 秒最多 120 次调用，单次参数最多 64 KiB。
普通请求超时为 30 秒，目录选择等界面交互为 5 分钟。6 小时运行会话过期后需要重新打开应用。

### v2：网页内沉浸（可选能力）

新宿主为 v2 应用声明 `ui.setImmersive`：

```js
const context = await drive.ready
if (context.capabilities?.includes('ui.setImmersive')) {
  // 仅在应用已经提供返回和菜单入口的阅读场景中开启。
  await drive.ui.setImmersive(true)
  // 返回内容库或打开失败时恢复；不要等待卸载才关闭。
  await drive.ui.setImmersive(false)
}
```

`ui.setImmersive(active)` 的开关必须是布尔值。消息方法为 `ui.setImmersive`，基础参数为 `{ active: boolean }`。
只有宿主另外声明 `ui.immersiveBackground` 时，插件才能调用 `ui.setImmersive(true, { background: '#f4ebd6' })`，
消息中附加 `background`。颜色仅接受六位十六进制不透明色，不接受 URL、透明色或任意 CSS；其他参数仍被拒绝。
阅读主题变化时可重复调用同步安全区底色，退出、会话失效和卸载会清理底色。不支持背景扩展的旧宿主只发送基础参数。
v1、非法参数、失效会话都会被拒绝。宿主在布局完成后确认，不重建 iframe；仅隐藏宿主导航并释放网页可视区，
不调用 Fullscreen API，不增加同源、网络或文件权限。能力调用仍经过 `app.ping` 校验。

宿主负责安全区、临时 viewport 设置及滚动样式，退出、重启、失败和应用撤销时自动复原，迟到请求不会重新隐藏导航。
插件仍必须提供可访问的返回与菜单入口；没有该能力时应保留宿主工具栏正常运行，而不是发送未知方法或阻止阅读。

### v2：分页、稳定引用与受控字节读取

| 调用 | 返回或行为 |
|---|---|
| `files.list({ path, cursor, limit }, options)` | 直属节点 `{ entries, next_cursor, has_more, path, total }`；默认 200、最多 500 项 |
| `files.searchPage({ under, q, kind, extensions, cursor, limit }, options)` | 子树搜索 `{ results, next_cursor, has_more }`；默认 200、最多 500 项，`extensions` 为字符串数组 |
| `files.stat({ id, content_version? } 或 { path }, options)` | 当前可见节点；不能同时传 ID 和路径 |
| `files.readRange(ref, offset, length, options)` | `Uint8Array`，偏移从 0 开始；单次 1 字节至 1 MiB |
| `assets.read(path, options)` | 当前插件包内资源的 `Uint8Array`，单资源最多 8 MiB，不接受 URL |

`options` 可传 `{ signal: AbortSignal }`。节点包含 `id`、`content_version`、`path`、`name`、`is_dir`、`size`、
`created_at`、`modified_at`、`favorite`；稳定内容引用为 `{ id, content_version }`。移动、重命名不改变内容版本，
覆盖上传的新内容不会误用旧引用。`files.stat` 和每次实际媒体读取仍检查节点可见性及应用当前修订版。

搜索 `under` 默认为 `/`，`kind` 为 `file`（默认）、`dir` 或 `all`，`q` 搜索文件名。
`next_cursor` 绑定目录、筛选条件和页大小；续页时保持原参数，只更换游标，切换条件则从第一页开始。
搜索按名称和节点 ID 稳定续读，不使用 OFFSET，也不受旧 `files.search` 的 2000 项限制。
`kind: 'all'` 且设置后缀时仍保留目录；所选取材目录是筛选条件，不是文件权限边界。

```js
const drive = window.tgdrive
await drive.ready
const controller = new AbortController()
const page = await drive.files.searchPage({ under: '/图书', extensions: ['txt', 'epub', 'pdf'], limit: 200 }, { signal: controller.signal })
// 在用户翻批时，用同样的筛选参数和 page.next_cursor 获取下一批。
if (page.results.length) {
  const file = await drive.files.stat({ id: page.results[0].id }, { signal: controller.signal })
  if (file.size) {
    const bytes = await drive.files.readRange(file, 0, Math.min(file.size, 1024 * 1024), { signal: controller.signal })
    console.log('首块字节数', bytes.length)
  }
}
// 切换内容或退出时取消在途读取。
controller.abort()
```

宿主使用既有单文件媒体票据和 HTTP Range 获取字节，再通过 MessagePort 转移缓冲区。
字节/资源读取另限每页 4 个并发；自带阅读器使用共享队列控制并发并取消离开视口的读取。
错误范围、过大响应、缺失或错误的 `Content-Range`、忽略 Range 返回整个文件的响应都会被拒绝，
不会回退为偷偷下载整本文件。应用不获得登录凭据或运行凭据。

### v2：应用私有存储与退出通知

| 调用 | 返回或行为 |
|---|---|
| `storage.get(key, options)` | `{ key, value, revision, updated_at }` 或不存在时 `null` |
| `storage.set(key, value, expected_revision = null, options)` | 条件写入并返回新记录；`null` 表示仅当键不存在时创建 |
| `storage.delete(key, expected_revision, options)` | 按修订标识删除，返回 `{ ok: true }` |
| `storage.list({ prefix, cursor, limit }, options)` | `{ records, next_cursor, has_more }`；默认 50、最多 200 条，按更新时间降序、键稳定续页 |
| `on('beforeClose', async callback)` | 宿主主动离开时等待保存，最多 2 秒，随后关闭页面并中止请求 |

存储自动绑定当前应用，不能传入其他应用 ID。键使用英文字母、数字、冒号、短横线、下划线，最多 128 字节。
每应用最多 10000 条、合计 8 MiB，单条 JSON 最多 32 KiB；设置偏好、进度和每个书签应分开存储。
`revision` 为不透明随机标识，`updated_at` 为服务端 Unix 秒时间戳。

```js
const key = 'progress:123'
const previous = await drive.storage.get(key)
try {
  await drive.storage.set(key, { page: 12 }, previous?.revision ?? null)
} catch (error) {
  if (error.code === 'storage_conflict') {
    // 让用户决定采用已保存内容还是保留当前内容，不自动无条件重试覆盖。
    document.getElementById('content').textContent = '另一设备更新了记录，请选择要保留的进度。'
  } else throw error
}
```

写入事务同时校验应用启用状态和修订版；旧标签页或过期记录会被拒绝。更新、停用和普通卸载保留数据，
清除数据卸载才删除。自带阅读器仅在恢复位置完成后允许保存；正常阅读最多每 5 秒保存，
翻页、书签操作、切书与主动退出及时保存。冲突不简单取最大页码，用户可以主动回读。
强制关闭浏览器只保证最近一次成功保存，不承诺离线同步或关闭后继续上传。

## 图书的移动阅读与更新

图书 `1.1.3` 在窄屏或主指针为触摸的设备上启用沉浸阅读，内容库仍显示普通导航。
正文默认上下滚动；TXT/EPUB 可切到单列分页，选择保存在图书的私有偏好中，不改变漫画偏好。
分页只排当前章节／文本分段，底栏展示本节页码；目录与章节／分段跳转不冒充全书显示页码。
EPUB 的 spine 是内容顺序，同一文档多个目录 hash 仅作为定位目标。旧 `entry`、字符偏移与书签继续有效。

轻点正文中央唤出／隐藏菜单；菜单隐藏时，分页模式两侧点按或明显横向滑动翻页。纵向滚动、选字、长按、链接和双指操作不翻页。
目录、设置、书签采用互斥面板，不改变正文尺寸；字号、模式、窗口宽高及图片加载变化后按逻辑位置恢复。
更多菜单提供目录设置、原文件下载和返回网盘；`1.1.3` 修复该菜单被底部分页栏遮挡的问题。Esc 和键盘菜单入口可恢复操作栏，错误及同步冲突不会静默隐藏。
PDF 使用实际页和适宽缩放，放大后的平移不触发翻页；普通桌面保留常驻导航，不提供浏览器原生全屏按钮。

先更新宿主，再主动更新已安装的图书包，才能获得完整的网页内沉浸效果。旧宿主能力缺失时仅保留外层导航。
图书 `1.1.2` 修复 iOS 主屏幕阅读中状态栏与正文重叠、底部露出宿主背景的问题：沉浸宿主固定在视口，统一扣除四边安全区，
插件以 `html/body/#app` 的百分比高度填满 iframe，不独立计算 `100dvh`。安全区随明亮、护眼和深色主题同步背景。
漫画更新到 `1.0.3`，统一移动触控和安全区，阅读器使用宿主 iframe 的实际高度；短视频 `1.0.2` 使用已支持的 API v2 沉浸能力，提供可见退出按钮，滑动只在播放区域生效并忽略控件、多指和取消事件。
安全区统一由宿主处理，iframe 内不重复加设备边距；主站固定启用 `viewport-fit=cover`，退出沉浸恢复进入前配置而不是删除该设置。
更新不自动安装应用，不清理现有设置或阅读记录，本轮移动适配无新增数据库迁移。主站体验和验证见 [移动端体验说明](mobile-experience.md)。

## 影视应用

影视 `1.1.0` 提供命名媒体库、库内大封面、详情选集、收藏、历史、续播、字幕和移动沉浸。用户在插件内创建媒体库并绑定一个文件夹（包含子文件夹），进入该库后才加载视频；首页不扫描整个网盘。旧 `source_dir` 不再自动启用，升级后需手动建库。只读取网盘文件与本地海报，不接外部影视源或自动刮削。

使用浏览器原生播放及纯 JavaScript 客户端转封装；首版必测 MKV H.264＋AAC → 分片 MP4／MSE。不做服务器转码或 WASM 重编码，不能解码的轨道明确提示下载原文件。支持外挂 SRT/VTT/ASS 和 MKV 内嵌文本字幕，ASS 仅显示文本，不支持图片字幕烧录。

仅申请 `files.read`、`media.read`，媒体库配置、收藏和观看进度保存在插件私有存储。收藏和历史只显示有效媒体库内的视频，库外记录仅隐藏、不会自动删除，重新加入对应目录后可以恢复；详情和播放操作会再次校验当前路径。既有包限制、沙箱同源隔离和禁止联网策略保持不变，媒体库功能无需新增宿主 API 或数据库迁移。保存与搜索不依赖原生表单提交，不增加 `allow-forms` 权限。首次使用需先更新主站并主动安装影视，已安装用户需在应用中心主动更新插件包。

详细安装、海报命名、格式边界、缓存限制和验证见 [影视应用说明](cinema.md)。

## 图书和漫画的格式与资源边界

| 格式 | 阅读方式与边界 |
|---|---|
| TXT | 完整正文最多 64 MiB；BOM、UTF-8、UTF-16、GB18030/GBK，支持手动切换；常见章节与分段导航，滚动／单列分页 |
| EPUB | 无 DRM 的 EPUB 2/3，最多 128 MiB；书名、作者、spine、目录、内部链接及包内位图插图；滚动／单列分页统一重排，不保留任意原书 CSS |
| PDF | 最多 512 MiB、10000 页；实际 Canvas 页面、目录、翻页、跳页和缩放；包内 PDF.js 主线程解析及字体/CMap |
| 图片目录 | 直属 JPG/JPEG、PNG、WebP、GIF、AVIF、BMP 组成一章，最多 10000 页；子目录保持分章 |
| CBZ/ZIP | 一包一册，最多 2 GiB；按相对路径自然页序收集位图，忽略隐藏和非图片条目，不递归读取内嵌包 |

归档最多 10000 条、索引最多 8 MiB、声明解压总量最多 2 GiB，单条目最多 32 MiB、XML/HTML 最多 8 MiB、
压缩比最多 200:1。只接受 Stored/Deflate，支持这些边界内的 ZIP64；拒绝加密、分卷、符号链接、
非法或重复路径及 CRC 错误，并校验实际解压大小。任何超限都会报错，不静默截断或解压写回网盘。

单张图片最多 32 MiB、3200 万像素，解码前检查尺寸；SVG 不作为漫画页面执行或内联显示。
文件块缓存最多 32 MiB，图片窗口解码预算 128 MiB，只读取视口附近内容。
TXT/EPUB 挂载当前文本窗口或章节，漫画连续模式挂载相邻最多五页；切换、退出时取消读取、回收 Blob URL 和画布，
旧加载和排版回调不能覆盖新内容。

EPUB 清洗移除脚本、事件、表单、任意样式和外部资源；内部资源必须落在归档根目录内。
PDF 不使用浏览器内置 iframe，不启用 WASM、XFA 或自动整本预取，CSP 不允许 eval。
沙箱仍为 `allow-scripts`，不新增同源权限、网络代理或外部连接权限。

首版不支持 MOBI/AZW3、CBR/RAR/7Z、DRM/密码保护内容、固定版式或交互式 EPUB；
不做 OCR、朗读、批注高亮、正文全文检索、双页漫画、离线缓存或自动转换。

## 主程序接口

安装管理接口均要求主站登录。导入采用两次请求，先查看清单，再对同一份包确认安装。

| 接口 | 说明 |
|---|---|
| `GET /api/apps` | 已安装记录、目录快照、接口版本；远程模式不等待联网 |
| `POST /api/apps/inspect` | 请求体为包字节，返回清单、SHA-256、已有版本、新增权限 |
| `POST /api/apps/install?digest=...&expected_revision=...` | 请求体为原包字节；首次安装省略旧修订版 |
| `POST /api/apps/catalog/refresh` | JSON `{ "force": false }` 为普通检查，`true` 为手动刷新；始终返回含目录状态的完整列表 |
| `GET /api/apps/catalog/:id/package?digest=...` | 按可选摘要下载精确包；省略时选择最高兼容版本 |
| `POST /api/apps/catalog/:id/install` | JSON 包含 `digest` 和可选 `expected_revision` |
| `POST /api/apps/:id/state` | JSON 中设置 `enabled` 或 `pinned` |
| `DELETE /api/apps/:id?purge_data=false` | 卸载，默认保留设置和私有数据；显式清除时一并删除阅读记录与书签 |
| `GET/PATCH /api/apps/:id/settings` | 获取或部分更新应用设置 |
| `POST /api/apps/:id/launch` | 为宿主创建受限运行会话 |
| `POST /api/apps/:id/rpc` | 宿主能力网关，应用使用 SDK 调用 |

目录状态 `catalog` 包含 `source`（`github`/`local`/`disabled`）、`repository`、`state`（`ready`/`stale`/`unavailable`/`disabled`）、
`last_success_at`（Unix 毫秒或 null）和 `error`。可安装条目在原有 `manifest`、`digest`、`update_available` 外提供 `size`、`source`、`source_url`。
远程失败只影响目录状态，列表仍返回已安装应用；目录查询、刷新、下载和安装全部要求主站登录。
接口不会因为前端隐藏按钮就放行未安装、已停用或权限不足的应用。

## 验证

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm --prefix frontend ci
npm --prefix frontend run check
npm --prefix frontend test
npm --prefix frontend run test:apps
```

宿主浏览器测试位于 `frontend/tests/apps/`，不需要插件仓库。需要 ffmpeg 和 Playwright Chromium，启动 `127.0.0.1:4187` 的独立测试网盘，
仅使用临时数据库、合成插件/视频/文档与内存 Telegram，不加载 `.env`、生产目录配置或真实网盘。
保留独立导入、权限确认、更新撤销、启停、侧栏、保留/清除数据卸载、手机设置/401 导航和 PWA 缓存回归。
可用 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定已有浏览器，`PLAYWRIGHT_APPS_OUTPUT_DIR` 指定截图和失败 trace；
若临时目录配额不足，可将 `TMPDIR` 与测试输出指向有空间的普通磁盘目录，不通过跳过断言解决。

固定本机夹具命令为 `cargo test --locked apps::tests::browser_fixture -- --ignored --exact --nocapture`，仅在测试构建中存在。
夹具身份接口 `/__apps_fixture` 返回 `{ "fixture": "tgdrive-apps-tests" }`，账号 `apps-test`、密码 `apps-test-only`；测试必须先核验身份再写入。
`TGDRIVE_TEST_VIDEO` 接收合成 WebM；`TGDRIVE_TEST_FILES_DIR` 接收合成目录树（兼容旧名 `TGDRIVE_TEST_READERS`）；
`TGDRIVE_APP_CATALOG_DIR` 必须显式指向绝对包目录。使用前先构建宿主 `frontend/dist`；没有目录输入时默认禁用目录。
维护者发布后可单独设置 `TGDRIVE_TEST_GITHUB_REPO=lengyuesky/tgdrive-app`，在同一个临时数据库夹具内验证真实远程目录；
若同时指定本地目录，则本地模式优先。该开关仅存在于 ignored 测试夹具，普通测试不读取这些环境变量，也不会默认联网。

真实短视频、图书、漫画、影视的业务回归由插件仓库维护，本机通过显式 `TGDRIVE_HOST_DIR` 连接已构建的私有宿主完成集成验证，
不能默认猜测相邻仓库。每次发布记录真实验证的完整宿主提交，不将宿主源码、产物或 trace 上传到公开插件 CI。
没有私有宿主权限的贡献者可运行插件独立检查和单元测试，但不能将其宣称为跨仓库集成通过。
触摸模拟不能替代 iOS Safari／Android Chrome 真机验收。插件测试夹具和业务验证说明见[插件仓库](https://github.com/lengyuesky/tgdrive-app)。
