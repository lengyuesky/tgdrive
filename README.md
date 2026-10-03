# tgdrive

用 Rust 编写的个人网盘,把 **Telegram 作为存储后端**:文件分块后以 document 形式发到你的私有频道,本地只保存元数据(SQLite)。自带 Web 界面与 WebDAV 服务,Docker 一键部署。

```
浏览器 SPA ──┐                             ┌─> SQLite(元数据:目录树 / 分块映射)
             ├─> axum HTTP 服务(单进程)────┤
WebDAV 客户端─┘   ├ /api/*  REST+SSE       └─> Telegram Bot API
                  ├ /dav/*  WebDAV(Class 2)      ├ 官方 api.telegram.org(分块≤18MB)
                  └ /       Vue 3 前端            └ 自部署 telegram-bot-api(分块可达 GB 级)
应用插件(沙箱 iframe)─ 控制面: MessagePort RPC；数据面: 宿主签发的单文件票据直连 Range 读取
```

## 特性

- **大文件不受单文件限制**:自动分块上传/拼接下载,官方 Bot API 也能存任意大小文件
- **双端点**:官方 `api.telegram.org` 开箱即用;可切换自部署 `telegram-bot-api`(单块上限 2000MB，默认使用更稳妥的 256MB 分块；local 模式直读磁盘,速度更快)
- **网络代理**:在设置中启用 HTTP / SOCKS5 代理，支持账号密码；tgdrive、项目内自建 Bot API、MTProto 流式下载、收件和远端备份共用，保存后无需重启容器
- **Web 界面**:目录树侧栏、列表/网格/大图/时间线四种视图、拖拽上传(带进度)、下载、重命名、移动(目录选择器)、复制、图片/视频/音频/文本预览、暗色模式、在线主屏幕应用
- **手机与 iPhone**:文件／传输／应用／我的四标签、触控列表、底部操作面板、安全区与软键盘适配；前台切页继续上传，支持添加到主屏幕
- **断点续传**:多分块大文件固定交给默认 Bot 串行上传,上传期间其他任务暂缓并在完成后自动继续;支持单项或一键批量暂停、继续、重试和取消;刷新页面或服务重启后重新选择原文件并通过 SHA-256 校验,只补传缺失分块
- **秒传**:上传前先算 SHA-256,库里已有同内容文件直接零流量落位;上传文件夹、Ctrl+V 粘贴截图均支持
- **分享链接**:对文件/目录生成免登录直链,可选提取码、有效期、下载次数限制
- **回收站**:网页删除时可选择移入回收站或直接删除;WebDAV 删除始终进回收站,超过保留天数自动彻底清理
- **高级搜索与收藏**:按名称、文件/目录、文件类别或后缀、大小、修改时间和收藏状态组合筛选,支持排序;文件与目录均可收藏并从侧栏集中查看
- **Bot 收件**:白名单用户私聊 Bot 转存与存储频道直发入库可分别开关;多档视频自动选择最高可用清晰度;可把视频、图片、书籍分别存到指定目录,并自定义「文件后缀 → 目录」规则
- **多渠道数据库备份**:一键下载一致性快照;可同时备份到 Telegram、WebDAV 与 S3 兼容对象存储,各渠道独立滚动保留最近 7 份并单独重试失败项;可设置备份密码,快照先用 age 加密再离开本机
- **可靠后台任务**:Telegram 删除失败进入持久化重试队列;Bot 无法删除发出超过 48 小时的消息,这类任务直接跳过、不再重试;Bot 收件连续失败会进入死信,避免卡住后续消息
- **后台日志**:管理页「后台日志」可查看消息清理、维护与备份等关键后台事件的进度和失败重试;日志持久化保留最近 2000 条
- **WebDAV**:支持 rclone、Windows 资源管理器、macOS Finder(Class 2 LOCK)
- **零成本复制**:复制文件/目录只复制元数据,共享底层 Telegram 消息(引用计数,删除安全)
- **SQLite 唯一索引**:普通文件上传不向 Telegram 消息写入虚拟路径；重命名和移动只修改 SQLite 元数据，目录结构随数据库备份恢复
- **流式传输**:上传下载全程流式,内存占用与文件大小无关;>32MB 的分块经临时文件中转
- **零原片流量缩略图**:网页上传视频时从浏览器本地取帧,按内容哈希缓存小图;浏览文件列表不会读取原视频
- **单用户认证**:Web 登录(HttpOnly cookie)+ WebDAV Basic;登录密码以 argon2 哈希保存;可选 TOTP 两步验证(附一次性恢复码),启用后 WebDAV 与命令行 API 改用独立的应用密码
- **应用中心**:按需安装独立页面插件，支持版本更新、启停、常用入口和卸载；提供短视频、图书（TXT/EPUB/PDF）、漫画（图片目录/CBZ/ZIP）、影视（影院界面、续播、字幕及客户端 MKV 转封装），阅读／观看进度跨设备实时同步；每应用可配置目录范围，插件凭票据直连字节数据面，无服务器转码
- **后台管理**:网页上改 bot token、存储频道、端点、分块大小、账号密码、回收站保留期、收件白名单,保存即生效,不用重启容器

## 快速开始

### 1. 准备 Telegram

