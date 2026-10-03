# 性能指标与诊断

`GET /api/metrics` 仍要求登录或 Basic Auth，不新增公开监控入口。原有指标保留兼容。新增计数全部为进程内状态，重启归零，不持久化文件、用户、票据或 Bot 标识。

## HTTP 与正文

| 名称 | 类型 | 含义 |
| --- | --- | --- |
| `tgdrive_http_headers_seconds` | histogram | 接收请求到响应头生成，按 Axum 注册路由模板分组 |
| `tgdrive_http_body_seconds` | histogram | 响应头生成后到正文读完、错误或丢弃的生命周期 |
| `tgdrive_http_body_active` | gauge | 当前尚未结束的正文数量 |
| `tgdrive_http_body_bytes_total` | counter | 正文交给 HTTP 层的累计字节，每帧更新 |
| `tgdrive_http_body_finished_total` | counter | `outcome=complete/error/cancelled` 的正文结束次数 |

路由标签只使用注册模板（例如 `/api/apps/media/{ticket}`），不使用实际请求路径或查询参数。最多 128 个模板桶，包含 `other` 溢出桶；未匹配请求统一使用 `unmatched`。模板值按 Prometheus 标签规则转义。

正文监测透传原始帧、trailers、长度提示及取消，不缓存或预读内容。`cancelled` 只表示正文没被读完就释放，可能是切片/切书、用户取消、连接断开或上层丢弃，不一定是故障。传给 HTTP 层不等于客户端已收到，也不等于物理网卡传输量。SSE 长连接直到关闭才记录正文耗时，不能和普通接口混合判断卡顿。

直方图桶为 5、10、25、50、100、250、500 毫秒及 1、2.5、5、10、30 秒，另有 `+Inf`。超过 30 秒的播放/SSE 全程分位数可能落在无穷桶；这类流更适合结合字节速率、在途数和结束原因分析。

按路由观察响应头 P95：

```promql
histogram_quantile(0.95,
  sum by (le, route) (rate(tgdrive_http_headers_seconds_bucket[5m]))
)
```

观察输出速率与非正常结束：

```promql
sum by (route) (rate(tgdrive_http_body_bytes_total[5m]))
sum by (route, outcome) (rate(tgdrive_http_body_finished_total{outcome!="complete"}[5m]))
```

## Bot API 与上传等待

- `tgdrive_telegram_attempt_seconds`：已完成的单次 Bot API 尝试耗时，包含 HTTP 请求和响应解析，不含重试退避与排队。
- `tgdrive_telegram_errors_total`：已完成的失败尝试数。
- `tgdrive_telegram_floods_total`：识别为 Telegram 限流的失败尝试数。
- `tgdrive_telegram_retries_total`：已完成的非首次尝试数，不把准备重试却被取消算作已完成重试。
- 以上 `kind` 只有 `api`（通用幂等调用）和 `upload`（sendDocument/sendVideo）；不含 MTProto、直接下载流或其他专用调用。
- `tgdrive_upload_wait_seconds{kind="bot_api"}`：上传竞争发送时隙的实际等待时间，包含中途取消。不等同于前端上传队列等待，也不是预计冷却时间。

```promql
sum by (kind) (rate(tgdrive_telegram_floods_total[5m]))
histogram_quantile(0.95,
  sum by (le, kind) (rate(tgdrive_telegram_attempt_seconds_bucket[5m]))
)
```

## 小 Range 缓存

- `tgdrive_range_cache_hits_total` / `misses_total`：进入小 Range 缓存查找的命中/未命中次数。大 Range 直接流式透传，不计未命中。
- `tgdrive_range_cache_evictions_total`：预算触发逐出的条目数量；替换同键不是逐出。
- `tgdrive_range_cache_entries` / `bytes` / `budget_bytes`：条目数量、正文占用及正文预算。字节数不含键、队列与分配器元数据。

```promql
rate(tgdrive_range_cache_hits_total[5m])
/
(rate(tgdrive_range_cache_hits_total[5m]) + rate(tgdrive_range_cache_misses_total[5m]))
```

无缓存流量时该比例无数据或 NaN，不应直接作为故障。此阶段不改变缓存算法、大小或服务并发，先收集真实负载基线，再决定调参。

## 当前边界

尚未增加数据库连接等待/SQL 延迟指标、MTProto 细分指标、前端首帧遥测或真实网络带宽探针。没有凭本次代码变更宣称吞吐提升。建议分别记录小/大目录、冷/热缓存、正常/弱网下的基线，避免用单一平均耗时判断媒体性能。

## 本轮验证与部署

- `cargo test --locked`：退出 0，320 项通过、2 项按配置忽略（独立浏览器夹具与真实 Telegram 环境测试）。使用临时构建目录，未运行真实 Telegram 测试。
- App 跨库 `npm run test:e2e`：首次执行被中断，仅留下前 13 项日志，不计通过；重新完整执行退出 0，52 项通过、1 项截图生成用例按配置跳过。测试使用临时数据库、内存 Telegram 与合成媒体。
- `git diff --check`：退出 0。
- `docker compose build tgdrive`：退出 0。
- `docker compose up -d --no-deps --force-recreate --wait --wait-timeout 120 tgdrive`：退出 0，容器 healthy。
- `docker compose exec -T tgdrive tgdrive healthcheck`：退出 0，返回 `ok`。
- `docker image prune -f --filter dangling=true`：退出 0，无悬空旧镜像，回收 0 B。

未执行生产负载压测、真机验收或监控系统接入；不宣称 P95 已改善。App 当前发布版本不受本轮主项目变更影响。
