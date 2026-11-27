---
name: 13-build-deploy-ci
description: 构建依赖、容器镜像、Caddy/deploy.sh 公网部署与 CI/Release 流水线规范
paths:
  - "Cargo.toml"
  - "Dockerfile"
  - "docker-compose*.yml"
  - "Caddyfile"
  - "deploy.sh"
  - ".env.example"
  - ".github/workflows/**"
---

# 构建、依赖、容器部署与 CI/Release 规范

本规范约束构建工具链、依赖管理、Docker 镜像、Caddy 公网编排、`deploy.sh` 脚本及 GitHub Actions 流水线。

## 工具链与依赖 (`Cargo.toml`)

- **Crate**：`any-proxy` `0.1.0`，Rust edition 2021，**MSRV `1.86`**（受 `url`/`icu` 传递依赖约束，CI `msrv` 作业强制锁定；历史文档提到的 1.75 已过时）。
- **Targets**：1 个 `lib`、1 个 `bin`（`src/main.rs`），以及 `fixture`、`integration`、`tls_spike`、`relay`、`concurrency`、`pool_and_ws`、`streaming_memory` 七个测试文件，使用 `#[test]` 与 `#[tokio::test]`，当前共 206 个测试（默认通过 205 个，另有 1 个默认 ignore 的 35 秒长传输回归）。`.gitignore` 忽略 `.env`、`/target` 及覆盖率产物。
- **Release Profile**：`opt-level=3`、`lto = true`、`codegen-units = 1`、`strip = true`。
- **核心直接依赖**：`tokio`、`url`、`ipnet`、`if-addrs`、`trait-variant`、`serde`/`serde_json`、`tracing`/`tracing-subscriber`、`http`、`http-body`、`bytes`、`axum`、`hyper`、`hyper-util`（仅 `tokio` feature，不启 `client-legacy`）、`rustls`（ring）、`rustls-native-certs`、`tokio-rustls`、`http-body-util`；dev 依赖含 `rcgen` 与 `tokio/test-util`。早期未使用的 `tower`、`futures-util`、`rustls-pemfile` 直接依赖已移除。
- **已知传递依赖告警**：`RUSTSEC-2025-0134`（`rustls-pemfile` 未维护）来自 `rustls-native-certs 0.7`，仅 informational、0 漏洞，不阻塞；待上游升级。新增或升级依赖须同步更新 `Cargo.lock` 并说明安全影响。

## 常用开发命令与提交流程

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run --release
```

- **Clippy 范围**：CI 已从旧版 `cargo clippy -- -D warnings` 升级为 `cargo clippy --all-targets --locked -- -D warnings`，测试代码的 lint 同样会被拦截。
- **提交流程**：直接在 `main` 分支提交（不建功能分支、不走 PR），提交前确保 `fmt + clippy + test` 三项全绿；采用中文 Conventional Commits。

## CI 流水线 (`.github/workflows/ci.yml`)

push / PR 到 `main` 时运行四个作业：

1. **`check`**：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --locked`。
2. **`msrv`**：在 Rust 1.86 下执行 `cargo check --locked`，防 MSRV 漂移。
3. **`audit`（`cargo-audit`）**：运行 `rustsec/audit-check@v2`。**必须保留 `permissions` 里的 `checks: write` 与 `issues: write`**——此前缺少 `checks/issues` 权限时，即使 0 漏洞也会报 `Resource not accessible by integration`。
4. **`docker`**：构建 `any-proxy:ci` 并真正启动容器做冒烟验证：
   - 无防护监听 `0.0.0.0:8080` 必须以退出码 1 退出且输出含「拒绝启动」（禁止仅判非零退出，防镜像拉取失败/OOM 假阳性）；
   - 带 `ALLOW_TARGETS` 启动后 `/healthz` 返回 `"status":"ok"` 且 `"version"` 与 `Cargo.toml` 一致；
   - `docker exec ... any-proxy health-check` 返回 0。

## 多架构发布流水线 (`.github/workflows/release.yml`)

