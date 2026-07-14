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

## 已完成能力

- library 形式的 M0 安全内核。
- 危险地址在策略失败时零 dial。
- DNS 答案全量校验和实际 peer 集合复核。
- 本地确定性 HTTP/TLS 测试基础设施。
- 稳定代理错误码与 HTTP 状态映射。
- main 分支 push/PR 的 fmt、clippy、test CI 配置。

## 未完成

### M0 收口项

- 设计要求的 fail-closed/IANA 数据维护流程尚未实现；当前是显式 deny-list 未命中默认允许。
- 宿主接口刷新只有方法，没有启动调用和周期调度。
- README 提到的公网 ignored 测试不存在。
- M0 尚未与真实 Hyper client 交接 TCP stream；`TcpDialer` 当前只验证可达性后丢弃连接。

### M1：完整 Relay

- 无 binary entrypoint、Axum Router、HTTP 接口或 healthz。
- 无 CORS 预检、请求/响应 header 清理和 Cookie/Set-Cookie 策略实现。
- 无流式请求/响应 Body、连接池、取消传播、idle timeout、并发上限和优雅关闭。
- 无稳定 JSON 错误响应、结构化日志、隐私过滤、Dockerfile 或 Compose。

### M2：发布供应链

- 无多架构镜像、GitHub Release 二进制、校验文件、SBOM、provenance、cargo deny 或 Prometheus exporter。

## 历史阻碍与结论

- “给匿名代理加 Token”与浏览器前端无法保守长期秘密的目标冲突，最终明确不做调用方访问控制。
- 高层默认 HTTP 客户端难以证明 DNS 校验与实际连接地址一致，因此选择可注入的自定义 Connector 边界。
- 公网匿名、任意公共端口和无硬额度的风险无法由当前产品约束消除，只能通过明确文档和部署侧措施管理。

## 验证记录

仓库历史已包含 M0 单元测试、集成测试和 GitHub Actions 配置。本次记忆银行生成只核对文件、代码和 Git 历史，没有重新执行 `cargo build`、`cargo clippy` 或 `cargo test`。
