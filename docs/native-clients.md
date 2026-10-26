# 原生客户端接口（契约 v1，宿主 0.3.4）

## 职责边界

```text
iOS / 其他原生客户端 ── HTTPS REST / SSE / Range ── tgdrive ── Telegram
```

原生客户端只实现界面、交互、播放阅读及本地传输调度。Telegram 凭据、分块入库、目录树、权限、分享计数、备份恢复和迁移仍由现有 Rust 服务执行。服务端不另起移动业务后端，不把 Telegram 存储逻辑搬到 iOS。

新增接口向后兼容。原有网页 Cookie、WebDAV Basic、插件 API v1/v2 保持不变。旧宿主 0.3.3 **不具备**本页的设备接口；iOS 当前基础版本需要先更新至 0.3.4，尚未实现旧 Cookie 登录降级。源码修改不等于已发布或已部署。

## 认证

- `POST /api/auth/device/login`：JSON `username / password / totp? / device_name`；主密码与 TOTP/恢复码校验和网页登录共用，同源 IP 的失败额度也共用。
- `POST /api/auth/device/refresh`：JSON `refresh_token`；原子消费旧哈希、生成新凭据。**不得自动重放**：任何已消费的刷新凭据再次使用会撤销整个设备会话。
- `POST /api/auth/device/logout`：当前设备 Bearer；撤销当前会话，其他设备不受影响。
- `GET /api/auth/devices`：主账号网页会话或原生设备 Bearer；返回 `items` 和 `current_session_id`。
- `DELETE /api/auth/devices/{id}`：主账号强认证；单设备撤销。

登录与刷新响应（`Cache-Control: no-store`，不发 Cookie）：

```json
{
  "token_type": "Bearer",
  "access_token": "短期签名凭据",
  "access_expires_at": 1780000900,
  "refresh_token": "一次性随机凭据",
  "refresh_expires_at": 1782592000,
  "session_id": "随机设备 ID",
  "username": "当前用户名",
  "server_id": "随机内容命名空间"
}
```

Access 有效期 15 分钟；刷新会话从首次登录起最多 30 天，不滑动延期；最多 32 个设备。单设备最多保留 4096 个已消费刷新哈希，超限要求重新登录。凭据哈希与设备表不触发每日数据备份修订，不写入远程快照。密钥轮换、修改账号、开启 TOTP、退出全部会话均使旧签名失效。进程重启保留有效会话。

原生 Bearer 只对 `/api` 有效，**不授权 `/dav`**。媒体票据、应用会话和分享票据不能冒充主账号 Access Token。安全设置接受主账号网页或原生会话，不接受 Basic/WebDAV 应用密码；数据库恢复确认仍保留密码、TOTP 二次认证及恢复确认文本，不因 Bearer 放行而省略。

原生开启 TOTP 后会话密钥轮换，当前设备也须重新登录；不会换发网页 Cookie。网页开启 TOTP 仍换发当前 Cookie。

## 能力与服务器身份

`GET /api/client/capabilities` 需登录：

- `contract_version: 1`
- `host_version: 0.3.4`
- `server_id`：由 `client_identity` 表持久化的随机命名空间。
- `capabilities`：`auth.device.v1`、`files.entries.v1`、`files.reference.v1`、`media.ticket.v1`、`download.etag.v1`、`uploads.receipts.v1`、`uploads.ticket.v1`、`uploads.conflict-policy.v1`、`apps.rpc.v2`、`events.sse.v1`。
- `limits`：目录分页上限、设备上限及认证 TTL。

`server_id` 用于隔离缓存，不是安全凭据。普通重启保持不变；恢复数据库时随机轮换，防止历史节点 ID / 内容版本与当前本地缓存误绑定。恢复候选中的设备表清空，主签名密钥轮换。

`/api/info` 的 `api_base` 仍是 Telegram 存储端点，**不是**客户端应该连接的网盘地址。

## 文件与媒体