- **触发方式**：推送 SemVer tag（如 `git tag v0.1.0 && git push origin v0.1.0`）。
- **版本强校验**：`verify-version` 对比 git tag 与 `Cargo.toml` 的 `version`，不一致直接失败，避免 `/healthz` 回显版本与镜像 tag 脱节。
- **原生多架构双推**：`linux/amd64`（`ubuntu-latest`）与 `linux/arm64`（`ubuntu-24.04-arm`）各自原生编译并按 digest 同时推向 `docker.io/kurisu003/any-proxy`（主源，需仓库 Secret `DOCKERHUB_TOKEN`）与 `ghcr.io/kurisu994/any-proxy`（备用源），`provenance: false`；`merge` 作业再分别为两个 registry 合成 manifest list，生成 `x.y.z`、`x.y`、`latest`（0.x 不生成 major tag）。
- **关键坑点**：导出 digest 文件名必须剥掉 `sha256:` 前缀（`upload-artifact` 拒收含冒号路径，`v0.1.0` 首发在 `51d472d` 修复）。首次发版需确认 Docker Hub 与 GHCR 包可见性为 Public。
- **按自用定位不做**：`cargo deny`、SBOM、provenance 与 GitHub Release 二进制压缩包均关闭。

## Dockerfile 与容器加固 (`Dockerfile` / `docker-compose.yml`)

- **构建基镜像必须 >= MSRV 1.86**（当前 `rust:1.86-bookworm`），与 `Cargo.toml` 同步；运行镜像为 `debian:bookworm-slim` + `ca-certificates` + 非 root 用户 `anyproxy`，无 wget/curl，健康检查走 `["any-proxy", "health-check"]`。
- **mtime 空占位二进制陷阱（极其重要）**：缓存依赖层先创建 `fn main(){}` 与空 `src/lib.rs` 预编译依赖；后续 `COPY src/ src/` 之后**必须执行 `touch src/*.rs`**，否则 COPY 保留的旧 mtime 早于 dummy 产物，cargo 会跳过重编译，把空占位二进制静默打进镜像且零构建错误！
- **单容器与 Caddy 编排通用加固**（`docker compose up -d`）：`read_only: true`、`cap_drop: [ALL]`、`security_opt: ["no-new-privileges:true"]`、`mem_limit: 256m`、`pids_limit: 256`。

## Caddy 公网 HTTPS 与 `deploy.sh`

- **`Caddyfile`**：配置 `flush_interval -1` 禁用响应缓冲，且**不设上游响应超时**（防破坏流式无总时长语义）；改完用 `caddy validate` 校验。注意 Caddy JSON 访问日志默认含完整 URI（含 query），目标 API 靠 query 传密钥时需按需调整。
- **`docker-compose.caddy.yml`**：默认拉固定版本镜像 `kurisu003/any-proxy:0.1.0`（备用 `ghcr.io/kurisu994/any-proxy:0.1.0`，禁止默认用 `latest`；跑本地改动用 `ANY_PROXY_IMAGE=any-proxy:dev`），不在 VPS 上编译 Rust 防 OOM；`any-proxy` 仅 `expose: 8080`，公网入口只有 Caddy；默认保留 `RATE_LIMIT_RPS=10` 与 `MAX_EGRESS_BYTES=10GiB`，并透传大模型超时（`UPSTREAM_HEADERS_TIMEOUT=180s`、`UPSTREAM_BODY_IDLE_TIMEOUT=180s`）、WebSocket（`ALLOW_WEBSOCKET=true`、`WS_IDLE_TIMEOUT=300s`）与连接池（`POOL_MAX_IDLE_PER_HOST=8`）变量。
- **`deploy.sh` 与 `.env.example`**：无需 clone 仓库，缺配置时自动生成 `Caddyfile` 与 `docker-compose.caddy.yml`；取值优先级为「命令行参数 > shell 环境变量 > `.env` > 默认值」；前置拦截 IP 域名、DNS 未指向本机、80/443 占用，主镜像源失败自动切 GHCR；启动后采用**三段快速探测**（容器运行态 → 内部 `health-check` → Caddy ACME 日志 + HTTPS），失败秒退。
- **Shell 兼容坑点**：`deploy.sh` 中紧挨中文字符的变量必须写成 `${VAR}`，否则 macOS 老 bash 3.2 在 `set -u` 下会把中文首字节并入变量名报 unbound variable。
