# 技术上下文

## 工具链与包信息

- 包名：`any-proxy`
- 当前版本：`0.1.0`
- Rust edition：2021
- MSRV：Rust/Cargo 1.75
- 许可证：MIT
- 仓库：`https://github.com/kurisu994/any-proxy`
- Cargo targets：一个 `lib` target、一个 `bin` target（`src/main.rs`），以及 `fixture`、`integration`、`tls_spike`、`relay` 四个 test target。

## 直接依赖

M0 + M1 实际使用的核心依赖：

- Tokio 1（rt-multi-thread, macros, net, signal, time, io-util）
- url 2：URL 解析
- ipnet 2：CIDR 匹配
- if-addrs 0.13：宿主网络接口枚举
- trait-variant 0.1：Send trait 自动派生
- Serde 1 / serde_json 1：配置与错误响应序列化
- tracing 0.1 / tracing-subscriber 0.3（env-filter, json）：结构化日志
- axum 0.8（http1, tokio, original-uri）：入站 HTTP 服务器
- hyper 1（client, http1）：出站 HTTP/1.1 客户端
- hyper-util 0.1（client, client-legacy, tokio）：Hyper 工具
- http 1：HTTP 类型（HeaderMap, Request, Response）
- http-body-util 0.1：Body 桥接
- futures-util 0.3：异步工具
- tower 0.5（limit, timeout, util）：服务层中间件
- rustls 0.23（ring, std）：TLS 协议
- rustls-native-certs 0.7：系统根证书加载
- tokio-rustls 0.26（ring, tls12）：Tokio TLS 集成

测试依赖：rcgen 0.13（自签名证书生成）。

Release profile 使用 `opt-level=3`、LTO、单 codegen unit 和 strip。

## 常用命令

```bash
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo run --release
LISTEN_ADDR=127.0.0.1:9090 cargo run --release
cargo test --test relay
cargo test --test integration
cargo test --test tls_spike
```

GitHub Actions 在 push/PR 到 `main` 时执行 `cargo fmt --check`、`cargo clippy -- -D warnings` 和 `cargo test`。

当前可以使用 `cargo run --release` 启动服务，默认监听 `0.0.0.0:8080`。Docker 部署使用 `docker compose up -d`。

## 配置事实

代码从环境变量读取以下 16 项配置（均有默认值和上下界校验）：

- `LISTEN_ADDR`：监听地址（默认 `0.0.0.0:8080`）
- `DENY_CIDRS`：额外拒绝 CIDR（逗号分隔）
- `MAX_CONCURRENT_REQUESTS`：并发上限（默认 256，1-1000000）
- `MAX_HTTP1_BUFFER_BYTES`：HTTP/1 buffer（默认 65536，1K-1M）
- `MAX_HEADERS_COUNT`：header 数量上限（默认 100，1-1000）
- `MAX_URI_BYTES`：URI 字节上限（默认 16384，256-1M）
- `DNS_TIMEOUT`：DNS 超时（默认 5s，1-300s）
- `CONNECT_TIMEOUT`：TCP 连接超时（默认 10s，1-300s）
- `TLS_TIMEOUT`：TLS 握手超时（默认 10s，1-300s）
- `UPSTREAM_HEADERS_TIMEOUT`：上游 headers 等待（默认 30s，1-600s）
- `UPLOAD_IDLE_TIMEOUT`：上传空闲超时（默认 30s，1-600s）
- `UPSTREAM_BODY_IDLE_TIMEOUT`：下载空闲超时（默认 60s，1-3600s）
- `POOL_IDLE_TIMEOUT`：连接池空闲超时（默认 30s，1-3600s）
- `SHUTDOWN_GRACE`：优雅关闭等待（默认 30s，1-300s）
- `HOST_REFRESH_INTERVAL`：宿主接口刷新间隔（默认 60s，1-3600s）
- `RUST_LOG`：日志级别（默认 info）

## 测试技术

- 单元测试：Rust 内置 `#[test]` 与 Tokio `#[tokio::test]`，共 95 个。
- 集成测试：本地 `TcpListener` fixture，不依赖第三方服务。
- TLS：rcgen 生成自签名证书，tokio-rustls 验证 SNI、Host 和证书失败路径。
- 端到端测试：`tests/relay.rs` 启动完整 Axum 服务验证代理转发、CORS、预检和错误路径。
- 可测试性：假 Resolver/Dialer 注入、dial 记录、并发端口映射和 peer 集合断言。
- 当前没有数值覆盖率门槛；`.gitignore` 预留 tarpaulin/cobertura 产物。
- 测试总数：112（95 单元 + 3 集成 + 10 relay + 4 TLS spike）。

## 仓库文件

- `README.md`：用户定位、风险、运行说明、配置和开发命令。
- `DESIGN.md`：完整 M0-M2 设计与安全决策。
- `CHANGELOG.md`：M0/M1 用户可见能力、限制与安全说明。
- `AGENTS.md`：贡献规则和不可破坏的安全约束。
- `Dockerfile`：多阶段构建，运行镜像含非 root 用户和健康检查。
- `docker-compose.yml`：单容器示例。
- `.github/workflows/ci.yml`：当前 CI 门槛。
