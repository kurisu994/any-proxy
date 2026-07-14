# Changelog

本文件记录 any-proxy 的版本变更，遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式。

## [Unreleased]

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

### Changed

- 无（首版）

### Security

- 安全连接器证明"检查与连接绑定"：DNS 解析、公网地址校验与实际连接在同一个原子步骤内完成，防止 DNS rebinding。
- 危险目标（私网、环回、链路本地、本机接口、云元数据、自定义拒绝网段）全部产生零次 dial。
- TLS SNI 和证书校验使用原始规范化主机名，TCP 连接使用已固定的公网 IP。

### Known Limitations

- M0 默认关闭上游连接池，每次请求走完整解析、校验、连接流程。
- 本机接口地址定期刷新，网络配置变化后存在短暂竞态窗口。
- 允许任意公共端口 1-65535，实例可能被用于端口扫描。
- 公网匿名无额度控制的风险没有技术消除，文档状态为 `DONE_WITH_CONCERNS`。
