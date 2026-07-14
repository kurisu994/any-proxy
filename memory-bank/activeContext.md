# 活跃上下文

更新时间：2026-07-14

## 当前状态

M0 安全连接器 spike 已按 Git 历史和 CHANGELOG 标记完成：URL 解析、地址 deny-list、系统/假 Resolver、可注入 Dialer、检查与连接绑定、重定向状态机、本地 HTTP(S) fixture 和 TLS spike 都已进入代码。最新 M0 修复提交是 `aec391f`，补充了并发连接映射、宿主接口刷新、`DENY_CIDRS` 测试和文档修正。

当前产物仍是 library-only 安全内核，不是可运行代理。M1 和 M2 均未开始。README/DESIGN 包含目标态内容，判断“已实现”时必须以 Cargo targets 和 `src/` 为准。

本轮新增贡献指南 `AGENTS.md` 和六文件记忆银行，尚未提交。未修改 Rust 源码。

## 活跃文件

- `src/target.rs`：目标 URL 解析与 authority 规范化。
- `src/resolver.rs`：当前安全策略与最主要的设计漂移所在。
- `src/connector.rs`：M0 核心安全不变量和并发测试。
- `src/redirect.rs`：逐跳策略和 Authorization 清理决策。
- `tests/integration.rs`、`tests/tls_spike.rs`、`tests/fixture.rs`：本地集成证据。
- `DESIGN.md`、`README.md`、`CHANGELOG.md`：目标态、用户说明和已交付记录。
- `AGENTS.md`：贡献流程与安全规则。

## 已确认决策

- Rust 单体 Relay，保持单 crate；安全连接器是不可绕过的核心边界。
- 默认匿名、无 Token/Origin/目标域名白名单、无按调用方硬额度。
- 允许任意公共 HTTP(S) URL 和端口 `1..=65535`，接受公共端口扫描与带宽滥用风险。
- DNS 全量校验、固定 SocketAddr、peer 复核和重定向逐跳校验必须保留。
- M0 关闭连接池；M1 只有证明 authority 与已验证 peer 绑定后才可启用复用。
- 公网 HTTPS 由外部反向代理终止；核心服务计划只监听 HTTP。

## 下一步

1. 在开始 M1 前先处理或明确接受实现与文档漂移：未知地址默认允许、接口刷新未调度、非法 `DENY_CIDRS` 静默忽略。
2. 为 M1 增加可执行入口、配置解析、Axum Router、代理编排、CORS/header 清理、稳定 JSON 错误和结构化日志。
3. 用 Hyper/Hyper-Rustls 把已验证的固定 TCP 连接真正交给 HTTP client，保留原始 hostname 的 SNI/Host。
4. 实现流式 Body、取消传播、idle timeout、并发上限和优雅关闭，并补足本地集成测试。
5. 修正 README 中当前不可运行的源码/Docker说明；若保留公网 E2E 命令，则新增实际 `#[ignore]` 测试。

## 阻塞与风险

- `AddressPolicy` 未命中 deny-list 时默认允许，与 DESIGN 的 fail-closed 承诺冲突，是进入完整公网服务前的首要安全决策。
- 当前 `TcpDialer` 连接后立即丢弃 stream，尚未证明与真实 Hyper client 的连接交接。
- 宿主接口只支持手动刷新，没有启动加载与 60 秒调度。
- 当前无运行时配置校验、资源上限、流式转发、日志隐私或容器加固实现。
- 仅生成文档，当前会话没有重新运行 Cargo 构建或测试；不要把历史测试代码等同于本轮测试已通过。
