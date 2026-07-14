# 活跃上下文

更新时间：2026-07-14

## 当前状态

M1 完整 Relay 已实现并提交（`0e4cc1d`）。项目从 M0 的 library-only 安全内核升级为可运行的代理服务，包含 binary target、Axum Router、CORS/header 清理、流式 Body 桥接、配置系统、结构化日志、Docker 和 112 个测试。

M0 遗留的安全收口项已在 M1 开头完成：
- AddressPolicy 从 fail-open 改为 fail-closed（IPv6 仅 `2000::/3`，IPv4 补全特殊用途 CIDR）。
- 宿主接口地址 60 秒自动刷新调度已实现（`spawn_host_refresh` + `RwLock`）。
- `Dialer` 返回真实 stream，Connector 内部完成 TLS 握手。

当前所有 fmt/clippy/test CI 三项检查通过。M2 尚未开始。

## 活跃文件

- `src/main.rs`：进程入口、配置加载、宿主接口刷新与优雅关闭。
- `src/app.rs`：Axum Router 与 Tower 并发限制。
- `src/proxy.rs`：代理编排、重定向跟随与流式 Body 桥接。
- `src/config.rs`：16 项环境变量配置。
- `src/headers.rs`：hop-by-hop 清理与 CORS。
- `src/error.rs`：JSON 错误响应、健康检查与首页。
- `src/telemetry.rs`：tracing 与 request_id。
- `src/connector.rs`：安全连接 + TLS 握手 + BoxStream。
- `src/resolver.rs`：fail-closed AddressPolicy + 宿主接口刷新。
- `tests/relay.rs`：M1 端到端集成测试（10 个）。

## 已确认决策

- Rust 单体 Relay，保持单 crate；安全连接器是不可绕过的核心边界。
- 默认匿名、无 Token/Origin/目标域名白名单、无按调用方硬额度。
- 允许任意公共 HTTP(S) URL 和端口 `1..=65535`，接受公共端口扫描与带宽滥用风险。
- DNS 全量校验、固定 SocketAddr、peer 复核和重定向逐跳校验必须保留。
- M1 关闭连接池；M2 只有证明 authority 与已验证 peer 绑定后才可启用复用。
- 公网 HTTPS 由外部反向代理终止；核心服务只监听 HTTP。
- AddressPolicy fail-closed：未知地址默认拒绝。

## 下一步

1. M2：多架构 Docker 镜像（Linux amd64 + arm64），CI 构建。
2. M2：GitHub Release 附带二进制压缩包和 SHA-256 校验文件。
3. M2：SBOM、provenance、`cargo deny` 许可证/安全公告检查。
4. M2：Prometheus exporter、可选 Caddy Compose 示例。
5. M2：容器启动测试和公开运维文档。

## 阻塞与风险

- M1 默认关闭连接池，每次请求走完整 resolve → validate → dial 流程，性能有改进空间。
- 上游 idle timeout 和取消传播的精细控制尚未完全实现（DESIGN Section 6 的逐 frame idle 超时）。
- 公网匿名无额度控制的风险没有技术消除，文档状态保持 `DONE_WITH_CONCERNS`。
- 容器无法可靠发现 NAT hairpin 对应的宿主公网地址，需部署者通过 `DENY_CIDRS` 补充。