- `GET /api/fs/stat?id=12` 或 `?path=/文档/a.txt`：两者只能提供一个；每次检查节点自身及祖先可见性，可选 `content_version` 校验。返回 `id / path / name / is_dir / size / created_at / modified_at / sha256 / favorite / content_version / has_thumbnail`。
- `/api/fs/entries` 增加 `content_version`；旧 `/api/fs/list` 的条目不变。
- `/api/fs/search` 增加 `id / content_version / sha256`，保持现有筛选和有界结果语义，不假装具备无限游标搜索。
- `POST /api/fs/media-ticket`：仅设备 Access；JSON `id / content_version / kind`。
- `GET /api/media/{ticket}`：无需主账号凭据，按设备、节点、版本和用途受限；每次请求重新核验设备撤销状态和节点可见性。

媒体用途：`preview` 2 小时、`download` 6 小时、`bytes`/`thumbnail` 10 分钟，均不超过设备会话过期时间。返回根相对 URL（如 `/api/media/...`）和 `expires_at`，客户端必须解析为当前服务器同源地址；反代子路径由客户端加在 `/api` 前，不接受服务端外域 URL。

内容版本与应用 RPC 共用 `Node::content_version()` 算法；移动、改名、收藏不会改变文件内容版本，覆盖写的新节点或不同内容会变。目录版本**不代表**子树修订，阅读全库索引仍使用已有 `reading_tree_revision`。

下载与分享流响应新增强 ETag、`private, no-cache`。`If-None-Match` 命中返回 304；Range 携带不匹配的 `If-Range`（包括弱标签或日期）返回完整 200，不能将旧缓存与新内容拼接。客户端必须核对 200/206、ETag 和 Content-Range。已建立的流在设备撤销后不主动截断，下一次请求立即被拒绝。

## 可恢复上传与后台票据

现有 Telegram 分块大小、Bot 选择与并发规则、秒传继续使用原实现。新增的是完成回执、只读结果核实及单会话受限票据；客户端不自行操作 Telegram。

### 会话与结果

- `POST /api/uploads`：使用 `dir/name/size/sha256/overwrite?`，返回 `session/concurrency/bot_count/retry_after`。`overwrite` 省略时为 `true`，保持旧网页行为；原生客户端始终显式传入所选策略。
- `POST /api/fs/instant`：同样接受 `overwrite?`。目标检查、同内容幂等和覆盖操作在同一写事务内执行，不依赖客户端查重快照。
- `GET /api/uploads/{id}`：保留网页成功响应必须含 `session` 的旧契约。会话完成后仍返回 404，**不会**将成功响应替换成缺少 session 的回执结构。
- `GET /api/uploads/{id}/result`：原生结果查询。活动会话返回会话结构，已完成/取消返回 `{receipt}`。
- `GET /api/uploads/lookup?path=...&size=...&sha256=...`：主账号已认证接口，仅定位同路径/大小/SHA 的活动会话，返回 `{id: string|null}`；创建响应丢失时使用，绝不创建节点或覆盖文件。
- `POST /api/uploads/{id}/complete`：保留 `{ok,entry}` 并新增 `receipt`；重复提交返回原回执节点，不重新替换同名文件。
- `DELETE /api/uploads/{id}`：停止并清理未完成会话。七天有效回执存在时重复取消幂等；已完成文件和后来覆盖的文件不被删除。

会话增加 `overwrite`、`in_flight_parts` 与 `uncertain_parts`；确认进度仅以 `uploaded_parts/uploaded_bytes` 为准。claim 与保守不确定标记同一 `BEGIN IMMEDIATE` 事务写入；成功记录分块时一起清除。状态读取使用同一 SQLite 快照，不混合并发提交前后的分块与字节数。进程重启释放 claim，保留不确定标记。

### 批量导入的冲突保护

迁移 `0024_upload_conflict_policy.sql` 持久化每个会话的覆盖策略，历史会话默认 `true`。已有同路径/大小/SHA 会话的策略不能被后续创建请求静默修改；策略不同返回 409。

