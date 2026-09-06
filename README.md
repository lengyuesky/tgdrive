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
- **双端点**:官方 `api.telegram.org` 开箱即用;可切换自部署 `telegram-bot-api`(单块上限 2000MB，默认使用更稳妥的 256MB 分块；local 模式直读磁盘,速度更快)
- **Web 界面**:目录树侧栏、列表/网格/大图/时间线四种视图、拖拽上传(带进度)、下载、重命名、移动(目录选择器)、复制、图片/视频/音频/文本预览、暗色模式、可安装 PWA
- **断点续传**:多分块大文件固定交给默认 Bot 串行上传,上传期间其他任务暂缓并在完成后自动继续;支持单项或一键批量暂停、继续、重试和取消;刷新页面或服务重启后重新选择原文件并通过 SHA-256 校验,只补传缺失分块
- **秒传**:上传前先算 SHA-256,库里已有同内容文件直接零流量落位;上传文件夹、Ctrl+V 粘贴截图均支持
- **分享链接**:对文件/目录生成免登录直链,可选提取码、有效期、下载次数限制
- **回收站**:网页删除时可选择移入回收站或直接删除;WebDAV 删除始终进回收站,超过保留天数自动彻底清理
- **高级搜索与收藏**:按名称、文件/目录、文件类别或后缀、大小、修改时间和收藏状态组合筛选,支持排序;文件与目录均可收藏并从侧栏集中查看
- **Bot 收件**:往存储频道直发或白名单用户私聊文件/视频/图片自动零流量转存;多档视频自动选择最高可用清晰度;可把视频、图片、书籍分别存到指定目录,并自定义「文件后缀 → 目录」规则
- **多渠道数据库备份**:一键下载一致性快照;可同时备份到 Telegram、WebDAV 与 S3 兼容对象存储,各渠道独立滚动保留最近 7 份并单独重试失败项
- **可靠后台任务**:Telegram 删除失败进入持久化重试队列;超过 Bot API 48 小时窗口的永久失败会停止重试并提示手动清理;Bot 收件连续失败会进入死信,避免卡住后续消息
- **后台日志**:管理页「后台日志」可查看消息清理、维护与备份等关键后台事件的进度和失败重试;日志持久化保留最近 2000 条
- **WebDAV**:支持 rclone、Windows 资源管理器、macOS Finder(Class 2 LOCK)
- **零成本复制**:复制文件/目录只复制元数据,共享底层 Telegram 消息(引用计数,删除安全)
- **SQLite 唯一索引**:普通文件上传不向 Telegram 消息写入虚拟路径；重命名和移动只修改 SQLite 元数据，目录结构随数据库备份恢复
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
| `TGDRIVE_UPLOAD_BOT_TOKENS` | | — | ✔ | 额外上传 Bot Token，逗号分隔；各 Bot 必须是同一存储频道管理员，主 Bot 也会参与上传池 |
| `TGDRIVE_CHAT_ID` | | — | ✔ | 存储频道 ID(`-100…`)或 `@频道名`;同上 |
| `TGDRIVE_API_BASE` | | `https://api.telegram.org` | ✔ | Bot API 端点 |
| `TGDRIVE_CHUNK_SIZE` | | 官方 18MB / 自建 1900MB | ✔ | 分块大小,支持 `18MB`/`1g` 写法;官方端点上限 19.5MB(受 getFile 20MB 限制),自建上限 1900MB |
| `TGDRIVE_UPLOAD_CONCURRENCY` | | `2` | ✔ | 单个上传 Bot API 实例的并发数；总并发 = 此值 × 上传 Bot 数量 |
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

上传并发是每个上传 Bot API 实例的并发数；总并发为“设置值 × 上传 Bot 数量”。例如配置 3 个 Bot、并发设置为 1 时，总并发为 3；设置为 2 时，总并发为 6。单块文件按 Bot 冷却状态和轮询分配；需要多个 Telegram 分块的文件统一使用默认 Bot，并在浏览器队列中独占上传通道、单分块串行，完成后再恢复其他任务。

