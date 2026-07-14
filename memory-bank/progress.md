# 项目进度

## 时间线

### 2026-07-13：设计确定

- 完成公网匿名 CORS Relay 的问题、用户、风险与里程碑设计。
- 从 Node.js、Go、OpenResty 和 Rust 四条路线中选择 Rust 单体 Relay。
- 明确 URL 前缀式 HTTP(S) 范围，不实现通用隧道或整机代理。
- 接受匿名无硬额度、任意公共端口带来的滥用和带宽风险，状态定为 `DONE_WITH_CONCERNS`。

### 2026-07-14：M0 安全连接器 spike

- S1：初始化 Rust 2021 crate 和模块骨架。
- S2：实现目标 URL、scheme、userinfo、host、端口、IP literal、尾点域名和 query 解析测试。
- S3：实现 `Resolver`、`SystemResolver`、`AddressPolicy`、特殊用途 CIDR 拒绝与混合答案整体拒绝。
- S4：实现可注入 `Dialer` 和 `Connector`，绑定 resolve → validate → dial，并核对实际 peer。
- S5：实现重定向状态机、10 跳上限、环检测、HTTPS 降级拦截和跨 origin Authorization 清理。
- S6：实现本地 HTTP/HTTPS fixture、Connector 集成测试、TLS SNI/Host 和证书拒绝测试。
- 补充 CI、README、CHANGELOG、LICENSE 和安全/重定向 ASCII 图。
- `aec391f` 处理工程评审项：并发连接映射测试、`DENY_CIDRS` 加载、宿主接口刷新测试、authority 复用和文档整理。

### 2026-07-14：M1 完整 Relay

- 安全收口：`AddressPolicy` 从 fail-open 改为 fail-closed（IPv6 仅 `2000::/3`，IPv4 补全保留/文档/基准/IETF/6to4 CIDR）。
- 安全收口：宿主接口地址改用 `RwLock`，新增 `spawn_host_refresh` 60 秒后台刷新。
- 新增 `src/config.rs`：16 项环境变量解析与校验。
- 新增 `src/headers.rs`：请求/响应 hop-by-hop 清理、CORS 预检与统一响应头。
- 新增 `src/error.rs`：JSON 错误响应、健康检查与首页。
- 新增 `src/telemetry.rs`：tracing 初始化、request_id 与隐私过滤日志。
- 改造 `src/connector.rs`：`Dialer` 返回真实 stream、`BoxStream` 包装、TLS 握手。
- 改造 `src/target.rs`：新增 `path` 字段、`request_target()` 和 `full_url()`。
- 新增 `src/proxy.rs`：代理编排、重定向跟随、流式 Body 桥接。
- 新增 `src/app.rs`：Axum Router 与 Tower 并发限制。
- 新增 `src/main.rs`：进程入口、配置加载、优雅关闭。
- 新增 `Dockerfile` 和 `docker-compose.yml`。
- 新增 `tests/relay.rs`：10 个端到端集成测试。
- `0e4cc1d` 提交，测试总数 112（95 单元 + 3 集成 + 10 relay + 4 TLS spike）。

## 已完成能力

### M0 安全连接器

- 危险地址在策略失败时零 dial。
- DNS 答案全量校验和实际 peer 集合复核。
- 本地确定性 HTTP/TLS 测试基础设施。
- 稳定代理错误码与 HTTP 状态映射。
- 重定向状态机：10 跳、环检测、HTTPS 降级拦截、跨 origin Authorization 清理。

### M1 完整 Relay

- 可运行的代理服务进程（`cargo run` 或 Docker）。
- URL 前缀式代理转发：GET/HEAD/POST/PUT/PATCH/DELETE 流式转发。
- OPTIONS 预检固定 204 + CORS headers。
- 请求/响应 header 清理与统一 CORS 响应头。
- 稳定 JSON 错误响应（含 CORS + request_id）。
- 405 Method Not Allowed 拒绝不支持的方法。
- 健康检查 `/healthz` 和首页 `/`。
- 环境变量配置系统（16 项，带校验边界）。
- 进程级并发上限（Tower ConcurrencyLimitLayer）。
- SIGTERM/SIGINT 优雅关闭。
- 结构化日志（tracing，隐私过滤）。
- 宿主接口 60 秒自动刷新。
- HTTPS 目标 TLS 握手（tokio-rustls + 系统根证书）。
- AddressPolicy fail-closed。
- 多阶段 Dockerfile + docker-compose.yml。
- main 分支 push/PR 的 fmt、clippy、test CI 配置。

## 未完成

### M2：发布供应链

- 无多架构镜像（Linux amd64 + arm64）CI 构建。
- 无 GitHub Release 二进制压缩包和 SHA-256 校验文件。
- 无 SBOM、provenance、`cargo deny` 检查。
- 无 Prometheus exporter。
- 无可选 Caddy Compose 示例。
- 无公开运维文档。

### M1 精细化项（可后续补充）

- 上游 idle timeout 和取消传播的逐 frame 精细控制未完全实现。
- 连接池复用未启用（M1 默认关闭，M2 评估启用）。
- 256 MiB 流式响应内存测试未实现。

## 历史阻碍与结论

- "给匿名代理加 Token"与浏览器前端无法保守长期秘密的目标冲突，最终明确不做调用方访问控制。
- 高层默认 HTTP 客户端难以证明 DNS 校验与实际连接地址一致，因此选择可注入的自定义 Connector 边界。
- 公网匿名、任意公共端口和无硬额度的风险无法由当前产品约束消除，只能通过明确文档和部署侧措施管理。
- M0 AddressPolicy 的 fail-open 与 DESIGN 的 fail-closed 承诺冲突，在 M1 开头通过 IPv6 白名单 + IPv4 穷举 deny 列表解决。

## 验证记录

- M0：仓库历史包含单元测试、集成测试和 GitHub Actions 配置。
- M1：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`（112 个）全部通过。二进制启动验证：healthz、首页、OPTIONS 预检、私网拦截、非法协议拒绝和优雅关闭均正常。
