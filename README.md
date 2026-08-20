# tgdrive

用 Rust 编写的个人网盘,把 **Telegram 作为存储后端**:文件分块后以 document 形式发到你的私有频道,本地只保存元数据(SQLite)。自带 Web 界面与 WebDAV 服务,Docker 一键部署。

```
浏览器 SPA ──┐                             ┌─> SQLite(元数据:目录树 / 分块映射)
             ├─> axum HTTP 服务(单进程)────┤
WebDAV 客户端─┘   ├ /api/*  REST            └─> Telegram Bot API
                  ├ /dav/*  WebDAV(Class 2)      ├ 官方 api.telegram.org(分块≤18MB)
                  └ /       Vue 3 前端            └ 自部署 telegram-bot-api(分块可达 GB 级)
```

## 特性

- **大文件不受单文件限制**:自动分块上传/拼接下载,官方 Bot API 也能存任意大小文件
- **双端点**:官方 `api.telegram.org` 开箱即用;可切换自部署 `telegram-bot-api`(单块上限 2000MB,local 模式直读磁盘,速度更快)
- **Web 界面**:目录树侧栏、列表/网格/大图/时间线四种视图、拖拽上传(带进度)、下载、重命名、移动(目录选择器)、复制、图片/视频/音频/文本预览、暗色模式、可安装 PWA
- **断点续传**:浏览器按 Telegram 分块独立并发上传,可暂停、继续和取消;刷新页面或服务重启后重新选择原文件并通过 SHA-256 校验,只补传缺失分块
- **秒传**:上传前先算 SHA-256,库里已有同内容文件直接零流量落位;上传文件夹、Ctrl+V 粘贴截图均支持
- **分享链接**:对文件/目录生成免登录直链,可选提取码、有效期、下载次数限制
- **回收站**:网页删除时可选择移入回收站或直接删除;WebDAV 删除始终进回收站,超过保留天数自动彻底清理
- **高级搜索与收藏**:按名称、文件/目录、文件类别或后缀、大小、修改时间和收藏状态组合筛选,支持排序;文件与目录均可收藏并从侧栏集中查看
- **Bot 收件**:往存储频道直发或白名单用户私聊文件/视频/图片自动零流量转存;多档视频自动选择最高可用清晰度;可把视频、图片、书籍分别存到指定目录,并自定义「文件后缀 → 目录」规则
- **多渠道数据库备份**:一键下载一致性快照;可同时备份到 Telegram、WebDAV 与 S3 兼容对象存储,各渠道独立滚动保留最近 7 份并单独重试失败项
- **可靠后台任务**:Telegram 删除失败进入持久化重试队列;Bot 收件连续失败会进入死信,避免卡住后续消息
- **WebDAV**:支持 rclone、Windows 资源管理器、macOS Finder(Class 2 LOCK)
- **零成本复制**:复制文件/目录只复制元数据,共享底层 Telegram 消息(引用计数,删除安全)
- **路径同步**:重命名或移动文件、文件夹后自动更新独占 Telegram 消息 caption 中的恢复路径；失败会持久化重试。Telegram 不支持原地修改附件文件名，共享消息也不会强行绑定到某一个副本路径
- **流式传输**:上传下载全程流式,内存占用与文件大小无关;>32MB 的分块经临时文件中转
- **零原片流量缩略图**:网页上传视频时从浏览器本地取帧,按内容哈希缓存小图;浏览文件列表不会读取原视频
- **单用户认证**:Web 登录(HttpOnly cookie)+ WebDAV Basic,同一套账号密码
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

**A. 网页里配**(推荐):后台「自建 Bot API 服务器」填好 api_id / api_hash 并保存,点「导出 .env」把内容覆盖回项目目录的 `.env`,执行:

```bash
docker compose --profile local-bot-api up -d
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

| 变量 | 必填 | 默认 | 可在后台改 | 说明 |
|---|---|---|---|---|
| `TGDRIVE_USERNAME` / `TGDRIVE_PASSWORD` | ✔ | — | ✔ | Web 与 WebDAV 的账号密码;唯一必填项(没有它就没法登录后台配其余部分) |
| `TGDRIVE_BOT_TOKEN` | | — | ✔ | bot token;留空则启动后在网页里配 |
| `TGDRIVE_CHAT_ID` | | — | ✔ | 存储频道 ID(`-100…`)或 `@频道名`;同上 |
| `TGDRIVE_API_BASE` | | `https://api.telegram.org` | ✔ | Bot API 端点 |
| `TGDRIVE_CHUNK_SIZE` | | 官方 18MB / 自建 1900MB | ✔ | 分块大小,支持 `18MB`/`1g` 写法;官方端点上限 19.5MB(受 getFile 20MB 限制),自建上限 1900MB |
| `TGDRIVE_UPLOAD_CONCURRENCY` | | `2` | ✔ | 分块并发上传数,调大易触发 429 限流 |
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
| `TGDRIVE_STATIC_DIR` | | `/app/static`(镜像内) | | 前端静态文件目录 |
| `RUST_LOG` | | `info` | | 日志级别 |

## 后台管理

右上角用户名菜单 → **系统设置**(`/admin`)。