## 后台管理

右上角用户名菜单 → **系统设置**(`/admin`)。

- **系统状态**:bot、端点、分块、文件数与占用、元数据库体积、运行时长、待删除任务、收件死信与后台任务健康度
- **Telegram 存储**:bot token、存储频道、接口端点、分块大小、上传并发、视频消息模式
- **历史存储迁移**:系统设置的「后台任务」可让主 Bot 在 Telegram 服务端逐条复制其他 Bot 的历史消息，原子切换所有共享分块引用；支持暂停、失败重试和服务重启后继续，用户上传期间自动让路
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

仓库内置 GitHub Actions CI,会在 push 和 pull request 时执行同一套 Rust 格式、Clippy、测试以及前端类型检查和生产构建。

## 重要注意事项

- **务必备份 `./data/tgdrive.db`(SQLite,项目目录下的 `data/` 映射进容器 `/data`)**。Bot API 无法读取频道历史消息，普通上传也不会把虚拟路径写进消息；元数据库一旦丢失，频道里的分块将无法自动找回。库里同时存着后台改过的配置(含 bot token 与密码,明文),请一并妥善保管。
- **限流**:Telegram 对单个 bot 发消息到同一频道限流约 20 条/分钟。触发 429 时该 Bot 会按 `retry_after` 自动重试；大量小文件可配置额外上传 Bot 分流，大文件则建议自建 Bot API 并调大分块。
- **删除**:网页删除可选择移入回收站或直接删除;后者不可恢复。删除文件不会等待 Telegram 消息删除完成;失败任务会持久化到 SQLite,后台按指数退避重试。任务入队超过 48 小时后若 Telegram 仍明确返回 `message can't be deleted`,会停止无效重试并提示在 Telegram 客户端手动删除。可在后台系统状态查看队列。
- **后台日志**:系统设置中的「后台日志」会显示消息清理、维护与备份任务的批次完成、限流重试和队列清空状态;容器重启后仍可查看最近记录。完整 HTTP 诊断仍需查看 Docker 日志。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):多用户、端到端加密。

断点续传任务会在服务端保留 7 天。取消任务会清理已经上传的 Telegram 分块；超过 7 天仍未继续的任务会由后台维护自动清理。

## 技术栈

axum · dav-server · sqlx(SQLite)· reqwest · Vue 3 · Element Plus · Vite

### 网页目录分批与虚拟视图

文件页、侧栏目录树和移动/复制目录选择器现已接入 `/api/fs/entries`，不再通过旧全量 `/fs/list` 读取目录：

- 文件页每批 200 项，滚动到末尾两视口内续载，每代最多一个请求；显示“已加载 X / 匹配 Y”。失败保留已加载项目，点击“重试”继续；空/全重复续批会停住自动续载，避免动态目录在后台被遍历。
- 名称过滤覆盖整个直接子目录，输入防抖 250ms；列表表头和排序菜单均由后端排序。切目录、查询、排序或进入/离开时间线都会取消旧请求、换代、清游标/选择/已加载项目。后端名称顺序是 SQLite NOCASE/BINARY，不是中文拼音排序。
- list/grid/photo/timeline 共用 `@tanstack/vue-virtual`。列表固定 56px 行且表头吸顶；网格/大图固定行高、自适应列数，resize 以稳定条目 ID 重定位；时间线固定文件、修改时间降序，按本地日期增量组织已加载项目，跨批次合并日期组头。
- 虚拟内容区最高 720px、最多 20 列，前后通常各缓冲一视口，并以 **200 个文件组件**为硬上限，优先保留可见行再减少缓冲；超高/超宽屏不会无限增加媒体组件。此限额不限制已经访问过的元数据数量，元数据浅响应原位追加并按 ID 去重。
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
