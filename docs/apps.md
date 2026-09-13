# 页面应用开发说明

应用中心管理可独立发布的页面应用。主程序负责安装、权限、登录、设置和文件访问；应用包只包含
编译好的 HTML、JavaScript、CSS 等资源，不在服务器上执行脚本。内置目录提供短视频（`apps/shorts/`）、
图书（`apps/books/`）、漫画（`apps/comics/`）和影视（`apps/cinema/`）四个独立安装包；插件页面不依赖主站的 Vue 页面或内部组件。
图书、漫画只读取网盘文件，不接外部书源，也不修改文件页原有预览与菜单。

## 使用与生命周期

在文件页侧边栏「分享管理」下方的应用区块打开「应用中心」，默认展示「已安装」。未安装任何应用时也保留此标签，
可通过「浏览可安装应用」切换；整页重载重新选择已安装，页面内刷新保留当前选择。可以安装应用目录中的包，或选择「导入插件包」。确认安装前会展示作者、
版本及全部权限；更新时另外标出新增权限。导入包的作者字段是包内声明，不表示官方认证。

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

使用 Node.js 22.22 或更高版本：

```bash
node apps/package.mjs /绝对路径/我的应用/dist /绝对路径/输出目录
```

工具会自动加入 `apps/sdk/tgdrive-sdk.js`，生成 `<ID>-<版本>.tgapp`。
应用源目录不要再放同名 SDK 文件。后端安装时还会执行完整的包、清单及兼容性校验。

主项目默认构建四个插件，也保留传入独立静态目录的用法：

```bash
npm --prefix frontend run build:apps
npm --prefix frontend run build:apps -- /绝对路径/我的应用/dist /绝对路径/输出目录
```

默认输出 `apps/catalog/`，可通过 `TGDRIVE_APP_BUILD_OUTPUT` 覆盖。图书和漫画的 TypeScript 源码需要先编译，
默认构建会完成编译、复制依赖许可及 PDF CMap/标准字体资源，再分别打包。`.build/` 与 `catalog/` 都是生成产物。
前端构建和 Docker 构建同样生成四份包，不依赖服务器上的解压、PDF 转换服务或系统字体。

App 的实现集中在根目录 `apps/`：`frontend/` 为应用中心、侧栏与运行页面，`backend/` 为安装、
权限和持久化，`shorts/`、`books/`、`comics/`、`cinema/` 为四款插件，`reader/` 为共享阅读逻辑，
`sdk/` 为插件 SDK，`tests/browser/` 为浏览器验证。
前端通过 `@apps` 引用应用代码，应用通过 `@drive` 使用主站能力；后端通过模块路径接入，
数据库迁移仍由主程序统一执行。

可以把包文件分发给用户，由用户在应用中心导入；更新时提高 `version` 后重新打包。
宿主已支持目标接口版本时，主程序无需重新发布。默认应用目录包含随发行版提供的四款安装包，
新部署及升级已有网盘都需要用户主动选择安装。

部署者也可以通过 `TGDRIVE_APP_CATALOG_DIR` 指向一个可信的服务器目录，把 `.tgapp` 文件放入其中，
它们就会出现在可安装列表。同一 ID 展示最高兼容版本。默认查找工作目录中的 `app-catalog/`
或 `apps/catalog/`；Docker 镜像中的默认目录为 `/app/app-catalog/`。
第一版不提供公共在线市场服务，第三方包通过导入或部署者维护的目录分发。

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
| `GET /api/apps` | 可安装目录、已安装记录和接口版本 |
| `POST /api/apps/inspect` | 请求体为包字节，返回清单、SHA-256、已有版本、新增权限 |
| `POST /api/apps/install?digest=...&expected_revision=...` | 请求体为原包字节；首次安装省略旧修订版 |
| `GET /api/apps/catalog/:id/package` | 下载目录中的插件包 |
| `POST /api/apps/catalog/:id/install` | JSON 包含 `digest` 和可选 `expected_revision` |
| `POST /api/apps/:id/state` | JSON 中设置 `enabled` 或 `pinned` |
| `DELETE /api/apps/:id?purge_data=false` | 卸载，默认保留设置和私有数据；显式清除时一并删除阅读记录与书签 |
| `GET/PATCH /api/apps/:id/settings` | 获取或部分更新应用设置 |
| `POST /api/apps/:id/launch` | 为宿主创建受限运行会话 |
| `POST /api/apps/:id/rpc` | 宿主能力网关，应用使用 SDK 调用 |

接口不会因为前端隐藏按钮就放行未安装、已停用或权限不足的应用。

## 验证

```bash
CARGO_TARGET_DIR=/tmp/tgdrive-codex-target cargo test --locked
npm --prefix frontend run check
npm --prefix frontend test
npm --prefix frontend run test:apps
```

浏览器测试需要 ffmpeg 和 Playwright Chromium，会启动监听 `127.0.0.1:4187` 的独立测试网盘，
使用临时数据库、合成视频/TXT/EPUB/PDF/漫画和内存 Telegram 实现，不加载项目 `.env` 或读取真实网盘。
可通过 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定本机 Chromium 或 Chrome 可执行文件，
通过 `PLAYWRIGHT_APPS_OUTPUT_DIR` 保存截图和失败 trace。覆盖沙箱首次点击、实际阅读、跨浏览器同步/冲突、
自然页序、手机触摸分页与浮层、横竖屏锚点、纯图片进度、旧宿主降级、归档拒绝和退出取消，短视频及 v1 生命周期同时回归。
触摸模拟不能替代 iOS Safari／Android Chrome 的安全区、地址栏、软键盘、选字和双指缩放真机验收。
中文 PDF 已附合成夹具；正常测试不依赖 Python，重新生成方法见 [apps/README.md](../apps/README.md)。
