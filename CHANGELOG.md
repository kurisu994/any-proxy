# Changelog

本文件记录 any-proxy 的版本变更，遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式。

## [Unreleased]

### Added — M0: 安全连接器 spike

- **URL 解析与校验** (`target.rs`)：从原始 `OriginalUri` 解析绝对 URL，只剥离一个前导 `/`，不二次解码。只允许 `http`/`https`，拒绝 userinfo/空主机/非法端口，允许端口 1-65535。支持 IPv4/IPv6 literal、尾点域名、IDNA。
- **地址分类策略** (`resolver.rs` → `AddressPolicy`)：IANA 特殊用途地址拒绝列表（私网/环回/链路本地/广播/CGN/IPv4-mapped IPv6/IPv6 私网/多播/未指定/NAT64/文档地址）。支持 `DENY_CIDRS` 环境变量和宿主接口地址枚举（`if-addrs`，定期刷新）。混合 A/AAAA 答案含非公网地址时拒绝整个目标。
- **DNS 解析 trait** (`resolver.rs` → `Resolver`)：可注入测试替身的 DNS 解析接口，定义 CNAME 链跟踪到最终 IP 的接口约定（最大深度 10）。
- **系统 DNS 解析器** (`resolver.rs` → `SystemResolver`)：基于 `tokio::net::lookup_host` 的真实 DNS 解析实现，自动跟踪 CNAME 链。
- **安全连接编排** (`connector.rs` → `Connector`)：在一次调用中原子完成 resolve → 全量校验 → select SocketAddr → dial → 记录 peer_addr。危险目标零次 dial。允许目标 dial peer 必须属于同一次已验证地址集合。
- **TCP 连接 trait 与实现** (`connector.rs` → `Dialer`/`TcpDialer`)：可注入测试替身的 TCP 连接接口和基于 `tokio::net::TcpStream` 的真实实现。
- **重定向状态机** (`redirect.rs` → `RedirectMachine`)：最多 10 跳，canonical URL 集合环检测。阻止 HTTPS 降级。GET/HEAD 跟随 301/302/307/308，303 对 GET 保持 GET。POST/PUT/PATCH/DELETE 不跟随。相对 Location 按当前 URL 解析。跨 origin（scheme+host+port 变化）删除 Authorization。
- **错误码与 HTTP 映射** (`lib.rs` → `ProxyError`)：`invalid_target`(400)、`target_blocked`(403)、`dns_failed`/`connect_failed`/`upstream_failed`(502)、`connect_timeout`/`upstream_timeout`(504)。
- **本地 HTTP/HTTPS 测试 fixture** (`tests/fixture.rs`)：使用 `rcgen` 生成自签名证书的本地 HTTPS 服务器，DNS/Resolver/Dialer 均可注入。
- **TLS spike 集成测试** (`tests/tls_spike.rs`)：验证 Connector 建立 TCP 连接到固定 IP，TLS SNI 使用原始 hostname，Host header 使用原始 hostname，证书校验失败时拒绝。
- **端到端集成测试** (`tests/integration.rs`)：HTTP GET、HTTPS GET、重定向链多跳连接。

### Changed

- 无（首版）

### Security

- 安全连接器证明"检查与连接绑定"：DNS 解析、公网地址校验与实际 dial 在同一个 Connector 内原子完成，防止 DNS rebinding。
- 危险目标（私网/环回/链路本地/宿主接口/云元数据/`DENY_CIDRS`）全部产生零次 dial。
- TLS SNI 和证书校验使用原始规范化 hostname，TCP 连接使用 Connector 固定的 IP。

### Known Limitations

- M0 默认关闭上游连接池，每次请求走完整 resolve → validate → dial。
- 宿主接口地址每 60 秒刷新，网络配置变化后最多 60 秒竞态窗口。
- 允许任意公共端口 1-65535，实例可被用于端口扫描。
- 公网匿名无额度控制的风险没有技术消除，文档状态为 `DONE_WITH_CONCERNS`。
