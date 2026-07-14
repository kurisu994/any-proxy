# Repository Guidelines

## 项目结构与模块组织

本仓库是 Rust 2021 edition 的单 crate 项目，最低支持 Rust 1.75。M1 已实现完整 Relay，包含 library target 和 binary target，可通过 `cargo run` 启动代理服务。

- `src/main.rs`：进程入口、配置加载、宿主接口刷新调度与 SIGTERM 优雅关闭。
- `src/app.rs`：Axum Router 与 Tower 并发上限中间件。
- `src/proxy.rs`：核心代理编排，集成 Connector、重定向状态机与流式 Body 桥接。
- `src/config.rs`：环境变量解析、校验与 typed Config。
- `src/headers.rs`：请求/响应 header 清理与 CORS 预检/响应头。
- `src/error.rs`：稳定 JSON 错误响应（含 CORS + request_id）、健康检查与首页。
- `src/telemetry.rs`：tracing 初始化、request_id 生成与隐私过滤日志。
- `src/target.rs`：目标 URL、协议、主机和端口解析与规范化。
- `src/resolver.rs`：DNS 解析、fail-closed 公网地址策略和连接地址固定。
- `src/connector.rs`：在一次调用中完成 resolve、全量校验、dial 和 TLS 握手。
- `src/redirect.rs`：重定向状态机、降级拦截与逐跳复查。
- `tests/`：本地 HTTP/HTTPS fixture、Connector 集成测试、TLS spike 和 M1 端到端 relay 测试。
- `Dockerfile`、`docker-compose.yml`：多阶段构建与单容器部署。
- `DESIGN.md`：安全模型、模块边界、里程碑与测试计划；涉及行为调整前应先核对这里的约束。
- `.github/workflows/ci.yml`：合并门槛，以 `fmt + clippy + test` 为准。

新增代码应放入职责最接近的现有模块。只有形成独立安全边界或清晰领域职责时才新增模块，并在 `src/lib.rs` 暴露必要 API。

## 构建、测试与开发命令

在仓库根目录执行：

```bash
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo run --release
```

- `cargo build`：编译 library、binary 和依赖，适合完成一组修改后的快速验证。
- `cargo fmt --check`：检查 Rustfmt 格式，不修改文件；需要修复时运行 `cargo fmt`。
- `cargo clippy -- -D warnings`：运行静态检查，并将所有 warning 视为错误；与 CI 一致。
- `cargo test`：运行全部测试（单元测试、集成测试和文档测试），当前共 112 个。
- `cargo test <test_name>`：只运行指定测试，适合开发时快速反馈。
- `cargo test --test integration`：只运行 Connector 集成测试。
- `cargo test --test tls_spike`：只运行 TLS fixture 测试。
- `cargo test --test relay`：只运行 M1 端到端集成测试。
- `cargo run --release`：启动代理服务，默认监听 `0.0.0.0:8080`。
- `LISTEN_ADDR=127.0.0.1:9090 cargo run --release`：指定监听地址启动。

提交前至少执行 CI 的三项检查（fmt + clippy + test）。

## 编码与文档规范

- 遵循 Rustfmt 默认格式和现有模块命名；文件、模块、函数、变量使用 `snake_case`，类型与 trait 使用 `UpperCamelCase`。
- 公共类型、trait 和函数使用中文 `///` 文档注释说明用途、安全前提和错误语义；复杂的 DNS、地址校验、连接固定及重定向逻辑补充必要中文行内注释。
- 复用 `ProxyError` 的稳定错误码，不在不同模块创建语义重复的字符串错误。错误与日志不得泄露凭证、完整敏感 header 或不必要的目标数据。
- 保持 `Resolver`、`Dialer`、`AddressPolicy` 等边界可注入、可测试。优先小步修改，不顺手重构无关模块。
- 新增或升级依赖前说明必要性、功能开关和安全影响，并保持 `Cargo.lock` 同步。

## 安全边界

本项目是公网匿名代理，安全正确性高于便利性。任何修改都必须维持以下不变量：

- 只允许规范化后的 HTTP/HTTPS 目标；拒绝 userinfo、非法端口、私网、本机、链路本地、保留地址和云元数据地址。
- 地址策略采用 fail-closed：IPv6 仅允许 `2000::/3`（全局单播），IPv4 通过穷举特殊用途 deny 列表覆盖所有非公网范围。未知地址默认拒绝。
- DNS 解析、全部 A/AAAA 地址校验、地址选择与实际 TCP dial 必须绑定在同一个 `Connector` 调用内。禁止先校验 hostname、再由其他客户端重新解析，以免引入 DNS rebinding。
- DNS 答案只要混入一个非公网地址就整体拒绝，不能挑选其中的公网地址继续连接；实际 `peer_addr` 必须属于本次已验证集合。
- 每次重定向都重新解析并校验，拒绝重定向环、超限跳转和 HTTPS 降级到 HTTP。
- TLS 证书校验和 SNI 使用原始规范化 hostname，TCP 使用已固定的 IP；生产代码不得接受任意证书。
- 不读取 `HTTP_PROXY`、`HTTPS_PROXY` 或 `ALL_PROXY`。`AddressPolicy::allow_all_for_test()`、宽松证书验证器及 loopback fixture 只能出现在测试代码中。

修改上述边界时，PR 必须说明威胁模型变化，并新增"危险目标零 dial"和"允许目标实际连接地址已验证"的回归测试。

## 测试指南

测试使用 Rust 内置测试框架与 `#[tokio::test]`。模块内单元测试放在对应源码的 `#[cfg(test)] mod tests` 中；跨模块或真实 I/O 路径放在 `tests/`。测试函数采用 `test_<行为或场景>` 命名，例如 `test_tls_cert_validation_failure`。

优先使用本地、确定性的 fixture 和注入式假 `Resolver`/`Dialer`，不要让默认测试依赖公网 DNS 或第三方服务。安全修复至少覆盖成功路径、拒绝路径和"未发生 dial"的断言。仓库目前没有数值覆盖率门槛；覆盖关键安全分支比追求行覆盖率更重要。

## Commit 与 Pull Request 指南

现有历史采用中文 Conventional Commits：`类型(可选范围): 动词开头的主题`，例如 `feat(M1): 实现完整 Relay 代理服务`、`fix(M0): 处理代码评审发现项`、`docs: 补充安全流水线`。常用类型包括 `feat`、`fix`、`docs`、`test`、`refactor`、`chore`；提交应聚焦单一目的，不附加生成工具或共同作者署名。

Pull Request 应包含：

- 变更目的、主要实现和不做什么；有关联 issue 时使用链接或关闭语句。
- 对安全边界、公开 API、错误码、依赖或配置的影响；无影响也应明确说明。
- 实际执行的验证命令及结果；公网手动测试需注明环境。
- 行为变化对应的测试。只有涉及可视化文档或未来 UI 时才要求截图。

发起 PR 前确认工作树不含无关文件，并确保 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test` 全部通过。
