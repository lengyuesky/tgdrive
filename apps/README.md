# App 功能目录

应用相关实现集中在本目录。手机通过底部「应用」进入应用中心；文件页侧边栏「分享管理」下方仍提供应用中心和已固定 App 的入口。
应用中心默认显示「已安装」；没有安装记录时通过「浏览可安装应用」进入目录。短视频、图书、漫画、影视分别发布为独立 `.tgapp` 包，默认都不安装、不固定入口。

影视 `1.1.0` 由用户在插件内创建命名媒体库并绑定文件夹（含子文件夹），进入该库后才加载视频，首页不自动扫描网盘。旧版单目录设置不自动启用，更新后需手动建库；收藏和历史只显示库内视频，库外记录保留但隐藏。格式兼容、海报／字幕命名、数据保存和资源限制见 [影视应用说明](../docs/cinema.md)。只做客户端转封装，不重新编码，不增加服务器 FFmpeg 依赖。

## 目录

| 路径 | 内容 |
|---|---|
| `frontend/` | 应用中心、沙箱宿主、侧栏、设置、接口客户端、消息通道及测试 |
| `backend/` | 包校验、安装更新、权限网关、运行凭据、应用数据与测试 |
| `shorts/` | 短视频插件，声明 API v2，支持移动沉浸 |
| `books/` | TXT、EPUB、PDF 图书插件，声明 API v2 |
| `comics/` | 图片目录、CBZ/ZIP 漫画插件，声明 API v2 |
| `cinema/` | 深色影院、命名媒体库与按需加载、续播、字幕、MKV 客户端流式转封装，声明 API v2 |
| `reader/` | 阅读器共用的内容库、Range 缓存、归档、图片窗口、定位、同步与样式 |
| `sdk/` | 打包时加入的页面 SDK，以及阅读器使用的公开类型 |
| `tests/browser/` | 真实沙箱浏览器测试和临时网盘夹具 |
| `tests/fixtures/` | 合成中文 PDF、字体许可证及可选的再生成脚本 |
| `build.mjs` | 默认编译并打包四款插件，也接受已编译静态目录 |
| `package.mjs` | 不依赖前端框架的标准 ZIP 打包工具 |
| `.build/`、`catalog/` | 中间静态资源和最终包，不提交版本控制 |

数据库迁移仍放在主项目 `migrations/`；应用私有数据使用 `0015_app_data.sql`，由主程序按现有方式迁移。

## 使用图书和漫画

从应用中心安装后，在「应用设置」中分别选择图书目录或漫画目录，默认 `/`。从插件内选书、选章节，
不改变文件页原有预览与菜单，也不接入外部书源。

- **图书**：按文件名搜索所选目录及子目录；TXT 读取完整正文，支持 UTF-8、UTF-16、GB18030/GBK 和手动编码切换；
  EPUB 2/3 显示书名、作者、章节、目录与包内位图插图；PDF 提供实际页面、目录、翻页、跳转及缩放。
- **阅读设置**：TXT/EPUB 支持上下滚动、单列分页、字号、行距、正文宽度和主题，默认滚动、18px、1.8 行距、跟随主站主题。
  分页只排版当前章节或文本分段，页数随屏幕与字号变化；进度仍保存字符位置，不保存易变的显示页码。
  前后翻页自动跨当前文本分段或 EPUB 内容文档，不重复同一文档中的多个目录锚点。
- **手机阅读**：图书打开后隐藏宿主和插件常驻导航，正文铺满网页可视区，保留安全区和左右 12px 阅读边距。
  轻点中央显示或隐藏菜单；分页时轻点两侧或横向滑动翻页，滚动模式保留原生纵向滚动。选字、内部链接、双指缩放不触发翻页。
  目录、设置、书签是互斥浮层，不挤压正文；更多菜单提供下载、目录设置和返回网盘。Esc 关闭面板或显示菜单，Tab 也可唤出菜单。
  PDF 保持实际页和适宽缩放；放大后允许页内平移，不抢占拖动。这里是网页内沉浸，不请求浏览器原生全屏。
- **漫画目录**：直属图片是一章，子目录保留为独立章节，进入后点击「阅读本目录图片」。
  CBZ/ZIP 是一册，图片按包内路径自然排序，例如 `1、2、10`，不展开内嵌压缩包。
- **漫画模式**：默认上下连续滚动，也可单页翻阅；单页支持左右阅读方向、按钮、键盘、触摸和页码跳转。
  到末页不循环，也不自动下载下一章。
- **记录**：最近阅读、继续阅读、命名书签和阅读偏好保存在当前网盘实例的数据库中，手机与电脑共用。
  冲突时选择「使用已保存进度」或「从当前页继续并保存」，不会自动以旧页覆盖新页。
- **卸载**：默认保留设置、进度、书签；勾选「同时清除这个应用的数据」才删除。网盘文件和收藏不受影响。

格式、资源限制、隔离边界及 SDK 接口见 [页面应用开发说明](../docs/apps.md)。

## 阅读器升级

