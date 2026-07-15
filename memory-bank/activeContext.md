# 活跃上下文

更新时间：2026-07-15

## 当前状态

2026-07-15 对 DESIGN.md 跑了 /autoplan（CEO+Eng+DX 三相双声道，Claude+Codex）。评审确认安全内核与架构 CONFIRMED sound，但发现两个"声称完成实则未完成"的 M1 关键缺口，已当场修复并验证：
- **E1（已修）**：`redirect.rs` 的 `canonical_url` 只用 scheme://authority，同源换路径重定向被误判为环返回 403，破坏 M1 重定向标准。改用 `full_url()`，补 2 个回归测试。
- **D1（已修）**：Dockerfile/compose 健康检查用 `wget`，但 debian-slim 无 wget → 容器永久 unhealthy，破坏 M1 标准 7。新增 `any-proxy health-check` 子命令（无新依赖），运行时验证 healthy→0/unhealthy→1/`/healthz`→200。

修复后 fmt/clippy 干净，118 测试全通过。

M1 完整 Relay 已实现并提交（`0e4cc1d`）。项目从 M0 的 library-only 安全内核升级为可运行的代理服务，包含 binary target、Axum Router、CORS/header 清理、流式 Body 桥接、配置系统、结构化日志、Docker 和测试。

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

/autoplan 最终门用户接受了 3 个 User Challenge（均 additive，不反转原始决策），M2 优先级据此调整：

**先做（UC 落地 + 剩余 P1/P2 代码修正）：**
1. C1（UC-1）：增加可选、默认关的安全带 ALLOW_ORIGINS/ALLOW_TARGETS/AUTH_TOKEN + 默认端口限 80/443，显式 `unsafe-open` 全开。
2. C4（UC-3）：M2 前移 abuse kill-switch + 安全默认 + 5 分钟部署体验。
3. D3：随仓库交付 `docker-compose.caddy.yml` + Caddyfile（TLS 是魔法时刻前置条件，不推迟到 M2）。
4. D2：README 首屏写明"必须部署海外出口"前提。
5. 剩余 P2 代码修正：E2（cfg 门控测试逃生口）、E3（RFC Location 解析）、E4（IPv6 authority 方括号）、E5（接线 max_http1_buffer/headers_count knob）、E6（Connector 读真实 peer_addr）、E7（refresh 失败保留旧快照）、E8（deny 6to4/Teredo）。

**再做（原 M2，部分后置）：**
6. C2（UC-2）：Cloudflare Worker/边缘部署模板 + README 形态对比。
7. 多架构 Docker 镜像 + SHA-256 校验（保留）；SBOM/provenance/Prometheus 后置。
8. E11：CI 增加 `cargo deny`/`cargo audit`。

完整 21 项任务清单见 `DESIGN.md` 的 Implementation Tasks 与 `~/.gstack/projects/kurisu994-any-proxy/tasks-*.jsonl`。

## 阻塞与风险

- M1 默认关闭连接池，每次请求走完整 resolve → validate → dial 流程，性能有改进空间。
- 上游 idle timeout 和取消传播的精细控制尚未完全实现（DESIGN Section 6 的逐 frame idle 超时）。
- 公网匿名无额度控制的风险没有技术消除，文档状态保持 `DONE_WITH_CONCERNS`。
- 容器无法可靠发现 NAT hairpin 对应的宿主公网地址，需部署者通过 `DENY_CIDRS` 补充。
