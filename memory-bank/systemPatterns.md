# 系统模式

## 当前架构

仓库当前是 Rust 2021 单 crate、library-only 的 M0 安全内核：

```text
原始 path_and_query
  → target::parse_target
  → Resolver::resolve
  → AddressPolicy::validate_all
  → Connector::connect
  → Dialer::dial(固定 SocketAddr)
  → 校验 peer_addr 属于已验证集合
```

- `src/target.rs`：一次 URL 解析、scheme/userinfo/host/port 校验和 authority 规范化。
- `src/resolver.rs`：`Resolver` trait、`SystemResolver`、显式 deny-list `AddressPolicy` 和 SocketAddr 选择。
- `src/connector.rs`：持有可注入 Resolver、AddressPolicy、Dialer，在单次调用中绑定检查与连接。
- `src/redirect.rs`：独立重定向状态机；最多 10 跳、环检测、HTTPS 降级阻断、跨 origin 删除 Authorization。
- `src/lib.rs`：模块入口和稳定 `ProxyError`/HTTP 状态映射。
- `tests/fixture.rs`：本地 HTTP/HTTPS 服务器和测试 CA；集成测试不依赖公网。

当前没有 `main.rs`、Axum Router、完整 Hyper client、Body 转发、CORS/header 清理、telemetry 或 Docker。`TcpDialer` 在验证连接和 `peer_addr` 后会丢弃 stream，这是 M0 spike 行为。

## 代码模式

- 使用 trait 注入外部边界：Resolver 和 Dialer 都可替换为假实现，安全属性通过调用记录验证。
- 策略先于副作用：URL 或地址策略失败必须在 dial 前返回；安全测试需显式断言 dial 记录为空。
- 全量地址校验：DNS 结果中任一地址被拒绝时，整个目标失败，不从混合答案中挑选可用地址。
- 连接后复核：Dialer 返回的 `peer_addr` 必须属于本次校验后的 `SocketAddr` 集合。
- 稳定错误契约：复用 `ProxyError`，当前错误码为 `invalid_target`、`target_blocked`、`dns_failed`、`connect_failed`、`upstream_failed`、`connect_timeout`、`upstream_timeout`。
- 测试分层：纯逻辑测试放在源码模块；真实本地 TCP/TLS 路径放在 `tests/`。
- 公共 API 与复杂安全逻辑使用中文文档注释，命名遵循 Rust 约定。

## 不可破坏的约束

- ❌ 不允许“先解析并校验，再让默认客户端按 hostname 重新解析”。
- ❌ 不允许只验证 DNS 结果中的一个地址，或在混合公网/私网答案中继续连接。
- ❌ 不允许绕过 TLS 证书校验；SNI/Host 使用原始规范化 hostname，TCP 使用固定 IP。
- ❌ 不允许跟随 HTTPS → HTTP，或在重定向后跳过完整目标校验。
- ❌ 不读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`。
- ❌ `AddressPolicy::allow_all_for_test()` 和危险证书验证器不能进入生产路径。
- ❌ 不记录 query、Authorization、Cookie、完整 headers 或 Body。
- ❌ M1 前不把 README/DESIGN 中的计划模块描述为当前实现。

## 已知实现与设计差异

- DESIGN 4.4 要求未知或无法分类地址“默认拒绝”；当前 `AddressPolicy` 是显式 deny-list，未命中条目默认允许。代码注释已承认该差异，进入 M1 前需决定补全 IANA 数据生成流程还是修改设计承诺。
- DESIGN/README 描述宿主接口每 60 秒刷新；当前只有 `refresh_host_addresses()` 方法，没有启动时调用或周期调度。
- `with_env_deny_cidrs()` 对非法 CIDR 静默忽略；目标态配置要求是否应启动失败尚未落地。
- README 描述可执行服务和大量运行时变量；当前 crate 没有 binary target，代码只读取 `DENY_CIDRS`。
- README 提到 `E2E_PUBLIC=1 ... --ignored`，当前源码没有 `#[ignore]` 公网测试。

## M1 扩展边界

继续实现时保持单 crate，优先新增清晰模块：进程/配置、Axum app、代理编排、header/CORS、错误响应、telemetry。Body 必须流式桥接，不能完整缓冲；连接池只有在 authority 与已验证 peer 语义被测试证明后才能启用。
