[← 返回 README](../README.md)

# Telegram 存储、部署与自建 Bot API

## 官方端点与自建端点

官方 Bot API 下载单块上限 20MB,分块只能 ≤18MB,而 bot 发消息到同一频道约 20 条/分钟的限流让大文件吞吐约 6MB/s。自建 `telegram-bot-api` 后分块可到 GB 级(默认使用更稳妥的 256MB 分块),local 模式直读磁盘,速度更快。

`api_id` / `api_hash`(在 <https://my.telegram.org> 申请)既供 tgdrive 的 MTProto Range 下载使用,也由 telegram-bot-api 容器读取。浏览器播放视频时每次只向 Telegram 请求最多 512KiB,关闭预览后停止后续请求,不需要改变已有的 GB 级 Telegram 分块。

自建服务位于 Compose 的 `local-bot-api` profile 中,不会随普通的 `docker compose up` 启动。

两个服务由同一份 Compose 管理,并共用 tgdrive 后台「网络代理」设置。无法直连 Telegram 时,只需在后台启用代理,或在 `.env` 设置 `TGDRIVE_PROXY_ENABLED=true` 和 `TGDRIVE_PROXY_URL`。首次升级到支持共享代理的版本,需要执行下面的构建命令重建两个服务;以后修改代理无需重建。

**A. 网页里配**(推荐):后台「自建 Bot API 服务器」填好 api_id / api_hash 并保存,点「导出 .env」把内容覆盖回项目目录的 `.env`,执行:

```bash
docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api
```

重建容器后,回「Telegram 存储」把「接口端点」切到**自建 Bot API** 再保存即可。

**B. 直接改 .env**:填 `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 与 `TGDRIVE_API_BASE=http://telegram-bot-api:8081`,再执行 `docker compose --profile local-bot-api up -d --build`。

> api_id / api_hash 会在 tgdrive 启动时用于 MTProto 会话,也由 telegram-bot-api 容器读取
> (compose 的 `${VAR}` 插值取自宿主机 `.env`)。所以这两项无论在哪改,都要先导出并
> 覆盖宿主机 `.env`,再重启/重建 tgdrive 与 telegram-bot-api。

compose 已把 telegram-bot-api 的数据目录(`./telegram-bot-api-data`)以相同路径只读挂到 tgdrive 容器(local 模式下 `getFile` 返回容器内绝对路径,tgdrive 自动探测并直读磁盘,失败才回退 HTTP)。默认还会把 Bot API 的文件组 `101` 作为 tgdrive 的补充组;若换用 UID/GID 不同的镜像,请把 `TGDRIVE_BOT_API_GID` 改为该镜像中文件所属的组 ID。

> 注意:bot 在官方与自建服务器之间迁移时,Telegram 要求先对旧端点调用 `logOut`,详见 telegram-bot-api 文档。

## 配置层级

配置有三层,**后台修改 > 环境变量(`.env`)> 内置默认**。`.env` 里只有账号密码是必填的,其余留空就在网页「系统设置」里配。

回收站保留天数、备份渠道、Bot 收件规则、消息自动清理等维护设置没有对应的环境变量,只能在后台修改,保存在数据库里并随数据库备份。环境变量只在启动时读取一次,改 `.env` 后需重启容器。

| 变量 | 必填 | 默认 | 可在后台改 | 说明 |
|---|---|---|---|---|
| `TGDRIVE_USERNAME` / `TGDRIVE_PASSWORD` | ✔ | — | ✔ | Web 与 WebDAV 的账号密码;唯一必填项(没有它就没法登录后台配其余部分)。密码可写明文,也可写 `tgdrive hash-password` 生成的 argon2 哈希(须加单引号:`TGDRIVE_PASSWORD='$argon2id$...'`);后台改过的密码只以哈希存库 |
| `TGDRIVE_BOT_TOKEN` | | — | ✔ | bot token;留空则启动后在网页里配 |
| `TGDRIVE_UPLOAD_BOT_TOKENS` | | — | ✔ | 额外上传 Bot Token,逗号分隔;各 Bot 必须是同一存储频道管理员,主 Bot 也会参与上传池 |
| `TGDRIVE_CHAT_ID` | | — | ✔ | 存储频道 ID(`-100…`)或 `@频道名`;同上 |
| `TGDRIVE_API_BASE` | | `https://api.telegram.org` | ✔ | Bot API 端点 |
| `TGDRIVE_PROXY_ENABLED` | | `false`(仅配置代理 URL 时自动启用) | ✔ | 出站代理开关;关闭后直连,但保留已保存的地址 |
| `TGDRIVE_PROXY_URL` | | — | ✔ | HTTP / SOCKS5 代理 URL,例如 `http://proxy:7890`、`socks5h://user:password@proxy:1080` |
| `TGDRIVE_CHUNK_SIZE` | | 官方 18MB / 自建 256MB | ✔ | 分块大小,支持 `18MB`/`1g` 写法;官方端点上限 19.5MB(受 getFile 20MB 限制),自建上限 1900MB |
| `TGDRIVE_UPLOAD_CONCURRENCY` | | `2` | ✔ | 单个上传 Bot API 实例的并发数;总并发 = 此值 × 上传 Bot 数量 |
| `TGDRIVE_DOWNLOAD_CONCURRENCY` | | `4` | ✔ | MTProto 单个下载流的并行分段数,范围 1~8;「流式播放」中保存后对新请求生效。同一 Bot 的所有存储目标共享最多 8 路在途请求 |
| `TGDRIVE_SEND_VIDEO` | | 官方 `false` / 自建 `true` | ✔ | 视频消息模式:不超过分块大小的 mp4/mov/m4v 以「视频消息」发送(自动带宽高时长),Telegram 里可直接播放,文件内容不变;mkv/webm 仍走文档(Telegram 播放器不支持其常见编码);超过分块大小的视频仍按文档分块存储 |
| `TGDRIVE_TMP_DIR` | | 系统临时目录 | ✔ | 大分块暂存目录,需 ≥ 分块大小×(并发+1) 的空间 |
| `TGDRIVE_LOCAL_DATA_ROOT` | | `/var/lib/telegram-bot-api` | | 自建 Bot API local 模式允许读取的根目录;必须与 Compose 共享卷的容器内路径一致,修改需重启 |
| `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` | | — | ✔* | 在 <https://my.telegram.org> 申请;tgdrive 用于 MTProto Range 流式下载,自建 Bot API 也使用;\*后台可编辑并导出,修改后必须重启/重建两个服务 |
| `TGDRIVE_UID` / `TGDRIVE_GID` | | `1000` / `1000` | | Compose 中 tgdrive 进程的 UID/GID;应对 `./data` 有写权限 |
| `TGDRIVE_BOT_API_GID` | | `101` | | 自建 Bot API 数据文件的组 ID;tgdrive 以补充组身份只读 local 文件和缩略图 |
| `TGDRIVE_LISTEN` | | `0.0.0.0:8080` | | 监听地址(端口由 compose 映射,改需重启) |
| `TGDRIVE_DB_PATH` | | `/data/tgdrive.db`(镜像内) | | SQLite 路径(设置本身存在这个库里) |
| `TGDRIVE_SECRET` | | 自动生成并存库 | | cookie 签名密钥,**仅首次启动时作为初始值**,之后以库中值为准 |
| `TGDRIVE_COOKIE_SECURE` | | `false` | | 通过 HTTPS(含反向代理)访问时设为 `true`,为 Cookie 启用 Secure 并发送 HSTS 响应头;修改需重启 |
| `TGDRIVE_TRUSTED_PROXIES` | | 空 | | 反向代理的 IP 或 CIDR(逗号分隔)。仅来自这些地址的 `X-Forwarded-For` 会被采信,用于登录与提取码限速;不配置时反代后所有人共用一个限速额度;修改需重启 |
| `TGDRIVE_STATIC_DIR` | | `/app/static`(镜像内) | | 前端静态文件目录 |
| `RUST_LOG` | | `info` | | 日志级别 |

上传并发是每个上传 Bot API 实例的并发数;总并发为"设置值 × 上传 Bot 数量"。例如配置 3 个 Bot、并发设置为 1 时,总并发为 3;设置为 2 时,总并发为 6。单块文件按 Bot 冷却状态和轮询分配;需要多个 Telegram 分块的文件统一使用默认 Bot,并在浏览器队列中独占上传通道、单分块串行,完成后再恢复其他任务。

## 网络代理(HTTP / SOCKS5 / usque WARP)

打开「系统设置 → 网络代理」,可以选择「自定义代理」或「usque(Cloudflare WARP)」。切换方式会保留原有自定义代理地址。

**使用内置 usque:** 镜像包含 [usque v4.2.1](https://github.com/Diniboy1123/usque)。选择 usque 后,首次点击「同意条款并初始化 usque」注册 WARP 设备,再打开代理开关并保存。初始化需要连接 Cloudflare 注册服务;隧道默认使用 HTTP/2,通过 TCP/443 连接 Cloudflare,以兼容 QUIC 数据传输受限的网络。网络支持 QUIC 时可在 `.env` 设置 `TGDRIVE_USQUE_TRANSPORT=quic`(UDP/443),然后重建 tgdrive 容器;切换传输方式无需重新注册 WARP。页面显示进程状态,「测试连接」会额外检查 WARP 隧道。进程异常退出会自动重启,连接失败不会回退直连。

注册配置保存在数据库同级的 `usque/config.json`(默认容器 `/data/usque/config.json`,宿主机 `./data/usque/config.json`),重建容器后复用。它包含私钥,请单独妥善备份;数据库备份不包含这个文件。也可在启用前将已有 usque 配置放入该路径,初始化按钮不会覆盖已有文件。usque 只监听容器内 `127.0.0.1:1083`,使用自动生成的认证信息,无需公开端口、TUN 或额外权限。直接运行 Rust 程序时,请自行安装该版本 usque,并通过 `TGDRIVE_USQUE_BIN` 指定二进制路径。

**使用自定义代理:** 填写地址并打开开关,点击「保存并生效」。支持:

- `http://主机:端口`:HTTP 代理;MTProto 使用 HTTP CONNECT 隧道,代理需允许连接 Telegram DC 的端口。
- `socks5://主机:端口`:SOCKS5,HTTP 请求的目标域名由 tgdrive 解析。
- `socks5h://主机:端口`:SOCKS5,目标域名交给代理解析,推荐使用;`socks://` 是此方式的别名。
- 认证格式为 `协议://用户名:密码@主机:端口`,用户名、密码中的 `@`、`:`、`#` 等特殊字符需 URL 编码。HTTP 默认端口为 80,SOCKS5 默认端口为 1080。

「测试连接」不保存配置。连接官方或远端 Bot API 时,使用当前表单中的代理;项目内自建 Bot API 的 Telegram 连接使用已保存的代理,新的代理在保存后生效。保存时会验证当前 Bot API 链路;失败不会覆盖配置,仍可确认后强制保存。关闭代理后保留地址,恢复直连;恢复代理方式或地址时会一并恢复方式、地址与开关,避免留下无效组合。

代理覆盖 tgdrive 发出的 Bot API 请求(含历史存储目标)、MTProto、Bot 收件和 WebDAV / S3 备份。已发出的 HTTP 请求和已打开的 MTProto 下载流继续使用原连接;新请求使用新配置,收件在下一轮轮询切换。HTTP 请求到 `localhost`、`127.0.0.1`、`::1` 和 `telegram-bot-api` 时保持直连。

项目内的 `telegram-bot-api` 也共用这份配置。它的 Telegram TCP 连接由 proxychains 转给 tgdrive 的内部认证入口,再由 tgdrive 使用当前 HTTP CONNECT / SOCKS5 代理连接 Telegram;关闭代理时由 tgdrive 直连。保存或恢复代理设置会断开 Bot API 的旧连接,由 TDLib 自动重连,在途上传或下载可能短暂停顿。代理失败时不会自动绕过代理直连。

内部入口不映射到宿主机端口,也不需要 `NET_ADMIN`、TUN 或 Docker socket。Compose 使用 `TGDRIVE_BOT_API_PROXY_LISTEN=0.0.0.0:1082`、`TGDRIVE_BOT_API_PROXY_CONFIG=/bot-api-control/proxychains.conf` 和共享目录自动建立连接。配置文件只含启动时生成的内部认证密钥,不含上游代理凭据,且不进入镜像。tgdrive 重建后 Bot API 自动重新读取入口配置。请保留这两个环境变量和共享挂载,不要把该端口公开到公网;自行部署到项目外的 Bot API 不受此配置管理。

开发验证:构建两个项目镜像后运行 `node docker/test-shared-proxy.mjs`。测试创建独立的内部网络、空数据库和临时容器,验证代理切换、失败时不回退、恢复配置以及重启联动,结束后清理测试资源,不连接生产实例或 Telegram。

注意:
- 本项目 Compose 中的 `telegram-bot-api` 自动共用代理;其他外部 Bot API 服务及浏览器的网络设置需要分别管理。Bot API 原生 `--proxy` 只代理 webhook,不能替代这里的 Telegram 出站代理。
- Docker 中的 `127.0.0.1` 指容器本身。使用宿主机代理需填写容器可访问的宿主机地址,并让代理监听可访问的接口;不要向公网开放无认证代理。
- tgdrive 使用这里的显式设置,不读取系统 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`。也可在 `.env` 配置 `TGDRIVE_PROXY_PROVIDER`、`TGDRIVE_PROXY_URL` 和 `TGDRIVE_PROXY_ENABLED`,后台覆盖优先。
- 设置接口会隐藏代理用户名和密码,原样回传不会覆盖凭据;修改地址时需重新填写完整认证信息。数据库备份和「导出 .env」仍包含真实凭据,应妥善保管。

## 后台管理详情

手机从「我的」→ **系统设置**(`/admin`);桌面仍可使用右上角用户名菜单。未保存的修改会在离开时二次确认。

- **系统状态**:bot、端点、分块、文件数与占用、元数据库体积、运行时长、待删除任务、收件死信与后台任务健康度
- **Telegram 存储**:bot token、存储频道、接口端点、分块大小、上传并发、视频消息模式
- **网络代理**:HTTP / SOCKS5 开关、地址与连接测试;代理认证信息脱敏展示
- **历史存储迁移**:系统设置的「后台任务」可让主 Bot 在 Telegram 服务端逐条复制其他 Bot 的历史消息,原子切换所有共享分块引用;支持暂停、失败重试和服务重启后继续,用户上传期间自动让路
- **MTProto 流式播放 / 自建 Bot API**:api_id / api_hash,以及切换到自建端点的操作步骤
- **账号**、**高级**(临时目录)、需重启才能改的项(监听地址、数据库路径等)
- **备份与回收站**:Telegram、WebDAV、S3 渠道可分别启用;WebDAV 支持 Basic Auth,S3 支持自定义端点、区域、桶、对象前缀、临时令牌和路径样式
- **Bot 收件**:独立控制 Bot 私聊收件和频道收件,两种入口共用默认、视频、图片、书籍目录;自定义类型名称、一个或多个文件后缀及目标目录。自定义规则优先,多段后缀按更具体的规则匹配;新收件记录保存来源,频道中已登记的上传消息不会重复入库

配置表中标了「可在后台改」的项都能在后台改,**保存后立即生效,不需要重启容器**(api_id / api_hash 除外)。

- **安全网**:改 Telegram 相关配置时会验证 Bot 身份、目标会话以及频道/群组的管理员、发消息和删消息权限;验证失败就拒绝保存(可选择强制保存,给自建端点还没启动的场景用)。分块大小与端点的组合也会一并校验,例如切回官方端点时不允许保留 512MB 的分块
- **导出 .env**:把当前生效配置生成 `.env` 文本,方便同步回宿主机文件
- **维护**:设置多渠道数据库备份、Telegram 备份分块大小(默认采用官方 Bot API 安全值 18MB)、清理上传中断留下的残留、登出所有会话(轮换签名密钥)

> 改账号后所有设备的登录状态立即失效,使用登录密码的 WebDAV 客户端也要同步修改。

### 改配置前请注意

- **换 Bot、端点或存储频道**只影响之后开始的上传。每个分块会记录原 Bot/API/频道,历史文件仍按原目标读取和删除;请保留原 Bot Token、端点和频道权限,否则对应历史文件会不可用。
- **旧数据库首次升级**无法凭 Telegram 消息反推出每个历史分块的原目标,程序会把尚未关联目标的旧分块一次性绑定到升级时的当前配置。如果旧库曾经换过 Bot、端点或频道,请先备份数据库并人工核对。
- **api_id / api_hash 改完不会立刻生效**:tgdrive 在启动时读取它们建立 MTProto 会话,Compose 也把它们传给 telegram-bot-api。保存后要导出 `.env` 覆盖回项目目录,再执行 `docker compose --profile local-bot-api up -d --build tgdrive telegram-bot-api`。

## Range 流式播放

下载接口支持标准单段 HTTP Range。配置 api_id/api_hash 后,文档、音频和视频会直接通过 MTProto 按 512KiB 读取;任意偏移最多只会额外读取前方不足 512KiB 的对齐数据。播放器关闭或请求取消后,服务会停止发出后续 MTProto 请求。若自建 Bot API 的文件已在共享卷缓存中,仍优先本地直读,完全不产生 Telegram 下载流量。照片等不支持的 file_id 以及未配置 MTProto 时才保留原 Bot API 下载路径。

## 重要注意事项

- **务必备份 `./data/tgdrive.db`(SQLite,项目目录下的 `data/` 映射进容器 `/data`)**。Bot API 无法读取频道历史消息,普通上传也不会把虚拟路径写进消息;元数据库一旦丢失,频道里的分块将无法自动找回。库里同时存着后台改过的配置(含 bot token 等明文凭据;登录密码只存哈希),请一并妥善保管;远端备份建议开启备份加密。
- **限流**:Telegram 对单个 bot 发消息到同一频道限流约 20 条/分钟。触发 429 时该 Bot 会按 `retry_after` 自动重试,并把发送间隔自适应翻倍(每个 Bot 独立观测,成功发送后缓慢回落),减少反复撞限造成的整段冷却;大量小文件可配置额外上传 Bot 分流,大文件则建议自建 Bot API 并调大分块。
- **删除**:网页删除可选择移入回收站或直接删除;后者不可恢复。删除文件不会等待 Telegram 消息删除完成;失败任务会持久化到 SQLite,后台按指数退避重试。Telegram 不允许 Bot 删除发出超过 48 小时的消息(频道管理员也不例外),因此删除上传已超过两天的文件时,其频道消息通常会保留;遇到 `message can't be deleted` 的任务直接移出队列,只在每轮汇总日志中计数,如需清理可在 Telegram 客户端手动删除。消息清理默认手动执行,建议切到自动,以便上传残片在 48 小时内被清掉。可在后台系统状态查看队列。
- **后台日志**:系统设置中的「后台日志」会显示消息清理、维护与备份任务的批次完成、限流重试和队列清空状态;容器重启后仍可查看最近记录。完整 HTTP 诊断仍需查看 Docker 日志。
- **不加密**:分块原样存储,频道成员可直接查看文件;需要保密请勿把他人拉进存储频道。
- 未实现(欢迎扩展):多用户、端到端加密。

断点续传任务会在服务端保留 7 天。取消任务会清理已经上传的 Telegram 分块;超过 7 天仍未继续的任务会由后台维护自动清理。
