# 技术上下文

## 工具链与包信息

- 包名：`any-proxy`
- 当前版本：`0.1.0`
- Rust edition：2021
- MSRV：Rust/Cargo 1.75
- 许可证：MIT
- 仓库：`https://github.com/kurisu994/any-proxy`
- Cargo targets：一个 `lib` target，以及 `fixture`、`integration`、`tls_spike` 三个 test target；没有 binary target。

## 直接依赖

当前 M0 实际使用的核心依赖包括：Tokio 1、url 2、ipnet 2、if-addrs 0.13、trait-variant 0.1、Serde 1/serde_json 1，以及 TLS 测试路径使用的 rustls 0.23、tokio-rustls 0.26、rcgen 0.13。

`Cargo.toml` 已预留 M1 依赖：Axum 0.8、Hyper 1、hyper-util 0.1、hyper-rustls 0.27、http-body-util 0.1、futures-util 0.3、Tower 0.5、tracing 0.1、tracing-subscriber 0.3。它们出现在 manifest 中不代表完整 Relay 已实现。精确传递依赖版本由 `Cargo.lock` 锁定。

Release profile 使用 `opt-level=3`、LTO、单 codegen unit 和 strip。

## 常用命令

```bash
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo test <test_name>
cargo test --test integration
cargo test --test tls_spike
```

GitHub Actions 在 push/PR 到 `main` 时执行 `cargo fmt --check`、`cargo clippy -- -D warnings` 和 `cargo test`。公网 E2E 不是当前 CI 门槛；仓库目前也没有实际的 ignored 公网测试。

当前不能使用 `cargo run` 启动服务，因为没有 `src/main.rs` 或 `[[bin]]`。README 中的 `./target/release/any-proxy`、Docker 和 Compose 属于目标态说明，不是当前可运行事实。

## 配置事实

当前代码唯一读取的环境变量是：

- `DENY_CIDRS`：逗号分隔的额外拒绝 CIDR；空项和非法项会被静默忽略，需要显式调用 `AddressPolicy::with_env_deny_cidrs()` 才会加载。

README/DESIGN 中的并发、URI、DNS/TCP/TLS、上游 headers、上传/下载 idle、连接池和关闭超时变量均为 M1 规划，当前代码没有读取它们。

## 测试技术

- 单元测试：Rust 内置 `#[test]` 与 Tokio `#[tokio::test]`。
- 集成测试：本地 `TcpListener` fixture，不依赖第三方服务。
- TLS：rcgen 生成自签名证书，tokio-rustls 验证 SNI、Host 和证书失败路径。
- 可测试性：假 Resolver/Dialer 注入、dial 记录、并发端口映射和 peer 集合断言。
- 当前没有数值覆盖率门槛；`.gitignore` 预留 tarpaulin/cobertura 产物。

## 仓库文件

- `README.md`：用户定位、风险、目标态接口与开发命令。
- `DESIGN.md`：完整 M0-M2 设计与安全决策。
- `CHANGELOG.md`：M0 用户可见能力、限制与安全说明。
- `AGENTS.md`：贡献规则和不可破坏的安全约束。
- `.github/workflows/ci.yml`：当前 CI 门槛。

仓库没有 Makefile、Justfile、Dockerfile、Compose、数据库、迁移或部署配置。
