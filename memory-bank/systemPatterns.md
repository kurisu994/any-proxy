# 系统模式

## 当前架构

M1 完整 Relay 已实现。仓库是 Rust 2021 单 crate，包含 library 和 binary target：

```text
入站请求 (Axum)
  │
  ├─ GET /healthz → 健康检查
  ├─ GET / → 首页
  ├─ OPTIONS /<url> → CORS 预检 (204)
  │
  └─ GET|HEAD|POST|PUT|PATCH|DELETE /<url>
       │
       ▼
  target::parse_target          URL 解析与校验
       │
       ▼
  headers::clean_request_headers  请求 header 清理
       │
       ▼
  Connector::connect
    ├─ Resolver::resolve          DNS 解析 A+AAAA
    ├─ AddressPolicy::validate_all  fail-closed 全量校验
    ├─ Dialer::dial(固定 SocketAddr)  TCP 连接
    ├─ 校验 peer_addr 属于已验证集合
    └─ TLS 握手 (HTTPS 目标, SNI=原始 hostname)
       │
       ▼
  hyper::client::conn::http1     HTTP/1.1 请求发送
       │
       ▼
  RedirectMachine::handle_redirect  重定向决策
    ├─ Follow → 重新走 parse_target → connect
    └─ PassThrough / 非 3xx → 返回响应
       │
       ▼
  headers::clean_response_headers  响应 header 清理
  headers::add_cors_headers        添加 CORS headers
       │
       ▼
  Axum Body (流式桥接)           响应返回调用方
```

### 模块职责

- `src/main.rs`：进程入口、配置加载、宿主接口刷新调度与 SIGTERM 优雅关闭。
- `src/app.rs`：Axum Router 与 Tower 并发上限中间件（泛型支持测试注入）。
- `src/proxy.rs`：核心代理编排，集成 Connector、重定向状态机与流式 Body 桥接。
- `src/config.rs`：16 项环境变量解析、校验与 typed Config。
- `src/headers.rs`：请求/响应 hop-by-hop 清理、CORS 预检与统一响应头。
- `src/error.rs`：稳定 JSON 错误响应（含 CORS + request_id）、健康检查与首页。
- `src/telemetry.rs`：tracing 初始化、request_id 生成与隐私过滤日志。
- `src/target.rs`：URL 解析、scheme/userinfo/host/port/path 校验和 authority 规范化。
- `src/resolver.rs`：`Resolver` trait、`SystemResolver`、fail-closed `AddressPolicy`（IPv6 `2000::/3` 白名单 + IPv4 穷举 deny 列表）、`RwLock` 宿主接口刷新。
- `src/connector.rs`：持有可注入 Resolver、AddressPolicy、Dialer 和 TLS 配置，在单次调用中绑定 resolve → validate → dial → (TLS)。`BoxStream` 包装 TCP/TLS stream。
- `src/redirect.rs`：独立重定向状态机；最多 10 跳、环检测、HTTPS 降级阻断、跨 origin 删除 Authorization。
- `src/lib.rs`：模块入口和稳定 `ProxyError`/HTTP 状态映射。
- `tests/fixture.rs`：本地 HTTP/HTTPS 服务器和测试 CA。
- `tests/relay.rs`：M1 端到端集成测试（10 个）。

## 代码模式

- 使用 trait 注入外部边界：Resolver 和 Dialer 都可替换为假实现，安全属性通过调用记录验证。
- 策略先于副作用：URL 或地址策略失败必须在 dial 前返回；安全测试需显式断言 dial 记录为空。
- 全量地址校验：DNS 结果中任一地址被拒绝时，整个目标失败，不从混合答案中挑选可用地址。
- 连接后复核：Dialer 返回的 `peer_addr` 必须属于本次校验后的 `SocketAddr` 集合。
- fail-closed 地址策略：IPv6 仅允许 `2000::/3`，其余拒绝；IPv4 穷举特殊用途 deny 列表。
- 流式 Body 桥接：入站 Axum Body → Hyper 上游请求 body；上游 Hyper Incoming → Axum Response body。不完整缓冲。
- 稳定错误契约：复用 `ProxyError`，错误码为 `invalid_target`、`target_blocked`、`dns_failed`、`connect_failed`、`upstream_failed`、`connect_timeout`、`upstream_timeout`。
- 测试分层：纯逻辑测试放在源码模块；真实本地 TCP/TLS 路径放在 `tests/`；完整服务端到端放在 `tests/relay.rs`。
- 公共 API 与复杂安全逻辑使用中文文档注释，命名遵循 Rust 约定。

## 不可破坏的约束

- ❌ 不允许"先解析并校验，再让默认客户端按 hostname 重新解析"。
- ❌ 不允许只验证 DNS 结果中的一个地址，或在混合公网/私网答案中继续连接。
- ❌ 不允许绕过 TLS 证书校验；SNI/Host 使用原始规范化 hostname，TCP 使用固定 IP。
- ❌ 不允许跟随 HTTPS → HTTP，或在重定向后跳过完整目标校验。
- ❌ 不读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`。
- ❌ `AddressPolicy::allow_all_for_test()` 和危险证书验证器不能进入生产路径。
- ❌ 不记录 query、Authorization、Cookie、完整 headers 或 Body。
- ❌ 不允许 AddressPolicy 默认允许未知地址（必须 fail-closed）。

## 已知实现与设计差异

M0 遗留的设计差异已在 M1 中全部解决：

- ✅ ~~DESIGN 4.4 要求 fail-closed；M0 是 deny-list 默认允许~~ → M1 已改为 fail-closed（IPv6 `2000::/3` 白名单）。
- ✅ ~~宿主接口没有周期调度~~ → M1 已实现 `spawn_host_refresh` 60 秒后台刷新。
- ✅ ~~`TcpDialer` 丢弃 stream~~ → M1 返回真实 stream 并接入 Hyper client。
- ✅ ~~无可执行 binary~~ → M1 已有 `src/main.rs` 和 `cargo run` 支持。
- ✅ ~~README 描述目标态不可运行~~ → M1 已更新为可运行事实。

### M1 尚未完全实现的 DESIGN 细节

- DESIGN Section 6 的逐 frame idle timeout（`UPLOAD_IDLE_TIMEOUT`、`UPSTREAM_BODY_IDLE_TIMEOUT`）尚未在 Body 桥接层精细实现。
- 连接池复用未启用（M1 默认关闭，待 M2 评估）。
- 256 MiB 流式响应内存测试未实现（M1 成功标准 3）。