- `overwrite: false`：创建时拒绝已有同名条目；最终提交时再次在写事务内检查。上传期间由其他客户端新增的同名文件会导致 409，保留未完成会话及已确认分块，不生成成功回执、不删除对方文件。
- `overwrite: true`：保留原先替换同名文件的行为及收藏继承；无论策略如何，文件都不能覆盖目录。
- 上传目标目录及祖先在提交时必须仍可见。目录被移入回收站后返回 409；恢复目录后可重试，不能提交出隐藏祖先下的 ready 文件。
- 秒传采用同样的同名策略；`overwrite: false` 即使目标内容相同也不当作新上传成功。客户端可在查重阶段直接报告跳过。
- 取消冲突会话只删除其上传影子及已知分块，对方同名条目继续保留。`lookup` 只读定位会话，客户端恢复时还需核对返回会话的 `overwrite`。

批量目录规划、照片选择、暂存空间检查和导入报告属于原生客户端职责，服务器仍负责 Telegram 上传和原子提交。

已提交到 Telegram 的请求若结果不确定，必须返回 `telegram_uncertain` 并阻止自动重传。只有明确未发送、成功入库或受监督的单 Bot 取消确认才可解除阻止。多 Bot 池、无可确认监督器或监督器重启失败不得将取消当作安全重传依据。用户取消只能清理已知存储消息，无法保证清理没有返回消息 ID 的未知上游结果。

### 回执

迁移 `0023_upload_receipts.sql` 引入七天完成/取消回执及分块不确定标记。最终节点、覆盖清理任务及完成回执在同一写事务提交；取消回执与未完成节点删除也在同一事务。回执含：

```json
{
  "id": "上传会话 ID",
  "status": "completed",
  "path": "/文档/a.txt",
  "size": 3,
  "sha256": "完整文件 SHA-256",
  "entry": {"id": 12, "name": "a.txt", "is_dir": false, "size": 3, "sha256": "完整文件 SHA-256", "content_version": "稳定内容版本"},
  "expires_at": 1780600000
}
```

取消为 `status: cancelled`、`entry: null`。回执记录的是当时的提交结果，不保证文件仍位于原路径；之后的移动、删除、覆盖不改变原回执。回执过期后 404 不能推断成功。维护任务清理过期回执。

### 上传票据

- `POST /api/uploads/{id}/ticket`：仅设备 Access，返回 `{url: "/api/transfer/{ticket}", expires_at}`。
- `GET /api/transfer/{ticket}/status`：查询绑定会话或回执。
- `PUT /api/transfer/{ticket}/parts/{idx}`：仅上传绑定会话内合法分块。
- `POST /api/transfer/{ticket}/complete`：幂等完成绑定会话。

票据最多 24 小时且不超过设备期限，绑定设备、认证代次、会话、节点及 SHA-256；每次检查设备撤销状态。无更换路径、访问其他会话或创建/删除文件权限。票据不能充当管理 Bearer 或 WebDAV 密码。访问日志的 `/transfer/{ticket}` 与媒体票据一样脱敏。

票据 URL 必须由客户端解析成当前服务同源地址，不将管理员凭据附加到后台请求。iOS 系统文件任务、后续调度和真机后台政策属于客户端验收范围，不由接口声明保证。

## 本轮未搬迁的业务

应用 RPC/CAS、Telegram 存储、权限、分享、备份恢复与迁移继续由当前服务负责。未部署服务、访问生产数据库或测试真实 Telegram 文件。旧网页/DAV 契约回归仍需随每次增强运行。

## 隔离验证

```bash
cargo test --locked --offline api::native::tests
cargo test --locked --offline apps::tests::native_downloads
cargo test --locked --offline api::transfer::tests
cargo test --locked --offline upload_commit_rejects_trashed_parent_and_can_resume_after_restore
cargo test --locked --offline engine::tests::failed_restart_keeps_all_interrupted_parts_uncertain
cargo test --locked --offline snapshot_scrub_removes_credentials_but_keeps_bot_tokens
cargo test --locked --offline confirm_preserves_security_and_only_next_start_switches_database
```

所有夹具使用临时 SQLite、空或内存 Telegram 注册表，不读取 `.env`。Swift/Xcode 联调工具可显式启动 ignored 测试 `apps::tests::native_client_http::native_client_fixture`，仅绑定随机 loopback 端口，并要求先识别 `/__native_fixture` 标记再使用测试凭据；不启动正常服务、不连接 Telegram。完整回归与 iOS 验证记录在客户端仓库 `docs/verification.md` 维护。
