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
- **秒传**:上传前先算 SHA-256,库里已有同内容文件直接零流量落位;上传文件夹、Ctrl+V 粘贴截图均支持
- **分享链接**:对文件/目录生成免登录直链,可选提取码、有效期、下载次数限制
- **回收站**:删除先软删可恢复(WebDAV 删除同样进回收站),超过保留天数自动彻底清理
- **全局搜索**:按文件名全库搜索,结果直达所在目录
- **Bot 收件**:往存储频道直发或白名单用户私聊文件/视频/图片自动零流量转存;可把视频、图片、书籍分别存到指定目录,并自定义「文件后缀 → 目录」规则
- **数据库备份**:一键下载一致性快照;可开启每日自动备份到存储频道(滚动保留 7 份)
- **WebDAV**:支持 rclone、Windows 资源管理器、macOS Finder(Class 2 LOCK)
- **零成本复制**:复制文件/目录只复制元数据,共享底层 Telegram 消息(引用计数,删除安全)
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
cp .env.example .env
# 编辑 .env:只需填 TGDRIVE_USERNAME / TGDRIVE_PASSWORD
docker compose up -d --build
```

打开 <http://localhost:8080> 登录,页面会提示「还没配置 Telegram 存储」,点「去配置」填好 **Bot Token** 与**存储频道**,保存即生效(不用重启容器)。

当然也可以在 `.env` 里预先填好 `TGDRIVE_BOT_TOKEN` / `TGDRIVE_CHAT_ID`,启动后直接可用。

### 3. 用自建 Bot API 服务器(推荐大文件场景)

官方端点下载单块上限 20MB,所以分块只能 ≤18MB,而 bot 发消息到同一频道约有 20 条/分钟 的限流,大文件吞吐约 6MB/s。自建 `telegram-bot-api` 后分块可到 GB 级,轻松跑满带宽。

在 <https://my.telegram.org> 申请 `api_id` / `api_hash` 后,有两种配法:

**A. 网页里配**(推荐):后台「自建 Bot API 服务器」填好 api_id / api_hash 并保存,点「导出 .env」把内容覆盖回项目目录的 `.env`,执行:

```bash
docker compose up -d telegram-bot-api
```

重建容器后,回「Telegram 存储」把「接口端点」切到**自建 Bot API** 再保存即可。

**B. 直接改 .env**:填 `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 与 `TGDRIVE_API_BASE=http://telegram-bot-api:8081`,再执行 `docker compose up -d`。

> telegram-bot-api 容器随 `docker compose up -d` 默认启动;没配 api_id / api_hash 时它会报错重启,不影响 tgdrive 走官方端点,不需要自建可 `docker compose stop telegram-bot-api`。

> api_id / api_hash 由 telegram-bot-api 容器读取(compose 的 `${VAR}` 插值取自宿主机 `.env`),tgdrive 改不了别的容器的环境变量 —— 所以这两项无论在哪改,都必须重建那个容器才生效。

compose 已把 telegram-bot-api 的数据目录(`./telegram-bot-api-data`)以相同路径只读挂到 tgdrive 容器(local 模式下 `getFile` 返回容器内绝对路径,tgdrive 自动探测并直读磁盘,失败才回退 HTTP)。

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
| `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` | | — | ✔* | 自建 Bot API 服务器的凭据,在 <https://my.telegram.org> 申请;\*后台可编辑并导出,但由 telegram-bot-api 容器读取,**必须重建那个容器才生效** |
| `TGDRIVE_LISTEN` | | `0.0.0.0:8080` | | 监听地址(端口由 compose 映射,改需重启) |
| `TGDRIVE_DB_PATH` | | `/data/tgdrive.db`(镜像内) | | SQLite 路径(设置本身存在这个库里) |
| `TGDRIVE_SECRET` | | 自动生成并存库 | | cookie 签名密钥,**仅首次启动时作为初始值**,之后以库中值为准 |
| `TGDRIVE_STATIC_DIR` | | `/app/static`(镜像内) | | 前端静态文件目录 |
| `RUST_LOG` | | `info` | | 日志级别 |

## 后台管理

右上角用户名菜单 → **系统设置**(`/admin`)。

- **系统状态**:bot、端点、分块、文件数与占用、元数据库体积、运行时长
- **Telegram 存储**:bot token、存储频道、接口端点、分块大小、上传并发、视频消息模式
- **自建 Bot API 服务器**:api_id / api_hash,以及切换到自建端点的操作步骤
- **账号**、**高级**(临时目录)、需重启才能改的项(监听地址、数据库路径等)
- **Bot 收件**:设置默认、视频、图片、书籍目录;自定义类型名称、一个或多个文件后缀及目标目录。自定义规则优先,多段后缀按更具体的规则匹配

上表中标了「可在后台改」的项都能在这里改,**保存后立即生效,不需要重启容器**(api_id / api_hash 除外,见下)。改动写进 SQLite,优先级高于 `.env`;每项都标了来源(`来自 .env` / `后台已改` / `默认值`),点「恢复」即可退回 `.env` 或默认值。

- **安全网**:改 Telegram 相关配置时先自动跑一次 `getMe`,连不上就拒绝保存(可选择强制保存,给自建端点还没启动的场景用);分块大小与端点的组合会一并校验,例如切回官方端点时不允许保留 512MB 的分块
- **导出 .env**:把当前生效配置生成 `.env` 文本,方便同步回宿主机文件
- **维护**:清理上传中断留下的残留;登出所有会话(轮换签名密钥)

> 改账号后所有设备的登录状态立即失效,WebDAV 客户端也要同步改密码。

**改配置前请注意:**

- **换存储频道**只影响之后上传的文件。已存文件的分块仍在原频道,删除它们时消息可能无法自动清理(需要手动去原频道删)。
- **从自建端点切回官方端点**前,确认历史分块都不超过 20MB —— 官方 `getFile` 下载上限 20MB,更大的分块将读不出来。
- **api_id / api_hash 改完不会立刻生效**:它们是 compose 传给 telegram-bot-api 容器的环境变量,tgdrive 改不了别的容器的环境。保存后要导出 `.env` 覆盖回项目目录,再执行 `docker compose up -d telegram-bot-api` 重建那个容器。

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

## 本地开发

```bash
# 后端(读取 .env)
cargo run -- serve
# 前端(代理 /api、/dav 到 127.0.0.1:8080)
cd frontend && npm install && npm run dev
# 测试
cargo test
```

## 重要注意事项

- **务必备份 `./data/tgdrive.db`(SQLite,项目目录下的 `data/` 映射进容器 `/data`)**。Bot API 无法读取频道历史消息,元数据库一旦丢失,频道里的分块将无法自动找回(每条消息的 caption 里记有 `{"p":路径,"i":分块序号}`,可作最后的人工恢复线索)。库里同时存着后台改过的配置(含 bot token 与密码,明文),请一并妥善保管。
- **限流**:Telegram 对 bot 发消息到同一频道限流约 20 条/分钟。触发 429 时程序会按 `retry_after` 自动重试;官方端点大文件多的场景建议自建 Bot API 并调大分块。
- **删除**:删除文件会尽力删除对应频道消息(bot 需要是频道管理员);删除失败仅记日志,不阻塞操作。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):断点续传、回收站、多用户、端到端加密、元数据库自动备份到频道、`chunks` 表记录 `chat_id`(支持换频道后仍能清理旧频道消息)。

## 技术栈

axum · dav-server · sqlx(SQLite)· reqwest · Vue 3 · Element Plus · Vite
