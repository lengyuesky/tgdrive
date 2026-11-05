# Mac 飞牛播放器 `/fntv` 前缀验证

日期：2026-11-02。结果：`no-go`，停止完整插件实现，等待用户确认是否调整入口方案。

## 实测范围

- 客户端：本机官方飞牛播放器 `1.12.3 (1123003)`，bundle ID 为 `com.trim.media.ios`。
- 安装入口：`/Applications/Runner 2.app`，属于 iOS/iPadOS 应用在 Mac 的兼容运行路径，不是电视实机或原生 macOS 客户端。
- 服务：独立 Node 探针，仅监听 `127.0.0.1` 的临时端口，不读取 TGDrive 数据库、Telegram、应用账号配置或真实媒体。
- 传输：本地 HTTP，登录页 HTTPS 开关关闭；没有修改系统证书、绕过安全提示或全局抓包。HTTPS 未实测。
- 探针不提供登录协议，只用于观察首个必要请求是否保留前缀；根路径及 `/v` 均返回 404，没有根路径别名、代理或重定向。

## 输入与请求

每种输入均在官方客户端界面中核对地址、合成账号和密码，再点击登录。三次均显示“服务器开小差了，请稍后重试”。

| 输入 | 探针收到的官方客户端请求 | 前缀保留 |
| --- | --- | --- |
| `http://127.0.0.1:49522/fntv` | `GET /trimcon`，`GET /v/api/v1/sys/version` | 否 |
| `127.0.0.1:49522/fntv` | `GET /trimcon`，`GET /v/api/v1/sys/version` | 否 |
| `http://127.0.0.1:49522/fntv/` | `GET /trimcon`，`GET /v/api/v1/sys/version` | 否 |

共收到 6 次官方客户端请求，全部位于 `/fntv` 之外并返回 404。另有 4 次带明确自检标记的请求，只用于检查探针行为，不计入官方客户端兼容证据。

这证明指定版本在上述本地 HTTP 连接的必要握手阶段没有保留输入的路径前缀。不能将“界面能输入 `/fntv`”等同于“实际接口会请求 `/fntv/...`”，也不能通过补齐根路径握手接口来宣称满足专用前缀要求。

未进入实际账号鉴权阶段，没有地址持久化、登录、媒体列表、原文件播放、Range 拖动、音轨或字幕通过结果。该结论不自动覆盖 HTTPS、其他客户端版本、AndroidTV 或 tvOS，也不宣称所有版本永久不支持路径前缀。

## 清理与保留

- 临时探针已停止；进程退出和原端口拒绝连接均已核验。
- 本次未保存的地址、合成账号和密码已清空；密码隐藏及 HTTPS 开关恢复到原始状态。
- 官方客户端的历史登录页显示“无历史登录记录”，没有删除历史条目或修改已有连接。最后停留在空白登录页。
- 未修改官方应用包、TGDrive 既有业务代码或日常服务，未访问真实账号和媒体。
- 初次锁屏中断的报告保留在原临时目录，续验材料独立存放，不把初次自检混入续验请求。

## 证据

本地目录：`tmp/fntv-validation-20261102-594mqj/resume-20261102/`。

- `verification.json`：机器可读结论、三种输入、验证范围和清理结果。
- `requests.jsonl`、`client-paths.json`：去除查询参数后的方法、路径、状态和时间；不记录请求头、正文或凭据。
- `full-url-before-login.png`、`host-port-before-login.png`、`trailing-slash-before-login.png`：三种输入提交前的实际客户端界面。
- `trailing-slash-after-login.png`、`trailing-slash-after-login.txt`：失败提示及当时输入。
- `history-after-probe.png`、`history-after-probe.txt`、`final-login-form.png`、`final-login-form.txt`：无历史记录及清理后的空白表单。
- `prefix-probe.mjs`、`self-test.json`、`shutdown.json`、`evidence-sha256.json`：复现探针、自检、停止记录和证据摘要。

## 后续边界

根据用户已确认的前缀验证门槛，当前不进入完整插件实现，也不擅自增加根路径兼容接口、改用第三方客户端或部署日常服务。若考虑将 `/fntv` 保留为插件管理入口、客户端改用专用域名或端口的根地址，必须先由用户确认；新的地址方案仍需重新验证登录和真实播放，不能把本次握手观察当作兼容通过。
