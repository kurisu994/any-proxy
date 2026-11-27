---
name: 10-security-connector
description: 安全内核规范：Target URL 解析、fail-closed AddressPolicy、Connector 连接固定与 TLS 握手、重定向状态机
paths:
  - "src/target.rs"
  - "src/resolver.rs"
  - "src/connector.rs"
  - "src/redirect.rs"
  - "tests/integration.rs"
  - "tests/tls_spike.rs"
  - "tests/fixture.rs"
---

# 安全内核与连接边界规范

本组模块构成 `any-proxy` 不可退让的 SSRF 防护内核，安全正确性高于便利性。

## 模块职责

- `src/target.rs`：目标 URL 解析、scheme/userinfo/host/port/`path`/query 规范化与 authority 格式化。
- `src/resolver.rs`：`Resolver` trait、`SystemResolver`、`AddressPolicy`（IPv6 白名单 + IPv4 穷举 deny + `DENY_CIDRS` + `RwLock` 宿主接口刷新）。
- `src/connector.rs`：`Connector<R, D>`、`Dialer` trait、`TcpDialer`、`BoxStream`、顺序 failover 与 TLS 握手。
- `src/redirect.rs`：`RedirectMachine` 重定向状态机、降级阻断、环检测与跨 origin 清理判定。
- `tests/`：本地确定性 `TcpListener` 夹具（`tests/fixture.rs`，target 名 `fixture`）、Connector 集成测试（`tests/integration.rs`，target 名 `integration`）和 TLS 测试（`tests/tls_spike.rs`，target 名 `tls_spike`）。

## Target 解析与规范化 (`src/target.rs`)

- `parse_target` 接收 `OriginalUri` 的 `path_and_query`，**只剥离恰好一个前导 `/`**，不折叠重复斜线、不二次 percent-decode；兼容前置反向代理折叠后的 `/https:/...` 形态（有回归测试锁定）。
- 只接受 `http`、`https`、`ws`、`wss`；拒绝 userinfo、空主机、端口 `0`；允许端口 `1..=65535`；`ws`/`wss` 默认端口分别为 `80`/`443`（普通非升级 HTTP 请求指向 `ws`/`wss` 会在 `proxy.rs` 触网前被 400 拒绝）。
- 处理 IDNA 域名、去除域名尾点，识别 IPv4 与带/不带方括号的 IPv6 字面量。
- 关键输出方法：
  - `authority()`：默认端口（HTTP 80 / HTTPS 443）省略；**IPv6 字面量必须按 RFC 3986 用方括号包裹**（如 `[2606:4700::1]:8443`），否则 `Host` header 与 URL 拼接都会畸形。
  - `request_target()`：返回 `path + query`，供上游 HTTP/1.1 请求行使用。
  - `full_url()`：返回完整规范化 URL（含 query，不含 userinfo/fragment），仅用于环检测与 Location 基准，**严禁打进日志**。
  - `display_safe()`：不含 query，供调试日志安全输出。

## DNS 解析与地址策略 (`src/resolver.rs`)

- `AddressPolicy` 的真实形态：
  - **IPv6 正向白名单（真 fail-closed）**：仅允许 `2000::/3` 全局单播；并在 deny 列表中显式拒绝落在该段内但会嵌入/转换 IPv4 的 6to4（`2002::/16`）、Teredo（`2001::/32`）与 ORCHID（`2001:10::/28`）。
  - **IPv4 穷举 denylist**：覆盖私网（10/8、172.16/12、192.168/16）、链路本地与云元数据（169.254/16）、环回、多播、广播、未指定、CGN（100.64/10）、Class E（240/4）、文档段、基准测试段、IETF 协议分配与废弃 6to4 任播；因 Rust `Ipv4Addr::is_global()` 尚未稳定，不在列表内视为公网。
- **全量校验**：`validate_all` 检查本次解析返回的全部 A/AAAA 地址，**只要混入一个非公网地址即整体拒绝**，禁止从混合答案中挑选公网地址继续。
- **反预言机约束**：被拒 IP 仅记入内部 `tracing::debug!`，`ProxyError::TargetBlocked` 对外 message 绝不回显目标 IP。
- **`DENY_CIDRS` fail-closed**：`with_env_deny_cidrs()` 遇到任一非法 CIDR 必须返回 `Err` 终止启动，不允许静默忽略。
- **宿主接口刷新**：通过 `Arc<RwLock<HashSet<IpAddr>>>` 并发安全维护；`spawn_host_refresh` 立即刷新一次并按周期刷新。`if_addrs::get_if_addrs()` 失败时必须**保留上一次快照**，不可用空集合覆盖。

## 原子连接固定与 TLS (`src/connector.rs`)

- `Connector::connect` 在单次调用内原子完成：
  1. `Resolver::resolve`（套 `dns` 超时）
  2. `AddressPolicy::validate_all`（失败立即返回，**零次 dial**）
  3. 组装候选 `SocketAddr` 列表，顺序 failover 逐个 `Dialer::dial`（每个候选地址各自套 `connect` 超时，避免坏 AAAA 阻塞可用 A）
  4. **复核 `peer_addr` 必须属于本次已验证的 `SocketAddr` 集合**，不匹配则弃用并仅记内部 warn（不回显 `peer_addr`）
  5. HTTPS 目标执行 `tokio-rustls` TLS 握手（套 `tls` 超时），SNI 与证书校验使用原始规范化 `canonical_host`，TCP 走已固定的 IP
- `create_tls_config()` 加载系统根证书；根证书加载为空时必须直接 `exit(1)`，不「假活着」。
- `Connector` 暴露 `policy()` 与 `policy_arc()` 供上游连接池在取出空闲连接时复核 `AddressPolicy::is_allowed(&peer_addr)`；任何新建连接与 WebSocket 升级连接都必须走完本安全流水线。

## 重定向状态机 (`src/redirect.rs`)

- 仅对 `GET`/`HEAD` 且状态码为 `301`/`302`/`303`/`307`/`308` 自动跟随；`304`、`300` 等非重定向 3xx 及 `POST`/`PUT`/`PATCH`/`DELETE` 一律返回 `PassThrough`。
- 使用 `url::Url::join` 按 RFC 3986 解析相对/协议相对/绝对 `Location`，并复用 `parse_target` 校验；缺失、畸形或非 http(s) scheme（如 `mailto:`/`javascript:`）返回 `PassThrough`。
- **强拦截**：阻止 HTTPS 降级到 HTTP；最多跟随 10 跳；以完整 `full_url()`（必须含 `path` + query，避免同源换路径误判）做环检测；跨 origin（scheme + host + port 任一不同）置 `strip_authorization = true`。
- 每一跳回到 `Connector::connect` 重新解析与校验。

## 不可破坏的红线与测试要求

- ❌ 禁止「先校验 hostname，再由高层 HTTP 客户端按 hostname 重新解析」。
- ❌ 禁止读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`。
- ❌ 生产路径禁止接受任意 TLS 证书；`AddressPolicy::allow_all_for_test()`、宽松证书验证器与 loopback fixture 只能用于测试代码。
- ✅ 修改本组安全边界时，必须补充「危险目标零 dial」和「允许目标实际 `peer_addr` 属于已验证集合」的确定性本地回归测试，不依赖公网 DNS。