1. 找 [@BotFather](https://t.me/BotFather) 创建 bot,拿到 **bot token**
2. 新建一个**私有频道**作为存储空间,把 bot 加入频道并设为**管理员**(需要发消息和删消息权限)
3. 获取频道 ID(形如 `-1001234567890`):把频道任意消息转发给 [@userinfobot](https://t.me/userinfobot) 之类的工具即可看到

### 2. 部署(官方 Bot API,最简单)

`.env` 里**只有账号密码是必填的**,其余留空即可 —— 服务照常启动,登录后在网页里配置:

```bash
mkdir -p data
cp .env.example .env
# 编辑 .env:只需填 TGDRIVE_USERNAME / TGDRIVE_PASSWORD
docker compose up -d --build
```

镜像默认以非 root 用户 `1000:1000` 运行。`data` 目录必须对该用户可写;如果宿主机当前用户不是 `1000:1000`,请把 `.env` 中的 `TGDRIVE_UID` / `TGDRIVE_GID` 改为 `id -u` / `id -g` 的输出。先由当前用户创建 `data` 目录,可避免 Docker 自动创建出 root 所有的目录。

打开 <http://localhost:8080> 登录,页面会提示「还没配置 Telegram 存储」,点「去配置」填好 **Bot Token** 与**存储频道**,保存即生效(不用重启容器)。

当然也可以在 `.env` 里预先填好 `TGDRIVE_BOT_TOKEN` / `TGDRIVE_CHAT_ID`,启动后直接可用。

### 3. 用自建 Bot API 服务器(推荐大文件场景)

官方端点下载单块上限 20MB,所以分块只能 ≤18MB,而 bot 发消息到同一频道约有 20 条/分钟 的限流,大文件吞吐约 6MB/s。自建 `telegram-bot-api` 后分块可到 GB 级,轻松跑满带宽。

在 <https://my.telegram.org> 申请 `api_id` / `api_hash` 后，有两种配法。这两项也供
tgdrive 的 MTProto Range 下载使用：浏览器播放视频时每次只向 Telegram 请求最多
512KiB，关闭预览后停止后续请求，不需要改变已有的 GB 级 Telegram 分块。

自建服务位于 Compose 的 `local-bot-api` profile 中,不会随普通的 `docker compose up` 启动。

两个服务由同一份 Compose 管理，并共用 tgdrive 后台「网络代理」设置。无法直连 Telegram 时，只需在后台启用代理，或在 `.env` 设置 `TGDRIVE_PROXY_ENABLED=true` 和 `TGDRIVE_PROXY_URL`。首次升级到支持共享代理的版本，需要执行下面的构建命令重建两个服务；以后修改代理无需重建。

**A. 网页里配**(推荐):后台「自建 Bot API 服务器」填好 api_id / api_hash 并保存,点「导出 .env」把内容覆盖回项目目录的 `.env`,执行:

```bash
docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api
```

重建容器后,回「Telegram 存储」把「接口端点」切到**自建 Bot API** 再保存即可。

**B. 直接改 .env**:填 `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 与 `TGDRIVE_API_BASE=http://telegram-bot-api:8081`,再执行 `docker compose --profile local-bot-api up -d --build`。

> api_id / api_hash 会在 tgdrive 启动时用于 MTProto 会话，也由 telegram-bot-api 容器读取
> （compose 的 `${VAR}` 插值取自宿主机 `.env`）。所以这两项无论在哪改，都要先导出并
> 覆盖宿主机 `.env`，再重启/重建 tgdrive 与 telegram-bot-api。

compose 已把 telegram-bot-api 的数据目录(`./telegram-bot-api-data`)以相同路径只读挂到 tgdrive 容器(local 模式下 `getFile` 返回容器内绝对路径,tgdrive 自动探测并直读磁盘,失败才回退 HTTP)。默认还会把 Bot API 的文件组 `101` 作为 tgdrive 的补充组；若换用 UID/GID 不同的镜像，请把 `TGDRIVE_BOT_API_GID` 改为该镜像中文件所属的组 ID。

> 注意:bot 在官方与自建服务器之间迁移时,Telegram 要求先对旧端点调用 `logOut`,详见 telegram-bot-api 文档。

## 配置项

配置有三层,**后台修改 > 环境变量(`.env`)> 内置默认**。`.env` 里只有账号密码是必填的,其余留空就在网页「系统设置」里配。

回收站保留天数、备份渠道、Bot 收件规则、消息自动清理等维护设置没有对应的环境变量,只能在后台修改,保存在数据库里并随数据库备份。环境变量只在启动时读取一次,改 `.env` 后需重启容器。

| 变量 | 必填 | 默认 | 可在后台改 | 说明 |
|---|---|---|---|---|
| `TGDRIVE_USERNAME` / `TGDRIVE_PASSWORD` | ✔ | — | ✔ | Web 与 WebDAV 的账号密码;唯一必填项(没有它就没法登录后台配其余部分)。密码可写明文,也可写 `tgdrive hash-password` 生成的 argon2 哈希(须加单引号:`TGDRIVE_PASSWORD='$argon2id$...'`);后台改过的密码只以哈希存库 |
| `TGDRIVE_BOT_TOKEN` | | — | ✔ | bot token;留空则启动后在网页里配 |
| `TGDRIVE_UPLOAD_BOT_TOKENS` | | — | ✔ | 额外上传 Bot Token，逗号分隔；各 Bot 必须是同一存储频道管理员，主 Bot 也会参与上传池 |
| `TGDRIVE_CHAT_ID` | | — | ✔ | 存储频道 ID(`-100…`)或 `@频道名`;同上 |
| `TGDRIVE_API_BASE` | | `https://api.telegram.org` | ✔ | Bot API 端点 |
| `TGDRIVE_PROXY_ENABLED` | | `false`（仅配置代理 URL 时自动启用） | ✔ | 出站代理开关；关闭后直连，但保留已保存的地址 |
| `TGDRIVE_PROXY_URL` | | — | ✔ | HTTP / SOCKS5 代理 URL，例如 `http://proxy:7890`、`socks5h://user:password@proxy:1080` |
| `TGDRIVE_CHUNK_SIZE` | | 官方 18MB / 自建 256MB | ✔ | 分块大小,支持 `18MB`/`1g` 写法;官方端点上限 19.5MB(受 getFile 20MB 限制),自建上限 1900MB |
| `TGDRIVE_UPLOAD_CONCURRENCY` | | `2` | ✔ | 单个上传 Bot API 实例的并发数；总并发 = 此值 × 上传 Bot 数量 |
| `TGDRIVE_DOWNLOAD_CONCURRENCY` | | `4` | ✔ | MTProto 单个下载流的并行分段数，范围 1~8；「流式播放」中保存后对新请求生效。同一 Bot 的所有存储目标共享最多 8 路在途请求 |
| `TGDRIVE_SEND_VIDEO` | | 官方 `false` / 自建 `true` | ✔ | 视频消息模式:不超过分块大小的 mp4/mov/m4v 以「视频消息」发送(自动带宽高时长),Telegram 里可直接播放,文件内容不变;mkv/webm 仍走文档(Telegram 播放器不支持其常见编码);超过分块大小的视频仍按文档分块存储 |
| `TGDRIVE_TMP_DIR` | | 系统临时目录 | ✔ | 大分块暂存目录,需 ≥ 分块大小×(并发+1) 的空间 |
| `TGDRIVE_LOCAL_DATA_ROOT` | | `/var/lib/telegram-bot-api` | | 自建 Bot API local 模式允许读取的根目录;必须与 Compose 共享卷的容器内路径一致,修改需重启 |
| `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` | | — | ✔* | 在 <https://my.telegram.org> 申请；tgdrive 用于 MTProto Range 流式下载，自建 Bot API 也使用；\*后台可编辑并导出，修改后必须重启/重建两个服务 |
| `TGDRIVE_UID` / `TGDRIVE_GID` | | `1000` / `1000` | | Compose 中 tgdrive 进程的 UID/GID;应对 `./data` 有写权限 |
| `TGDRIVE_BOT_API_GID` | | `101` | | 自建 Bot API 数据文件的组 ID;tgdrive 以补充组身份只读 local 文件和缩略图 |
| `TGDRIVE_LISTEN` | | `0.0.0.0:8080` | | 监听地址(端口由 compose 映射,改需重启) |
| `TGDRIVE_DB_PATH` | | `/data/tgdrive.db`(镜像内) | | SQLite 路径(设置本身存在这个库里) |
| `TGDRIVE_SECRET` | | 自动生成并存库 | | cookie 签名密钥,**仅首次启动时作为初始值**,之后以库中值为准 |
| `TGDRIVE_COOKIE_SECURE` | | `false` | | 通过 HTTPS(含反向代理)访问时设为 `true`,为 Cookie 启用 Secure 并发送 HSTS 响应头;修改需重启 |
| `TGDRIVE_TRUSTED_PROXIES` | | 空 | | 反向代理的 IP 或 CIDR(逗号分隔)。仅来自这些地址的 `X-Forwarded-For` 会被采信,用于登录与提取码限速;不配置时反代后所有人共用一个限速额度;修改需重启 |
| `TGDRIVE_STATIC_DIR` | | `/app/static`(镜像内) | | 前端静态文件目录 |
| `RUST_LOG` | | `info` | | 日志级别 |

上传并发是每个上传 Bot API 实例的并发数；总并发为“设置值 × 上传 Bot 数量”。例如配置 3 个 Bot、并发设置为 1 时，总并发为 3；设置为 2 时，总并发为 6。单块文件按 Bot 冷却状态和轮询分配；需要多个 Telegram 分块的文件统一使用默认 Bot，并在浏览器队列中独占上传通道、单分块串行，完成后再恢复其他任务。

## 手机与 iPhone 主屏幕

手机底部提供 **文件／传输／应用／我的**。文件页右上角「+」用于上传照片或文件、新建文件夹；「视图」切换列表、网格、大图和时间线，「选择」进入批量操作。收藏、分享管理、回收站和系统设置集中在「我的」。

- 在 iPhone Safari 中打开网盘，点击 **分享 → 添加到主屏幕**；也可从「我的」查看操作说明。主屏幕模式使用独立窗口，不重复显示安装提示。
- 文件页只缓存一个实例，切换标签保留目录、视图和滚动位置；前台上传与文件夹扫描不依赖文件页是否打开，上传目标固定为发起时的目录。
- 「传输」管理上传任务；下载由浏览器或系统接管。退出登录或认证失效会终止本地上传和扫描、释放文件引用，不自动删除服务器续传任务；重新登录后按提示选择原文件。
- **在线优先**：不注册 Service Worker。缩略图与应用读取的媒体字节段可存入浏览器私有缓存，但每次使用都经服务器的 ETag 校验（304 再验证），不会缓存到任何第三方位置，也不跨会话长期保留。完全断网时不保证冷启动可用；iOS 锁屏、切后台或回收页面可能暂停上传，回到前台会核实服务器进度，不承诺原生 App 的后台上传能力。
- 优先适配 iOS/iPadOS 16.4 及以上。建议经 HTTPS 访问，系统分享与自动复制按浏览器能力提供；复制受限时显示可选文本。主屏幕模式不是原生安装包。

布局、验证命令和 iPhone 真机验收清单见 [移动端体验说明](docs/mobile-experience.md)。

## 应用中心

宿主 0.3.0 新增文件“打开方式”、应用内文件定位、首次目录授权、应用详情、更新前版本与数据恢复，以及扫描任务取消。使用与边界见 [应用平台说明](docs/app-platform.md)。

手机直接点击底部「应用」打开 `/apps`；桌面文件页侧边栏「分享管理」下方保留独立的应用区块，平板可从侧边栏按钮打开。
四款插件默认均未安装、未固定入口；在「可安装」中查看权限并安装，
在「已安装」点击卡片图标即可打开应用；配置、更新、固定入口、停用或卸载统一在「应用设置」标签中操作。
含「读取文件信息」权限的应用可在「应用设置 → 目录范围」把访问收窄到指定目录子树（默认全盘，逐次调用强制校验）。
阅读与观看进度、插件设置经服务端事件流跨设备实时同步：另一台设备写入后，已打开的页面会收到变更通知并按需刷新。
新版宿主向插件提供字节数据面（票据直连 Range 读取）与批量段读能力，旧宿主自动降级到消息通道。

| 插件 | 内容与功能 |
|---|---|
| 短视频 | 随机播放网盘视频，管理取材目录和收藏 |
| 影视 | 自建命名媒体库并绑定文件夹，进入后才加载视频；详情选集、收藏历史、续播与字幕，原生播放及 MKV 客户端流式转封装，不做服务器转码 |
| 图书 | 阅读完整 TXT、无 DRM 的 EPUB 2/3、PDF；章节与目录跳转、文本排版及主题、PDF 翻页和缩放 |
| 漫画 | 阅读图片文件夹及 CBZ/ZIP，自然页序；连续条漫或左右方向单页翻阅，支持键盘、触摸与页码跳转 |

影视 `1.2.3` 不自动扫描整个网盘；新增票据直连读取与跨设备媒体库、进度同步。在插件内新建媒体库，例如「电影」对应 `/影视/电影`（含子文件夹），进入该库后才加载视频；旧版取材目录不自动启用，升级后需手动建库。收藏和历史仅显示媒体库内的视频，库外记录保留但隐藏。支持同目录海报、外挂 SRT/VTT/ASS 与 MKV 内嵌文本字幕；可播编码取决于浏览器，无法解码时提供原文件下载。安装、命名建议、格式边界和验证见 [影视应用说明](docs/cinema.md)。

图书和漫画只读取现有网盘文件，不接在线书源。从插件内选择内容，不改变文件页原有预览与菜单。
两个插件分别设置内容目录（默认 `/`），支持文件名搜索、最近阅读、继续阅读和命名书签。
漫画目录的直属图片组成一章，子文件夹保留为章节，点击「阅读本目录图片」开始阅读。
升级前的短视频取材目录会保留，首次安装后自动采用；旧 `/shorts` 书签会引导到应用页面。

阅读进度、偏好和书签保存在当前实例的数据库中，手机、电脑共用；重命名或移动后按节点身份继续定位。
正常阅读最长每 5 秒保存，翻页或主动退出会及时保存。另一设备更新进度时会暂停覆盖并要求选择，
网络失败显示「未同步」并可重试；强制关闭浏览器只保证最近一次成功保存，不提供离线同步。

首版上限为 TXT 64 MiB、EPUB 128 MiB、PDF 512 MiB、CBZ/ZIP 2 GiB；图片章最多 10000 页，
单张最多 32 MiB、3200 万像素，归档另有解压和压缩比限制。超过限制明确报错，不静默截断。
不支持 MOBI/AZW3、CBR/RAR/7Z、密码或 DRM 保护内容、固定版式 EPUB，也不执行 EPUB 中的脚本和外部资源。

应用以独立的 `.tgapp`（ZIP）包发布。可导入本地插件包，或导入更高版本更新已安装应用；
宿主支持该包接口版本时，无需重新编译主程序、重建镜像或重启服务。图书、漫画需要支持 API v2 的宿主，
已有 API v1 插件继续可用。更新前展示新增权限，失败保留原版本。
多个应用有新版本时，工具栏出现「一键更新」入口：单次确认后逐个下载安装全部可更新应用并实时展示进度，单个失败不影响其余并汇总结果。
卸载默认保留设置、阅读记录和书签，勾选「同时清除这个应用的数据」才删除；网盘文件和收藏始终保留。
清单可选声明包内 SVG `icon`（≤256 KiB），安装后在应用中心、侧边栏和宿主工具栏展示；未声明或未安装时用字母图标。
运行资源按修订不可变缓存并支持 ETag 再验证，文本资源（含 SVG）在安装时预生成 brotli/gzip 变体、运行时零压缩开销直接发送，桌面端悬停图标可预取启动。
已安装记录进程内缓存、可见路径单条递归查询、文件事件带目录并按应用范围过滤且服务端合并去抖、批量 RPC 信封（`rpc.batch`）与 4 路并行批量段读，降低插件热路径的数据库与网络往返。
运行会话到期前自动续签、过期后单次透明重试，长时间阅读/观影不再中断；媒体票据在签发时的过期时间前有效，重新打开内容即获得新票据。

安装包、配置和阅读记录一起保存在 SQLite 中，随现有数据库备份恢复。运行资源缓存在数据库所在目录的
`apps/` 下，恢复数据库后自动重新生成。图书、漫画、影视的封面生成一次后存入数据库旁的独立封面库
`tgdrive-covers.db`（每应用上限 256 MiB，超出自动淘汰最久没看的封面），再次打开直接显示、不再下载原图或解压归档；
封面库不进备份，可在「应用设置」中查看占用并一键清理，卸载应用时一并删除。页面应用通过受控接口访问文件和媒体，不运行后端脚本或后台任务。

开发、打包和 SDK 用法见 [页面应用开发说明](docs/apps.md)。
官方插件（短视频、图书、漫画、影视）已拆分至独立插件仓库 [lengyuesky/tgdrive-app](https://github.com/lengyuesky/tgdrive-app)。
每次打开应用中心会在展示已安装列表后，后台检查 GitHub 仓库根目录的应用索引；只有用户确认安装或更新时才下载对应摘要的包，并校验 SHA256、实际大小和完整清单。摘要只证明内容完整性，不认证清单中的作者。
默认仓库为 `TGDRIVE_APP_GITHUB_REPO=lengyuesky/tgdrive-app`（仅接受合法 `owner/repo`），入口为该仓库主分支的 `catalog.json`（`https://raw.githubusercontent.com/<repository>/main/catalog.json`）。
非空 `TGDRIVE_APP_CATALOG_DIR` 优先切换为本地安装包目录；空值视为未设置。没有显式目录且 `TGDRIVE_APP_CATALOG_REMOTE_ENABLED=false` 时，仅发现旧本地 `app-catalog/` 或 `apps/catalog/`，否则禁用目录。关闭目录不影响导入或已安装应用。
目录缓存有效期 5 分钟，手动刷新也遵守至少 30 秒联网间隔；离线保留上次成功目录并显示过期状态。数据库旁 `app-catalog-cache/` 按仓库隔离，已验证包采用 128 MiB LRU 缓存，独立于已安装包和用户数据。出站请求使用后台当前代理，不读取系统代理。
宿主 Docker 仅编译宿主前端与后端，不编译、复制或下载插件。
宿主接口声明与固定本机集成夹具要求见 [compatibility.json](compatibility.json)。宿主保持私有；插件发布前必须本机集成验收，公开插件 CI 不检出宿主或上传其产物。

## 后台管理

手机从「我的」→ **系统设置**(`/admin`)；桌面仍可使用右上角用户名菜单。未保存的修改会在离开时二次确认。

- **系统状态**:bot、端点、分块、文件数与占用、元数据库体积、运行时长、待删除任务、收件死信与后台任务健康度
- **Telegram 存储**:bot token、存储频道、接口端点、分块大小、上传并发、视频消息模式
- **网络代理**:HTTP / SOCKS5 开关、地址与连接测试；代理认证信息脱敏展示
- **历史存储迁移**:系统设置的「后台任务」可让主 Bot 在 Telegram 服务端逐条复制其他 Bot 的历史消息，原子切换所有共享分块引用；支持暂停、失败重试和服务重启后继续，用户上传期间自动让路
- **MTProto 流式播放 / 自建 Bot API**：api_id / api_hash，以及切换到自建端点的操作步骤
- **账号**、**高级**(临时目录)、需重启才能改的项(监听地址、数据库路径等)
- **备份与回收站**:Telegram、WebDAV、S3 渠道可分别启用；WebDAV 支持 Basic Auth，S3 支持自定义端点、区域、桶、对象前缀、临时令牌和路径样式
- **Bot 收件**:独立控制 Bot 私聊收件和频道收件,两种入口共用默认、视频、图片、书籍目录;自定义类型名称、一个或多个文件后缀及目标目录。自定义规则优先,多段后缀按更具体的规则匹配;新收件记录保存来源,频道中已登记的上传消息不会重复入库

上表中标了「可在后台改」的项都能在这里改,**保存后立即生效,不需要重启容器**(api_id / api_hash 除外,见下)。改动写进 SQLite,优先级高于 `.env`;每项都标了来源(`来自 .env` / `后台已改` / `默认值`),点「恢复」即可退回 `.env` 或默认值。

- **安全网**:改 Telegram 相关配置时会验证 Bot 身份、目标会话以及频道/群组的管理员、发消息和删消息权限;验证失败就拒绝保存(可选择强制保存,给自建端点还没启动的场景用)。分块大小与端点的组合也会一并校验,例如切回官方端点时不允许保留 512MB 的分块
- **导出 .env**:把当前生效配置生成 `.env` 文本,方便同步回宿主机文件
- **维护**:设置多渠道数据库备份、Telegram 备份分块大小(默认采用官方 Bot API 安全值 18MB)、清理上传中断留下的残留、登出所有会话(轮换签名密钥)

> 改账号后所有设备的登录状态立即失效,使用登录密码的 WebDAV 客户端也要同步修改。

### 账号安全

- **密码哈希**:后台设置的登录密码以 argon2id 哈希存进数据库;旧版明文会在启动时自动迁移。「导出 .env」导出的也是哈希(带单引号,可直接使用)。
- **两步验证**:在「账号安全」中扫码绑定任意 TOTP 验证器(Google Authenticator、Microsoft Authenticator、1Password 等),绑定成功会显示 10 个一次性恢复码,并登出其他设备。之后网页登录需输入 6 位验证码,手机丢失时可用恢复码代替。
- **WebDAV 应用密码**:随机生成,只显示一次,用户名不变。启用两步验证后,WebDAV 与 `/api` 的 Basic 认证**不再接受登录密码**,只能用应用密码,避免借 WebDAV 绕过两步验证。
- **找回**:验证器和恢复码都丢失时，先 `docker compose stop tgdrive`，再运行 `docker compose run --rm tgdrive reset-2fa` 关闭两步验证，最后 `docker compose up -d tgdrive`。服务与写库命令共用实例锁，不能在服务运行时直接执行重置。
- 两步验证密钥、恢复码和应用密码与登录密码一样,不会写进离开本机的备份;从备份恢复后需要重新绑定。

**改配置前请注意:**

- **换 Bot、端点或存储频道**只影响之后开始的上传。每个分块会记录原 Bot/API/频道,历史文件仍按原目标读取和删除;请保留原 Bot Token、端点和频道权限,否则对应历史文件会不可用。
- **旧数据库首次升级**无法凭 Telegram 消息反推出每个历史分块的原目标,程序会把尚未关联目标的旧分块一次性绑定到升级时的当前配置。如果旧库曾经换过 Bot、端点或频道,请先备份数据库并人工核对。
- **api_id / api_hash 改完不会立刻生效**：tgdrive 在启动时读取它们建立 MTProto 会话，Compose 也把它们传给 telegram-bot-api。保存后要导出 `.env` 覆盖回项目目录，再执行 `docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api`。

### 网络代理（HTTP / SOCKS5 / usque WARP）

打开「系统设置 → 网络代理」，可以选择「自定义代理」或「usque（Cloudflare WARP）」。切换方式会保留原有自定义代理地址。

**使用内置 usque：** 镜像包含 [usque v4.2.1](https://github.com/Diniboy1123/usque)。选择 usque 后，首次点击「同意条款并初始化 usque」注册 WARP 设备，再打开代理开关并保存。初始化需要连接 Cloudflare 注册服务；隧道默认使用 HTTP/2，通过 TCP/443 连接 Cloudflare，以兼容 QUIC 数据传输受限的网络。网络支持 QUIC 时可在 `.env` 设置 `TGDRIVE_USQUE_TRANSPORT=quic`（UDP/443），然后重建 tgdrive 容器；切换传输方式无需重新注册 WARP。页面显示进程状态，「测试连接」会额外检查 WARP 隧道。进程异常退出会自动重启，连接失败不会回退直连。

注册配置保存在数据库同级的 `usque/config.json`（默认容器 `/data/usque/config.json`，宿主机 `./data/usque/config.json`），重建容器后复用。它包含私钥，请单独妥善备份；数据库备份不包含这个文件。也可在启用前将已有 usque 配置放入该路径，初始化按钮不会覆盖已有文件。usque 只监听容器内 `127.0.0.1:1083`，使用自动生成的认证信息，无需公开端口、TUN 或额外权限。直接运行 Rust 程序时，请自行安装该版本 usque，并通过 `TGDRIVE_USQUE_BIN` 指定二进制路径。

**使用自定义代理：** 填写地址并打开开关，点击「保存并生效」。支持：

- `http://主机:端口`：HTTP 代理；MTProto 使用 HTTP CONNECT 隧道，代理需允许连接 Telegram DC 的端口。
- `socks5://主机:端口`：SOCKS5，HTTP 请求的目标域名由 tgdrive 解析。
- `socks5h://主机:端口`：SOCKS5，目标域名交给代理解析，推荐使用；`socks://` 是此方式的别名。
- 认证格式为 `协议://用户名:密码@主机:端口`，用户名、密码中的 `@`、`:`、`#` 等特殊字符需 URL 编码。HTTP 默认端口为 80，SOCKS5 默认为 1080。

「测试连接」不保存配置。连接官方或远端 Bot API 时，使用当前表单中的代理；项目内自建 Bot API 的 Telegram 连接使用已保存的代理，新的代理在保存后生效。保存时会验证当前 Bot API 链路；失败不会覆盖配置，仍可确认后强制保存。关闭代理后保留地址，恢复直连；恢复代理方式或地址时会一并恢复方式、地址与开关，避免留下无效组合。

代理覆盖 tgdrive 发出的 Bot API 请求（含历史存储目标）、MTProto、Bot 收件和 WebDAV / S3 备份。已发出的 HTTP 请求和已打开的 MTProto 下载流继续使用原连接；新请求使用新配置，收件在下一轮轮询切换。HTTP 请求到 `localhost`、`127.0.0.1`、`::1` 和 `telegram-bot-api` 时保持直连。

项目内的 `telegram-bot-api` 也共用这份配置。它的 Telegram TCP 连接由 proxychains 转给 tgdrive 的内部认证入口，再由 tgdrive 使用当前 HTTP CONNECT / SOCKS5 代理连接 Telegram；关闭代理时由 tgdrive 直连。保存或恢复代理设置会断开 Bot API 的旧连接，由 TDLib 自动重连，在途上传或下载可能短暂停顿。代理失败时不会自动绕过代理直连。

内部入口不映射到宿主机端口，也不需要 `NET_ADMIN`、TUN 或 Docker socket。Compose 使用 `TGDRIVE_BOT_API_PROXY_LISTEN=0.0.0.0:1082`、`TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf` 和共享目录自动建立连接。配置文件只含启动时生成的内部认证密钥，不含上游代理凭据，且不进入镜像。tgdrive 重建后 Bot API 自动重新读取入口配置。请保留这两个环境变量和共享挂载，不要把该端口公开到公网；自行部署到项目外的 Bot API 不受此配置管理。

开发验证：构建两个项目镜像后运行 `node docker/test-shared-proxy.mjs`。测试创建独立的内部网络、空数据库和临时容器，验证代理切换、失败时不回退、恢复配置以及重启联动，结束后清理测试资源，不连接生产实例或 Telegram。

注意：
- 本项目 Compose 中的 `telegram-bot-api` 自动共用代理；其他外部 Bot API 服务及浏览器的网络设置需要分别管理。Bot API 原生 `--proxy` 只代理 webhook，不能替代这里的 Telegram 出站代理。
- Docker 中的 `127.0.0.1` 指容器本身。使用宿主机代理需填写容器可访问的宿主机地址，并让代理监听可访问的接口；不要向公网开放无认证代理。
- tgdrive 使用这里的显式设置，不读取系统 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`。也可在 `.env` 配置 `TGDRIVE_PROXY_PROVIDER`、`TGDRIVE_PROXY_URL` 和 `TGDRIVE_PROXY_ENABLED`，后台覆盖优先。
- 设置接口会隐藏代理用户名和密码，原样回传不会覆盖凭据；修改地址时需重新填写完整认证信息。数据库备份和「导出 .env」仍包含真实凭据，应妥善保管。

### 数据库备份恢复

#### 从 Telegram 频道恢复

在「系统设置 → 备份」中选择「从 Telegram 恢复」，可以：

- **探测频道备份**：全新部署在 `.env` 配置 `TGDRIVE_BOT_TOKEN`、`TGDRIVE_CHAT_ID`、`TELEGRAM_API_ID`、`TELEGRAM_API_HASH` 及网页登录账号密码后启动，即可直接查找频道里以前发送的 `tgdrive-backup-*.db` / `.db.age` 备份，无需旧数据库或手机账号登录。打开向导后需手动点击「探测频道备份」才开始。每批读取 100 个消息位置，找到最近 5 份完整备份即停止，列表最多显示 5 份；不足 5 份时探测到频道起点。按文件名组合非连续分块，缺块、重复或不一致的备份不能恢复。后台已保存的频道配置优先于 `.env`。
- **选择已记录的备份**：使用当前数据库保存的频道备份记录。
- **输入备份消息链接/ID**：适用于全新部署或本地记录丢失。先配置可访问该频道的 Bot、Bot API 端点及代理，再粘贴备份消息链接；也可填写频道 ID 和消息 ID。分块备份必须提供同一份备份的**所有分块消息**，消息号不需要连续。

频道探测使用 Bot 可调用的 MTProto 频道差分和按消息号读取接口，只读取消息信息，不发送、转发或删除消息，也不把探测结果加入自动备份清理队列。未配置 API ID / Hash 时仍可手动提供消息链接。选择备份并开始下载预检时，会把指定消息**临时转发回同一频道**，下载完成后仅清理本次产生的临时消息，不删除来源备份。开始前需要确认此行为；Bot 必须能访问频道并发布消息，禁止转发的频道不支持这种方式。没有删除权限时，临时消息可能需要手动清理。

向导会下载、按分块编号合并、解密（如有）并校验 SQLite 数据库。加密备份需要备份时的密码，与登录密码不是同一个用途。文件不完整、密码错误、数据库损坏或版本比当前程序新时，不能确认恢复。旧版备份没有内容签名，只应导入自己信任的频道备份。

单次最多 128 块，密文和解密后的数据库分别不超过 8 GiB；准备任务与已就绪候选各有 30 分钟时限。暂存、试迁移和回滚都需要额外磁盘空间。age 工作因子上限为 20，密钥派生峰值内存约 1 GiB；超过此限制的旧备份需在资源足够的受控环境使用外部 `age` 离线解密后手动恢复。进程被强制终止或 Telegram 响应丢失时，临时转发可能残留，请按提示手动清理，不要删除原备份。

**确认恢复会覆盖当前文件索引和后台配置。** 查看预检摘要后，需要输入「恢复」、当前登录密码及已启用的两步验证码。恢复接口只允许网页登录会话，WebDAV 应用密码不能授权恢复。程序保留当前实例的登录安全设置与当前网络代理，不恢复旧的登录会话；完成后重新登录。未包含在备份里的远端备份密码等敏感配置，可能仍需重新填写。

数据库不是在线替换：服务先持久化恢复任务并退出，下一次启动在打开连接池前生成包含已提交 WAL 数据的本机回滚快照，再切换数据库。项目的 Docker Compose 会自动重启；直接运行二进制时需由进程管理器拉起，或手动再次执行 `tgdrive serve`。回滚副本包含本机敏感配置，请妥善保护，不要公开上传。

恢复后默认进入**安全模式**，暂停旧的上传残留清理、消息删除、迁移、自动备份和收件等后台任务，避免备份中的旧队列误删频道数据。检查恢复结果后，在备份页面再次认证并明确解除安全模式；该操作会重启服务。恢复仅导入主数据库，不会重新上传频道文件，也不恢复独立的封面缓存等旁路文件。

网络必须先连通 Telegram：本项目 Compose 中的 `telegram-bot-api` 共用后台代理，外部 Bot API 服务仍需单独保证网络可用；大分块还需要支持相应大小的 Bot API 或有效的 MTProto 配置。

#### 手动下载备份恢复

自动备份只生成一次 SQLite 一致性快照，再同时投递到全部已启用渠道。WebDAV 与 S3 保存完整
`.db` 文件；Telegram 使用独立的备份分块设置，默认每块 18MB，确保官方 Bot API 也能下载。
未超过分块大小时只有一个 `.db` 文件，可直接替换 `data/tgdrive.db`；多块备份的文件名以
`.part0001-of-000N` 结尾。把同一时间戳的全部分块下载到单独目录后，按编号顺序合并：

```bash
cat tgdrive-backup-YYYYMMDD-HHMMSS.db.part*-of-* > tgdrive.db
```

设置了**备份加密密码**时,远端快照是 age 格式,文件名以 `.db.age` 结尾(多块时为
`.db.age.part0001-of-000N`)。先按上面的方法合并分块,再解密:

```bash
cat tgdrive-backup-YYYYMMDD-HHMMSS.db.age.part*-of-* > backup.db.age   # 单块可省略
# 任选其一:官方 age 工具,或 tgdrive 自带的子命令
age -d -o tgdrive.db backup.db.age
docker run --rm -i -v "$PWD:/work" -w /work --entrypoint tgdrive tgdrive:latest decrypt-backup backup.db.age tgdrive.db
```

备份密码只用于加密远端快照,丢失后这些备份无法解密;「下载数据库备份」得到的始终是明文完整数据库。

停止 tgdrive 后再用合并出的 `tgdrive.db` 替换原数据库。每个渠道独立滚动保留最近 7 份；
Telegram 按整组清理，不会只留下半组。修改 WebDAV URL，或修改 S3 端点、桶、前缀、Access
Key 后，程序不会跨目标删除旧备份，需要自行清理旧位置。

### Range 流式播放

下载接口支持标准单段 HTTP Range。配置 api_id/api_hash 后，文档、音频和视频会直接通过
MTProto 按 512KiB 读取；任意偏移最多只会额外读取前方不足 512KiB 的对齐数据。播放器关闭
或请求取消后，服务会停止发出后续 MTProto 请求。若自建 Bot API 的文件已在共享卷缓存中，
仍优先本地直读，完全不产生 Telegram 下载流量。照片等不支持的 file_id 以及未配置 MTProto
时才保留原 Bot API 下载路径。

## WebDAV 使用

地址:`http://<host>:8080/dav`,Basic 认证,账号密码同上;启用两步验证后密码一栏填「账号安全」里生成的应用密码。

**rclone:**

```bash
rclone config create tgdrive webdav url=http://localhost:8080/dav vendor=other \
    user=admin pass=$(rclone obscure '你的密码')
rclone ls tgdrive:
rclone copy ./本地目录 tgdrive:备份目录
```

**macOS Finder**:访达 → 前往 → 连接服务器 → `http://<host>:8080/dav`。

**Windows 资源管理器**:映射网络驱动器 → `http://<host>:8080/dav`。注意 Windows 默认**禁止 HTTP 明文 Basic 认证**,需要:
- 推荐:套一层 HTTPS 反向代理(Caddy/Nginx),用 `https://` 地址连接;或
- 修改注册表 `HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters\BasicAuthLevel` 为 `2` 后重启 WebClient 服务(仅建议内网使用)。

限制:WebDAV 为顺序写,不支持部分(Range)PUT 与追加写;常见客户端不受影响。

## 命令行工具

```bash
# 用真实 bot 自检整条链路:getMe → 上传 1MB → 下载比对 → 删除
# check 会访问数据库；先停止主服务以获取实例锁
docker compose stop tgdrive
docker compose run --rm tgdrive check
docker compose up -d tgdrive

# 容器健康检查(镜像 HEALTHCHECK 已内置)
tgdrive healthcheck

# 生成 argon2 密码哈希,写进 .env 时加单引号(从标准输入读取,避免留在 shell 历史)
printf '%s' '新密码' | docker compose run --rm -T tgdrive hash-password

# 丢失验证器与恢复码时关闭两步验证（需独占数据库）
docker compose stop tgdrive
docker compose run --rm tgdrive reset-2fa
docker compose up -d tgdrive

# 解密 .db.age 备份;口令取 TGDRIVE_BACKUP_PASSWORD,未设置时从标准输入读取
tgdrive decrypt-backup tgdrive-backup-YYYYMMDD-HHMMSS.db.age tgdrive.db
```

HTTP 探针:

- `GET /healthz`:存活探针,HTTP 服务能响应即返回 `200`
- `GET /readyz`:就绪探针,检查 SQLite、当前存储目标和关键后台任务心跳;异常时返回 `503` 及各项 JSON 状态
- `GET /api/metrics`:Prometheus 文本格式指标(请求数按状态码类别、累计耗时、慢请求、文件/目录/容量、待删消息、后台任务状态),需登录;Prometheus 用 `basic_auth` 抓取即可

运行时镜像基于 distroless(无 shell),排查问题请用 `docker compose logs` 或 `docker cp`,不能 `docker exec sh`。

## 大目录元数据接口

`GET /api/fs/entries` 是文件页、侧栏目录树和目录选择器使用的连续滚动接口，需要与其他文件接口相同的登录 Cookie 或 Basic 认证；旧 `/api/fs/list`、分享目录接口与 WebDAV 仍保持全量列表兼容。已有实例需要重新构建并升级 tgdrive 服务后才会生效。

- 参数：`path` 必填，规范化后返回；`limit` 默认 `200`，允许 `1..500`；`sort=name|size|mtime` 默认 `name`；`order=asc|desc` 默认 `asc`；`kind=all|dir|file` 默认 `all`；`q` 默认空字符串；`cursor` 首批省略。非法参数返回 `400`。
- `q` 在整个直接子目录范围内按名称字面子串匹配，仅 ASCII 忽略大小写，`%`、`_` 没有通配符含义。不做递归搜索。`q` 最多 255 UTF-8 字节且不能含控制字符，`path` 最多 4096 字节（每段仍最多 255 字节）。
- 响应为 `{path, entries, next_cursor, has_more, total?}`。每项保留旧条目的 `name/is_dir/size/created_at/modified_at/sha256/favorite`，增加数值型 `id` 与布尔型 `has_thumbnail`。时间仍是 Unix 秒。末批 `next_cursor=null`、`has_more=false`；`total` 只在首批提供，表示符合 `kind/q` 的总数，续批省略此字段。
- 目录始终排在文件前面；组内按指定字段、`name COLLATE NOCASE`、`name COLLATE BINARY`、`id` 同方向稳定排序。中文采用 SQLite 编码序，不是浏览器的中文区域排序。客户端不要再次全量排序。
- 游标是不可依赖内部结构的版本化续读标记，绑定目录 ID 和全部查询参数（包括 `limit`），最多 4096 字节。改变目录、过滤、排序或批量大小必须清空游标重建；每批仍重新鉴权、解析 ready 目录，游标不授予权限。
- 静态目录遍历不重不漏；批次之间允许目录变化，采用弱一致性。客户端按 `id` 去重，刷新重新开始；已选择集合只覆盖用户实际选择的已加载条目，追加批次不应自动选中。侧栏/目录选择器使用 `kind=dir`；时间线使用 `kind=file&sort=mtime&order=desc`。
- 接口只读 SQLite 元数据，不请求 Telegram 内容。缩略图存在性单批查询 `chunks(node_id,idx=0)`；缩略图下载 handler 也只取首块，不加载全部分块。客户端可优先小图，小图不存在或失效时按媒体策略回退。
- 迁移 `0012` 仅增加三个 ready 部分索引，未修改 `0011`。分页用 `LIMIT+1` 和互斥的等值前缀/单范围 seek，避免 `OFFSET` 和混合排序规则行值比较退化为扫描。首次精确 `COUNT` 仍需扫描匹配目录；字面子串没有全文索引，稀疏过滤可能扫描较多剩余候选。新索引增加数据库占用与写入成本，实际大库必须先在安全副本验证迁移耗时、空间和查询计划。

后端回归（全部使用临时数据库，不需要 Telegram 配置）：

```bash
cargo test --locked entries -- --nocapture
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`entries_100k_deep_cursor_seeks_every_tail_key` 构造 100000 条同 NOCASE 名称、同大小和同修改时间的病理目录，对三种排序、两个方向比较深度 200/90000 的续批结果、全部尾键的 `EXPLAIN QUERY PLAN` 和 SQLite VM 指令数；断言无临时排序、实际范围 seek 且深处扫描不随深度增长。其他测试覆盖 0/199/200/201 边界、整目录筛选、非法/跨条件游标、鉴权、首块缩略图、旧列表与 WebDAV 全量兼容。合成测试不能替代真实 767697 文件目录副本上的迁移和性能验收。

## 本地开发

```bash
# 后端(读取 .env)
cargo run -- serve
# 前端(代理 /api、/dav 到 127.0.0.1:8080)
npm --prefix frontend install
npm --prefix frontend run dev
# 提交前检查
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm --prefix frontend run check
```

## 重要注意事项

- **务必备份 `./data/tgdrive.db`(SQLite,项目目录下的 `data/` 映射进容器 `/data`)**。Bot API 无法读取频道历史消息，普通上传也不会把虚拟路径写进消息；元数据库一旦丢失，频道里的分块将无法自动找回。库里同时存着后台改过的配置(含 bot token 等明文凭据;登录密码只存哈希),请一并妥善保管;远端备份建议开启备份加密。
- **限流**:Telegram 对单个 bot 发消息到同一频道限流约 20 条/分钟。触发 429 时该 Bot 会按 `retry_after` 自动重试，并把发送间隔自适应翻倍（每个 Bot 独立观测，成功发送后缓慢回落），减少反复撞限造成的整段冷却；大量小文件可配置额外上传 Bot 分流，大文件则建议自建 Bot API 并调大分块。
- **删除**:网页删除可选择移入回收站或直接删除;后者不可恢复。删除文件不会等待 Telegram 消息删除完成;失败任务会持久化到 SQLite,后台按指数退避重试。Telegram 不允许 Bot 删除发出超过 48 小时的消息(频道管理员也不例外),因此删除上传已超过两天的文件时,其频道消息通常会保留;遇到 `message can't be deleted` 的任务直接移出队列,只在每轮汇总日志中计数,如需清理可在 Telegram 客户端手动删除。消息清理默认手动执行,建议切到自动,以便上传残片在 48 小时内被清掉。可在后台系统状态查看队列。
- **后台日志**:系统设置中的「后台日志」会显示消息清理、维护与备份任务的批次完成、限流重试和队列清空状态;容器重启后仍可查看最近记录。完整 HTTP 诊断仍需查看 Docker 日志。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):多用户、端到端加密。

断点续传任务会在服务端保留 7 天。取消任务会清理已经上传的 Telegram 分块；超过 7 天仍未继续的任务会由后台维护自动清理。

## 技术栈

axum · dav-server · sqlx(SQLite)· reqwest · Vue 3 · Element Plus · Vite

### 主站静态资源缓存与预压缩

- 构建时为全部产物预生成 `.gz` / `.br` 变体（vite.config.ts），服务端命中 `Accept-Encoding` 时零压缩开销直发；未命中变体的小文件由运行时压缩兑底，已带编码的响应不会二次压缩。
- `/assets/*` 带内容哈希，响应 `Cache-Control: public, max-age=31536000, immutable`；缺失的哈希资源真 404，不会被 SPA fallback 用 index.html 顶替。入口 `index.html` 与未哈希的 public 资源（图标、manifest）响应 `no-cache`，每次再验证，避免升级后旧页面引用已替换的哈希资源。应用插件的运行资源继续使用各自带修订的不可变缓存，不受影响。

### 网页目录分批与虚拟视图

文件页、侧栏目录树和移动/复制目录选择器现已接入 `/api/fs/entries`，不再通过旧全量 `/fs/list` 读取目录：

- 文件页每批 200 项，滚动到末尾两视口内续载，每代最多一个请求；显示“已加载 X / 匹配 Y”。失败保留已加载项目，点击“重试”继续；空/全重复续批会停住自动续载，避免动态目录在后台被遍历。
- 名称过滤覆盖整个直接子目录，输入防抖 250ms；列表表头和排序菜单均由后端排序。切目录、查询、排序或进入/离开时间线都会取消旧请求、换代、清游标/选择/已加载项目。后端名称顺序是 SQLite NOCASE/BINARY，不是中文拼音排序。
- list/grid/photo/timeline 共用 `@tanstack/vue-virtual`。桌面列表固定 56px 行且表头吸顶，手机使用 72px 触控行；网格/大图固定行高、自适应列数，resize 以稳定条目 ID 重定位；时间线固定文件、修改时间降序，按本地日期增量组织已加载项目，跨批次合并日期组头。
- 桌面虚拟内容区最高 720px，手机列表填满导航间的剩余高度；网格最多 20 列，前后通常各缓冲一视口，并以 **200 个文件组件**为硬上限，优先保留可见行再减少缓冲；超高/超宽屏不会无限增加媒体组件。此限额不限制已经访问过的元数据数量，元数据浅响应原位追加并按 ID 去重。
- Ctrl+A 只选择当前已加载项目，一次替换 `Set<id>`；滚出视口仍然选中，下一批不会自动选中。删除先冻结路径/条目，每请求最多 200 项并串行报告进度/失败；移动逐项串行。改名、移动、删除后重建当前代；收藏只更新本地条目。
- 上传的惰性扫描、冲突检查、拖拽和粘贴入口保持原行为。上传完成时若已经向下浏览、有选择或正进行批操作，只提示刷新；顶部无选择时合并刷新。显式刷新会重建目录，动态目录跨批次仍为弱一致观察。
- 侧栏和 DirPicker 共用虚拟扁平目录树，每个展开目录仅按 `kind=dir` 获取 200 项；后续目录需点击“加载更多目录”，该行不能成为操作目标。收起或销毁会取消对应请求，不后台递归扫描。

前端验证：

```sh
npm --prefix frontend ci
npm --prefix frontend run check
npm --prefix frontend run test
# 首次准备 Playwright 浏览器（已有 Chrome 时也可用下方环境变量指定路径）
(cd frontend && npx playwright install chromium)
npm --prefix frontend run test:e2e
# 或：PLAYWRIGHT_CHROMIUM_EXECUTABLE=/path/to/chrome npm --prefix frontend run test:e2e
```

Vitest 覆盖请求代次/旧 finally 隔离、防抖、同游标重试/ID 去重/空批停止、四视图挂载上限、表头偏移、resize 锚点、时间线跨批次、Set 选择、目录树、冻结批操作和媒体资源释放。Playwright 启动独立的发布版预览服务，拦截全部 API，以 767697 条逻辑目录及合成图片验证四视图、连续加载、选择范围、旧响应隔离、原图回退、视频无原片、三图预览和往返滚动，不连接生产服务。

### 图片缩略图与预览的资源预算

- 列表/网格仅为虚拟视口及缓冲区加载图片，所有模式共享 **4 个下载槽、1 个解码/缩小槽**；下载槽保留到缩小完成，避免原始 Blob 在等待解码时无限堆积。离开挂载范围即释放引用，并取消尚未完成的任务。
- 优先使用已登记的 Telegram 小图。图片缺少小图或小图失效时回退原图；视频绝不回退原视频。原图按声明和实际流字节双重检查，超过 **20 MiB** 时只显示图标，可点击预览或下载原图。失败不随虚拟行反复挂载而自动重试，显式刷新可重试。
- 图片串行缩小到最长边 **512px**。小图 LRU 最多 **128 项 / 16 MiB**，临时 Blob、ImageBitmap 和对象 URL 均在不再使用时释放；活动引用在驱逐时保留到最后一个显示组件释放，避免出现失效图片。
- 图片预览只挂载当前及相邻两张原图，保留上一张/下一张、缩放、旋转、键盘导航；导航范围明确为已加载项目，不为寻找下一张扫描整目录。视频播放列表也仅取已加载范围。
- **剩余限制：** 压缩文件大小不是解码内存大小，单张极端高分辨率图片仍可能有瞬时解码峰值；预览读取原图，不受缩略读取的 20 MiB 限制。已访问的元数据随实际访问量增长，不能将虚拟 DOM 或 LRU 上限理解为浏览器总内存恒定。

### 大目录升级注意

先做 SQLite 在线备份（不能仅复制活跃 WAL 模式下的 `.db`），保留旧镜像；确认没有在途上传，再仅重建 tgdrive 服务。新增 0012 迁移只添加 ready 节点的三个排序索引，不改变文件或 Telegram 分块。已有未部署迁移须先在副本验证，索引需要额外磁盘空间。旧 `/fs/list` 和 WebDAV 仍保留全量兼容行为，分享页/回收站的大列表不属于本次虚拟化范围。

**回滚不能仅切换镜像：** SQLx 会拒绝当前程序未包含的已执行迁移。若旧版本仅包含 0010，而升级同时执行了 0011/0012，须先在数据库副本验证完整回退流程；只有确认历史存储迁移任务表为空、没有新版本写入冲突时，才能在停写与最新备份保护下撤销这两个新增迁移对象及记录。不要盲目恢复发布前数据库，也不要在已有存储迁移任务时删表。

真实 767697 文件目录的发布、性能、浏览器与回滚验证记录见 [大目录修复验收记录](docs/directory-fix-verification.md)。
