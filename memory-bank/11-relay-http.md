---
name: 11-relay-http
description: HTTP Relay 编排规范：Axum 路由、C1 访问控制与不触网拒绝、Header/CORS/Trailer 清理、WebSocket 透明升级与错误日志
paths:
  - "src/app.rs"
  - "src/proxy.rs"
  - "src/headers.rs"
  - "src/error.rs"
  - "src/telemetry.rs"
  - "src/lib.rs"
  - "src/ws.rs"
  - "tests/relay.rs"
---

# HTTP Relay 编排、Header/CORS 与错误契约规范

本组模块负责入站 HTTP 请求接入、访问控制、头清理、流式桥接、统一错误契约与隐私日志。

## 模块职责

- `src/lib.rs`：crate 根入口，暴露公共模块与稳定 `ProxyError` 枚举及其错误码/HTTP 状态码映射。
- `src/app.rs`：`create_app<R, D>` 泛型组装 Axum Router、`ProxyState`、`Budget` 与并发限制中间件，便于测试注入假 `Resolver`/`Dialer`。
- `src/proxy.rs`：核心代理处理器 `proxy_handler`、WebSocket 升级分发与 HTTP 转发/连接池复用/重定向循环 `forward_request`。
- `src/ws.rs`：基于 Hyper 原生 HTTP/1.1 Upgrade 的 WebSocket 透明升级转发（`is_websocket_upgrade_request`、`handle_websocket_upgrade`、`relay_bidirectional`），零新增依赖。
- `src/headers.rs`：请求/响应 hop-by-hop 与敏感头清理（含 WS 升级专用的 `clean_ws_request_headers` / `clean_ws_response_headers`）、CORS 预检与统一响应头、`Host` 重建。
- `src/error.rs`：稳定 JSON 错误响应、405、预检 204、`/healthz` 与 `/` 首页构建。
- `src/telemetry.rs`：`tracing` 初始化、`request_id` 生成与流中止隐私日志。
- `tests/relay.rs`（target 名 `relay`）：端到端 Relay 集成测试（录制式假上游、6 种 method、CORS、预检、C1 安全带、重定向等）。

## 请求编排顺序与「被拒请求不得触网」(`src/proxy.rs`)

所有前置校验必须在进入 `Connector::connect` 之前完成，确保未授权或非法请求产生 **0 次 DNS 解析与 0 次 dial**（防止未授权调用方把代理当 DNS 预言机或端口探测器）：

1. **URI 长度检查**：超 `MAX_URI_BYTES` 返回 400 `invalid_target`。
2. **内置路由豁免**：`/healthz`（返回 `{"status":"ok","version":"<CARGO_PKG_VERSION>"}` + CORS）与 `/`（用法与风险说明 + CORS）豁免后续访问控制。
3. **`AUTH_TOKEN` 鉴权**：配置后校验 `X-Proxy-Token`，缺失或不匹配返回 401 `unauthorized`。
4. **`ALLOW_ORIGINS` 校验**：配置后若请求带 `Origin` 且不在列表内，返回 403 `target_blocked`（不回显 origin）；命中则在最终响应把 `Access-Control-Allow-Origin` 覆写为该具体 origin 并追加 `Vary: Origin`。无 `Origin`（非浏览器请求）不拦截。
5. **方法白名单**：仅允许 `GET`/`HEAD`/`POST`/`PUT`/`PATCH`/`DELETE`/`OPTIONS`，其余返回 405 `method_not_allowed`（含 `Allow` 头）。
6. **`OPTIONS` 预检**：直接返回 204（校验 `Access-Control-Request-Method` 合法且 `Access-Control-Request-Headers` `<= 8 KiB` 且各 token 为合法 RFC header 名，失败返回 400）。
7. **目标解析**：调用 `target::parse_target`，非法返回 400 `invalid_target`。
8. **WebSocket 协议与开关校验**：判定 `ws::is_websocket_upgrade_request`（严格要求 `GET` + `Connection: upgrade` + `Upgrade: websocket` + `Sec-WebSocket-Key` + `Sec-WebSocket-Version: 13`，拒绝 `h2c` 等其他升级）；若 `allow_websocket == false` 返回 403 `target_blocked`；若非升级请求却指向 `ws`/`wss` 返回 400 `invalid_target`。
9. **目标 allowlist**：校验 `ALLOW_TARGETS` 与 `ALLOW_PORTS`，不命中返回 403 `target_blocked`（仅记内部 debug 日志，不对外回显 host/port）。
10. **准入限流与预算**：调用 `Budget::check_admission()`，分别返回 429 `rate_limited` 或 503 `budget_exceeded`。
11. **分流转发**：
    - **WebSocket 升级**：走 `ws::handle_websocket_upgrade`，始终新建 `Connector` 连接并使用 `connection.with_upgrades()`；校验上游 101 含 `Upgrade: websocket` 与 `Sec-WebSocket-Accept`（否则 502）；101 成功时通过 `PermitSlot::take()` 接管并发 permit 移交后台全双工转发任务（支持半关闭、共享 `WS_IDLE_TIMEOUT` 与双向 `Budget::add_egress` 记账）；上游非 101 时按普通响应透传并带 CORS。
    - **HTTP 流式转发**：先尝试从 `UpstreamPool` 取出同源空闲连接并在发送字节前做 `sender.ready().await` 探活，失效则透明回退 `Connector::connect()`；入站 Body 套 `wrap_request_body`，响应头阶段套 `UPSTREAM_HEADERS_TIMEOUT`，响应体套 `wrap_response_body_with_on_complete` 流式回传，仅正常 EOF 且上游支持 Keep-Alive 时归还连接池。