当前移动适配版本为图书 `1.1.3`、漫画 `1.0.3`、短视频 `1.0.2`。
图书 `1.1.3` 修复手机阅读中“更多”菜单被底部分页栏遮挡的问题，下载、目录设置和返回网盘操作可完整显示。
图书和漫画统一触控尺寸、输入字号与宿主安全区，阅读器高度只取 iframe 可用区域。图书的 iOS 主屏幕沉浸固定到视口，安全区底色跟随阅读主题，避免状态栏压字和底部露底；需先更新宿主，再在应用中心主动更新图书包。短视频使用已有 API v2 的沉浸能力，保留明确退出入口，滑动忽略控件、多指和取消事件。
安全区只由宿主承担，插件填满宿主提供的可用区域，不再重复留白；退出沉浸恢复进入前的视口配置，主站始终保留 `viewport-fit=cover`。
先更新主程序，再在应用中心的「已安装」中主动更新对应插件包；主程序构建不会自动替换已安装的插件。
旧宿主未声明 `ui.setImmersive` 能力时仍能阅读和分页，只保留外层宿主工具栏。原有设置、书签、进度无需迁移。

主站以 767px 区分手机布局；阅读器也根据粗指针启用触控布局，涵盖手机横屏，普通桌面保留常驻导航。主站四标签、传输和主屏幕说明见 [移动端体验说明](../docs/mobile-experience.md)。
交互与布局参考 [Kavita 的 EPUB 阅读说明](https://wiki.kavitareader.com/guides/readers/epub/)，未引入其程序或依赖。

## 构建与独立打包

使用 Node.js 22.22 或更高版本。依赖和 lockfile 位于 `frontend/`，运行服务器不需要 Node.js。

```bash
npm --prefix frontend ci
# 编译前端及四款独立插件；不安装到正在运行的网盘。
npm --prefix frontend run check
# 只构建四款插件，输出到 apps/catalog/。
npm --prefix frontend run build:apps
```

也可通过 `TGDRIVE_APP_BUILD_OUTPUT` 指定包输出目录。图书依赖的 PDF 解析模块、字体和 CMap 以及依赖许可证随包分发，
运行时不从 CDN 下载。保持现有包大小和数量限制，不为阅读器放宽安装校验。

已编译的静态应用可单独打包；以下两种方式兼容原用法：

```bash
node apps/package.mjs /绝对路径/我的应用/dist /绝对路径/输出目录
npm --prefix frontend run build:apps -- /绝对路径/我的应用/dist /绝对路径/输出目录
```

源目录不能包含 `tgdrive-sdk.js`，打包工具会自动加入。不要把包含 TypeScript 的 `books/`、`comics/` 或 `cinema/` 源码目录直接当成静态成品打包；
默认构建已为它们完成编译。Docker 前端阶段同样使用相邻的 `frontend/`、`apps/` 目录并生成四份包。

## 隔离验证

```bash
CARGO_TARGET_DIR=/tmp/tgdrive-codex-target cargo test --locked
npm --prefix frontend run check
npm --prefix frontend test
npm --prefix frontend run test:apps
```

浏览器测试需要 ffmpeg 和 Playwright Chromium。可通过 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定已有 Chrome/Chromium。
测试服务仅监听 `127.0.0.1:4187`，使用临时数据库、内存 Telegram 和合成文件，不加载项目 `.env` 或真实书籍。
移动阅读用例使用触摸浏览器上下文和实际触摸事件，覆盖纯正文、浮层、分页、旋转、字符锚点、纯图片进度及旧宿主降级；仍需真机检查 Safari 安全区、地址栏与软键盘。
`PLAYWRIGHT_APPS_OUTPUT_DIR` 可指定截图、失败 trace 等产物目录。

如果 `/tmp` 使用带用户配额的 tmpfs，`df` 显示尚有总剩余空间并不保证当前用户还能写入。出现 `EDQUOT`／`-122`、浏览器页面崩溃或夹具资源写入失败时，先检查配额，再将临时文件和测试产物放到有足够空间的普通磁盘目录；不要通过跳过断言解决。可沿用上面命令产生的 Cargo 构建缓存：

```bash
mkdir -p "${XDG_CACHE_HOME:-$HOME/.cache}"
test_tmp=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/tgdrive-apps-test.XXXXXX")
TMPDIR="$test_tmp" CARGO_TARGET_DIR=/tmp/tgdrive-codex-target \
  PLAYWRIGHT_APPS_OUTPUT_DIR="$test_tmp/results" \
  PLAYWRIGHT_CHROMIUM_EXECUTABLE=/路径/chrome \
  npm --prefix frontend run test:apps
```

本轮影视与完整应用验收结果见 [影视应用说明](../docs/cinema.md#本轮验收2026-09-13)。测试目录只用于保存本轮临时文件与结果，不读取真实网盘，也不需要修改生产配置。

正常验收不需要 Python。如需重新生成中文 PDF 夹具，优先使用 uv 环境，并提供本地 Noto CJK 字体：

```bash
uv run --no-project --with fpdf2 --with fonttools apps/tests/fixtures/make_pdf.py /路径/NotoSansCJK-Regular.ttc
```

该可选命令只重建合成 PDF；字体许可见 `tests/fixtures/FONT-LICENSE.txt`。
