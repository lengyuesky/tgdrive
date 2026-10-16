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

- **不限大小**:自动分块上传/拼接下载;SHA-256 秒传、断点续传(暂停/继续/重试/取消,刷新后只补传缺失分块)
- **双端点**:官方 `api.telegram.org` 开箱即用,可切换自部署 `telegram-bot-api`(GB 级分块,local 模式直读磁盘);支持多上传 Bot 分流
- **Web 界面**:目录树、列表/网格/大图/时间线视图、拖拽上传、图片/视频/音频/文本预览、暗色模式;手机与 iPhone 专属布局,可添加到主屏幕
- **分享链接**:免登录直链,可选提取码、有效期、下载次数限制
- **回收站、高级搜索与收藏**:按类别/大小/时间/收藏组合筛选;文件与目录均可收藏
- **Bot 收件**:白名单用户私聊与频道直发入库,可按类型/后缀自动归类,视频自动选最高清晰度
- **多渠道备份**:一致性快照可同时备份到 Telegram、WebDAV 与 S3,各渠道滚动保留 7 份,支持 age 加密;网页向导可从频道备份一键恢复
- **应用中心**:短视频、影视、图书、漫画四款插件(独立仓库),进度跨设备实时同步,无服务器转码
- **账号安全**:argon2 密码哈希、可选 TOTP 两步验证、WebDAV 独立应用密码
- **网络代理**:HTTP / SOCKS5 / Cloudflare WARP(usque),网页保存即生效,自建 Bot API 共用
- **WebDAV**(Class 2):支持 rclone、Windows 资源管理器、macOS Finder
- **零成本复制**:只复制元数据,引用计数共享底层 Telegram 消息;流式传输,内存占用与文件大小无关

## 快速开始

### 1. 准备 Telegram

1. 找 [@BotFather](https://t.me/BotFather) 创建 bot,拿到 **bot token**
2. 新建一个**私有频道**作为存储空间,把 bot 加入频道并设为**管理员**(需要发消息和删消息权限)
3. 获取频道 ID(形如 `-1001234567890`):把频道任意消息转发给 [@userinfobot](https://t.me/userinfobot) 之类的工具即可看到

### 2. 部署

```bash
mkdir -p data
cp .env.example .env
# 编辑 .env:只需填 TGDRIVE_USERNAME / TGDRIVE_PASSWORD,其余留空
docker compose up -d   # 拉取发布镜像;首次使用或镜像拉不到时改用 `docker compose up -d --build` 本地构建
```

**先手动创建 data 目录再启动**(`mkdir -p data`)。镜像以非 root 用户 `1000:1000` 运行;如果省略 mkdir 让 Docker 自动建目录,它会属于 root,容器将无权写入,启动日志出现 `Permission denied (os error 13)`。宿主机用户不是 `1000:1000` 时,把 `.env` 中的 `TGDRIVE_UID` / `TGDRIVE_GID` 改为 `id -u` / `id -g` 的输出,或直接 `sudo chown -R 1000:1000 data`。启用自建 Bot API profile 时同样需要 `mkdir -p bot-api-control && chown 1000:1000 bot-api-control`(tgdrive 要往里写 proxychains 配置);`telegram-bot-api-data` 由服务自行管理属主,无需手动 chown。

打开 <http://localhost:8080> 登录,页面会提示「还没配置 Telegram 存储」,点「去配置」填好 **Bot Token** 与**存储频道**,保存即生效(不用重启容器)。也可以在 `.env` 里预先填好 `TGDRIVE_BOT_TOKEN` / `TGDRIVE_CHAT_ID`。

配置优先级:**后台修改 > 环境变量 > 内置默认**。绝大多数配置(bot token、频道、端点、分块大小、代理、备份、收件规则等)都能在网页「系统设置」中修改并立即生效;环境变量只在启动时读取一次。

### 3. 大文件场景:自建 Bot API(可选)

官方端点单块上限 20MB 且有限流,大文件吞吐约 6MB/s。自建 `telegram-bot-api` 后分块可到 GB 级。在 <https://my.telegram.org> 申请 `api_id` / `api_hash`,后台「自建 Bot API 服务器」填好并保存,点「导出 .env」覆盖回项目目录,执行 `docker compose --profile local-bot-api up -d`(本地构建则加 `--build`)。

## WebDAV

地址 `http://<host>:8080/dav`,Basic 认证,账号密码同 Web;启用两步验证后需使用「账号安全」里生成的应用密码。

```bash
rclone config create tgdrive webdav url=http://localhost:8080/dav vendor=other \
    user=admin pass=$(rclone obscure '你的密码')
```

macOS Finder:前往 → 连接服务器;Windows 资源管理器:映射网络驱动器(默认禁止 HTTP 明文 Basic,建议套 HTTPS 反代)。WebDAV 为顺序写,不支持部分 PUT 与追加写。

## 命令行工具

```bash
# 用真实 bot 自检整条链路(getMe → 上传 1MB → 下载比对 → 删除);先停止主服务
docker compose stop tgdrive && docker compose run --rm tgdrive check && docker compose up -d tgdrive

# 生成 argon2 密码哈希,写进 .env 时加单引号
printf '%s' '新密码' | docker compose run --rm -T tgdrive hash-password

# 丢失验证器与恢复码时关闭两步验证(需独占数据库)
docker compose stop tgdrive && docker compose run --rm tgdrive reset-2fa && docker compose up -d tgdrive
```

探针:`GET /healthz`(存活)、`GET /readyz`(就绪)、`GET /api/metrics`(Prometheus 指标,需登录)。镜像基于 distroless,排查问题请用 `docker compose logs` 或 `docker cp`。

## 本地开发

```bash
cargo run -- serve                 # 后端(读取 .env)
npm --prefix frontend install && npm --prefix frontend run dev   # 前端
# 提交前检查
cargo fmt --all --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
npm --prefix frontend run check
```

## 重要注意事项

- **务必备份 `./data/tgdrive.db`**。元数据库一旦丢失,频道里的分块将无法自动找回;库里还存有后台改过的配置,请一并妥善保管,远端备份建议开启加密。
- **限流**:单个 bot 发消息到同一频道约 20 条/分钟,触发 429 自动按 `retry_after` 重试;大量小文件可配置额外上传 Bot,大文件建议自建 Bot API 并调大分块。
- **删除**:Bot 无法删除发出超过 48 小时的消息,删除两天前上传的文件时频道消息通常会保留,需在 Telegram 客户端手动清理;删除失败任务持久化并按指数退避重试。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):多用户、端到端加密。

## 技术栈

axum · dav-server · sqlx(SQLite)· reqwest · Vue 3 · Element Plus · Vite
