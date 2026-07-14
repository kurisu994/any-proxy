# Changelog

本文件记录 any-proxy 的版本变更，遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式。

## [Unreleased]

### Added — M1: 完整 Relay

- 可运行的代理服务进程，支持 `cargo run` 或 Docker 启动，默认监听 `0.0.0.0:8080`。
- URL 前缀式代理转发：GET、HEAD、POST、PUT、PATCH、DELETE 六个方法流式转发到上游 HTTP(S) 目标。
- OPTIONS 预检固定返回 204 No Content，回显通过校验的请求方法和 header 名。
- GET/HEAD 自动跟随重定向（最多 10 跳），POST/PUT/PATCH/DELETE 的 3xx 原样返回调用方。
- 请求/响应 header 清理：移除 hop-by-hop headers、`Connection` 点名 headers、代理控制头和上游 CORS headers。
- 统一 CORS 响应头：`Access-Control-Allow-Origin: *`、固定 `Access-Control-Expose-Headers` 和 `Vary`。
- 稳定 JSON 错误响应：包含 `code`、`message` 和 `request_id`，所有错误响应添加 CORS headers。
- 405 Method Not Allowed 响应不支持的方法（CONNECT、TRACE 等）。
- `/healthz` 健康检查和 `/` 用法说明首页。
- 环境变量配置系统：支持 16 项运行时配置（并发、URI、超时、关闭等），所有值有默认值和上下界校验。
- 进程级并发上限由 Tower `ConcurrencyLimitLayer` 在解析 Body 前获取 permit。
- SIGTERM/SIGINT 优雅关闭：停止接收新连接，等待活跃连接完成。
- 结构化日志（tracing）：记录 request_id、方法、状态码和持续时间，不记录 query、headers、Cookie 或 Body。
- 宿主网络接口地址 60 秒自动刷新，后台任务通过 `RwLock` 并发安全更新。
- HTTPS 目标在 Connector 内部完成 TLS 握手，使用 tokio-rustls 和系统根证书。
- 多阶段 Dockerfile：运行镜像只包含二进制、CA 证书和非 root 用户。
- docker-compose.yml：单容器示例，含健康检查。
- 10 个端到端集成测试覆盖 M1 成功标准：健康检查、首页、代理转发、CORS、预检、405、4xx 原样转发、HEAD、非法目标和私网拦截。

### Changed — M1

- `AddressPolicy` 从 fail-open（默认允许）改为 fail-closed（默认拒绝）：IPv6 仅允许 `2000::/3` 全局单播，IPv4 补全保留地址、文档地址、基准测试、IETF 协议分配和 6to4 废弃任播 CIDR。
- `Dialer` trait 返回真实 stream（`BoxStream` 包装），不再连接后丢弃；`TcpDialer` 返回 `TcpStream`。
- `Connector` 新增 `with_tls` 构造和 TLS 握手逻辑，SNI 使用原始规范化主机名。
- `Target` 新增 `path` 字段和 `request_target()`、`full_url()` 方法。
- 宿主接口地址改用 `Arc<RwLock<HashSet>>` 支持后台刷新，新增 `spawn_host_refresh` 方法。
- `SystemResolver` 和 `TcpDialer` 实现 `Clone`。

### Security — M1

- 地址策略 fail-closed：未知或无法分类的 IPv6 地址默认拒绝，满足 DESIGN.md 4.4 的"默认拒绝"要求。
- TLS 证书校验和 SNI 使用原始规范化主机名，TCP 连接使用 Connector 已固定的 IP。
- 重定向跨 origin 时自动删除 `Authorization` header。
- 日志和错误响应不包含 URL query、Authorization、Cookie、headers 或 Body。

### Added — M0: 安全连接器 spike

- 解析 URL 前缀形式的代理目标，只接受 `http`/`https`，拒绝 userinfo、空主机和非法端口，公共端口 1-65535 可用。
- 解析 IPv4/IPv6 地址字面量、尾点域名和 IDNA 域名。
- 对目标地址进行分类校验，拒绝私网、环回、链路本地、广播、CGN、IPv4-mapped IPv6、IPv6 私网/多播/未指定、NAT64、文档地址和云元数据地址。
- 支持通过 `DENY_CIDRS` 环境变量追加自定义拒绝网段，并定期枚举本机网络接口加入拒绝集合。
- 对域名同时解析 A 与 AAAA 记录；只要答案中出现一个非公网地址，就拒绝整个目标，不会从混合答案中挑选地址继续连接。
- 安全连接器在同一调用中完成 DNS 解析、全量地址校验、固定 SocketAddr 与实际 TCP 连接，并记录最终 peer 地址；危险目标在建立任何上游连接前返回错误。
- 可注入的 DNS 解析器和 TCP 连接器接口，便于测试与替换底层实现。
- 重定向状态机：最多跟随 10 跳，检测重定向环，阻止 HTTPS 降级到 HTTP，非 GET/HEAD 请求的 3xx 不自动跟随，跨 origin 重定向删除 Authorization。
- 定义稳定代理错误码与 HTTP 状态码映射：`invalid_target`(400)、`target_blocked`(403)、`dns_failed`/`connect_failed`/`upstream_failed`(502)、`connect_timeout`/`upstream_timeout`(504)。
- 提供本地 HTTP/HTTPS 测试 fixture 与 TLS spike 验证，HTTPS 使用自签名测试证书，SNI 和 Host 仍使用原始主机名。

### Security — M0

- 安全连接器证明"检查与连接绑定"：DNS 解析、公网地址校验与实际连接在同一个原子步骤内完成，防止 DNS rebinding。
- 危险目标（私网、环回、链路本地、本机接口、云元数据、自定义拒绝网段）全部产生零次 dial。
- TLS SNI 和证书校验使用原始规范化主机名，TCP 连接使用已固定的公网 IP。

### Known Limitations

- M1 默认关闭上游连接池，每次请求走完整解析、校验、连接流程。
- 本机接口地址定期刷新，网络配置变化后存在短暂竞态窗口。
- 允许任意公共端口 1-65535，实例可能被用于端口扫描。
- 公网匿名无额度控制的风险没有技术消除，文档状态为 `DONE_WITH_CONCERNS`。
- 上游 idle timeout 和取消传播的精细控制尚未完全实现。
- 公网 HTTPS 由外部反向代理终止；核心服务只监听 HTTP。