- **系统状态**:bot、端点、分块、文件数与占用、元数据库体积、运行时长、待删除任务、收件死信与后台任务健康度
- **Telegram 存储**:bot token、存储频道、接口端点、分块大小、上传并发、视频消息模式
- **MTProto 流式播放 / 自建 Bot API**：api_id / api_hash，以及切换到自建端点的操作步骤
- **账号**、**高级**(临时目录)、需重启才能改的项(监听地址、数据库路径等)
- **备份与回收站**:Telegram、WebDAV、S3 渠道可分别启用；WebDAV 支持 Basic Auth，S3 支持自定义端点、区域、桶、对象前缀、临时令牌和路径样式
- **Bot 收件**:设置默认、视频、图片、书籍目录;自定义类型名称、一个或多个文件后缀及目标目录。自定义规则优先,多段后缀按更具体的规则匹配

上表中标了「可在后台改」的项都能在这里改,**保存后立即生效,不需要重启容器**(api_id / api_hash 除外,见下)。改动写进 SQLite,优先级高于 `.env`;每项都标了来源(`来自 .env` / `后台已改` / `默认值`),点「恢复」即可退回 `.env` 或默认值。

- **安全网**:改 Telegram 相关配置时会验证 Bot 身份、目标会话以及频道/群组的管理员、发消息和删消息权限;验证失败就拒绝保存(可选择强制保存,给自建端点还没启动的场景用)。分块大小与端点的组合也会一并校验,例如切回官方端点时不允许保留 512MB 的分块
- **导出 .env**:把当前生效配置生成 `.env` 文本,方便同步回宿主机文件
- **维护**:设置多渠道数据库备份、Telegram 备份分块大小(默认采用官方 Bot API 安全值 18MB)、清理上传中断留下的残留、登出所有会话(轮换签名密钥)

> 改账号后所有设备的登录状态立即失效,WebDAV 客户端也要同步改密码。

**改配置前请注意:**

- **换 Bot、端点或存储频道**只影响之后开始的上传。每个分块会记录原 Bot/API/频道,历史文件仍按原目标读取和删除;请保留原 Bot Token、端点和频道权限,否则对应历史文件会不可用。
- **旧数据库首次升级**无法凭 Telegram 消息反推出每个历史分块的原目标,程序会把尚未关联目标的旧分块一次性绑定到升级时的当前配置。如果旧库曾经换过 Bot、端点或频道,请先备份数据库并人工核对。
- **api_id / api_hash 改完不会立刻生效**：tgdrive 在启动时读取它们建立 MTProto 会话，Compose 也把它们传给 telegram-bot-api。保存后要导出 `.env` 覆盖回项目目录，再执行 `docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api`。

### 数据库备份恢复

自动备份只生成一次 SQLite 一致性快照，再同时投递到全部已启用渠道。WebDAV 与 S3 保存完整
`.db` 文件；Telegram 使用独立的备份分块设置，默认每块 18MB，确保官方 Bot API 也能下载。
未超过分块大小时只有一个 `.db` 文件，可直接替换 `data/tgdrive.db`；多块备份的文件名以
`.part0001-of-000N` 结尾。把同一时间戳的全部分块下载到单独目录后，按编号顺序合并：

```bash
cat tgdrive-backup-YYYYMMDD-HHMMSS.db.part*-of-* > tgdrive.db
```

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

地址:`http://<host>:8080/dav`,Basic 认证,账号密码同上。

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
docker compose run --rm tgdrive check

# 容器健康检查(镜像 HEALTHCHECK 已内置)
tgdrive healthcheck
```

HTTP 探针:

- `GET /healthz`:存活探针,HTTP 服务能响应即返回 `200`
- `GET /readyz`:就绪探针,检查 SQLite、当前存储目标和关键后台任务心跳;异常时返回 `503` 及各项 JSON 状态

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

仓库内置 GitHub Actions CI,会在 push 和 pull request 时执行同一套 Rust 格式、Clippy、测试以及前端类型检查和生产构建。

## 重要注意事项

- **务必备份 `./data/tgdrive.db`(SQLite,项目目录下的 `data/` 映射进容器 `/data`)**。Bot API 无法读取频道历史消息,元数据库一旦丢失,频道里的分块将无法自动找回(每条消息的 caption 里记有 `{"p":路径,"i":分块序号}`,可作最后的人工恢复线索)。库里同时存着后台改过的配置(含 bot token 与密码,明文),请一并妥善保管。
- **限流**:Telegram 对 bot 发消息到同一频道限流约 20 条/分钟。触发 429 时程序会按 `retry_after` 自动重试;官方端点大文件多的场景建议自建 Bot API 并调大分块。
- **删除**:网页删除可选择移入回收站或直接删除;后者不可恢复。删除文件不会等待 Telegram 消息删除完成;失败任务会持久化到 SQLite,后台按指数退避持续重试。可在后台系统状态查看队列。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):多用户、端到端加密。

断点续传任务会在服务端保留 7 天。取消任务会清理已经上传的 Telegram 分块；超过 7 天仍未继续的任务会由后台维护自动清理。

## 技术栈

axum · dav-server · sqlx(SQLite)· reqwest · Vue 3 · Element Plus · Vite