**关键禁止项**：`tokio::spawn(async move { let _ = connection.await; })` 驱动 Hyper 连接时**禁止套总时长 timeout**，否则会静默截断持续流动的长传输。

## Header、CORS 与 Trailer 策略 (`src/headers.rs`)

- **请求清理 (`clean_request_headers`)**：
  1. 解析 `Connection` 头中的逗号分隔 token 并删除所指 headers 及 `Connection` 本身。
  2. 删除固定 RFC 7230 hop-by-hop：`Connection`、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Transfer-Encoding`、`Upgrade`。
  3. 删除固定敏感/代理头：`Host`、`Forwarded`、`Via`、`X-Real-IP`、`Cookie`、`X-Proxy-Request-Id`、`X-Proxy-Token`。
  4. **通配删除**所有 `Proxy-*` 与 `X-Forwarded-*` 前缀 header。
  5. 由 `build_host_header(&target)` 按目标 authority 重建 `Host`；保留 `Authorization` 转发给目标 API。
- **响应清理 (`clean_response_headers` + `add_cors_headers`)**：
  1. 同样按 `Connection` token 与固定 hop-by-hop 清理。
  2. 删除 `Set-Cookie`、全部上游 `Access-Control-*`、`Server`、`X-Powered-By`。
  3. 写入 `Access-Control-Allow-Origin: *`（若 `ALLOW_ORIGINS` 命中则由 `apply_cors_origin` 覆盖）、固定 `Access-Control-Expose-Headers`（含 `X-Proxy-Request-Id`），并用 **`headers.append("vary", ...)` 追加** `Access-Control-Request-Method, Access-Control-Request-Headers`，禁止覆盖上游原有 `Vary`。
- **Trailer 策略**：由 `body_timeout` 层直接丢弃所有 trailer frame，杜绝借 trailer 绕过清理规则。

## 稳定错误契约 (`src/lib.rs` / `src/error.rs`)

响应头发出前的代理错误统一返回 JSON `{"error":{"code":"...","message":"...","request_id":"..."}}`，附带 `X-Proxy-Request-Id` 与 CORS headers；上游返回的合法 4xx/5xx 原样转发；响应头发出后发生 Body 错误则直接中止流。

| HTTP | `ProxyError` / 错误码 | 触发场景 |
|---|---|---|
| 400 | `invalid_target` | URL/协议/userinfo/端口非法、URI 超长、预检头非法 |
| 401 | `unauthorized` | 配置 `AUTH_TOKEN` 后 `X-Proxy-Token` 缺失或错误 |
| 403 | `target_blocked` | 非公网地址、HTTPS 降级、重定向环/超限、不在 allowlist |
| 405 | `method_not_allowed` | `CONNECT`/`TRACE` 等不支持的方法 |
| 429 | `rate_limited` | 超过 `RATE_LIMIT_RPS` 令牌桶 |
| 502 | `dns_failed` / `connect_failed` / `upstream_failed` | DNS 失败或空答案 / TCP 或 TLS 失败 / 上游 HTTP 协议错误 |
| 503 | `service_overloaded` / `budget_exceeded` | 并发满载 / 累计出口字节达 `MAX_EGRESS_BYTES` |
| 504 | `connect_timeout` / `upstream_timeout` | DNS·TCP·TLS 任一阶段超时 / 等待上游响应头超时 |

## 遥测与隐私约束 (`src/telemetry.rs`)

- `request_id` 格式为 `{进程启动纳秒 16 位 hex}-{递增计数器 4 位 hex}`。
- `proxy::log_complete` 仅记录 `request_id`、`method`、`scheme`、`host`、`port`、`status`、`error_code`、`duration_ms`；流完成/中止由 `body_timeout` 关联同一 `request_id` 记录方向与字节数。
- ❌ 严禁在日志或对外错误消息中出现 query、userinfo、headers、`Cookie`、`Authorization`、Body 或解析出的目标 IP。
- ✅ `proxy.rs` 内的单测覆盖 401/403/429/405/URI 超长五条拒网路径并断言零解析、零 dial，改动校验顺序时须维持该不变量。
