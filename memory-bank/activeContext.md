# 活跃上下文

更新时间：2026-07-25

## 当前状态

**项目定位已拍板：自用 / 实验项目，不按商业产品运作**（2026-07-25）。这是取舍依据：
C2 边缘形态、SBOM/provenance、Prometheus exporter、E2、E6、C4 均按此定位**关闭**（非推迟），
重开条件是「把实例长期公网开放给他人使用」。安全边界不因此放松——公网暴露的风险与用户数无关。

M1 完整 Relay 早已落地。此后依次完成：资源边界（M1/N1）、安全边界（M2/M3/N5/E7/E8/N12）、
代理正确性（N2/N3/N4/N7/N15/N6/E5/E9/N13/N14）、产品真实性（D2/D4/D5）、
安全默认 C1 两批（访问控制 + 启动 gate；出口预算 + 全局限速）。

2026-07-25 本轮：
- **D3 Caddy 公网 HTTPS 编排**（`Caddyfile` + `docker-compose.caddy.yml`），默认自带安全带，
  放宽是显式选择。`caddy validate` 已实跑通过。
- **多架构预构建镜像发布流水线**（`release.yml`）：amd64/arm64 各在原生 runner 构建后合成
  manifest list，发布前校验 git tag 与 `Cargo.toml` version 一致。
- **CI 新增 `docker` 作业**：构建镜像并真正跑起来断言启动 gate、`/healthz` 版本、`health-check` 子命令。
- **修复两个 Dockerfile 缺陷**：Rust 版本低于 MSRV（1.83 < 1.86）；以及更严重的
  **镜像里打的是 `fn main(){}` 空占位二进制**（cargo 按 mtime 判新鲜度，COPY 保留的源文件 mtime
  早于 dummy 预编译产物，遂跳过重编译，全程零构建错误）。若未发现，首个发布镜像将完全不可用。
- **N10 测试盲区收尾**：`proxy.rs` 9 个单测（核心断言「被拒请求不得触网」）、
  `tests/streaming_memory.rs` 用全局分配器断言 256 MiB 传输峰值堆增量 < 32 MiB（已负向验证）。
- **修复 CI 长期失败**：`cargo-audit` 作业缺 `checks/issues` 权限，即使 0 漏洞也报
  `Resource not accessible by integration`。

当前 CI 五个作业全绿（fmt+clippy+test / MSRV / cargo-audit / 镜像冒烟 / Security audit），
174 个测试通过。

## 活跃文件

- `src/main.rs`：进程入口、配置加载、启动 gate、宿主接口刷新与优雅关闭。
- `src/proxy.rs`：代理编排、访问控制、重定向跟随、流式 Body 桥接 + 9 个针对性单测。
- `src/config.rs`：环境变量配置（含 C1 安全带与预算项）。
- `src/budget.rs`：全局出口字节预算 + 令牌桶限速。
- `src/concurrency.rs`：permit 挂到响应 Body 的并发限制。
- `src/connector.rs`：安全连接 + TLS 握手 + 顺序 failover。
- `src/resolver.rs`：fail-closed AddressPolicy + 宿主接口刷新。
- `Dockerfile` / `docker-compose.caddy.yml` / `Caddyfile`：部署形态。
- `.github/workflows/ci.yml` / `release.yml`：CI 与发布流水线。
- `tests/streaming_memory.rs`：流式内存上界回归（单独 test binary，含全局分配器）。

## 已确认决策

- **定位：自用/实验**，工程成熟度不追求领先于实际用量。
- Rust 单体 Relay，保持单 crate；安全连接器是不可绕过的核心边界。
- 默认匿名，但**新增启动 gate**：非 loopback 监听且零防护时拒绝启动。
- 安全带（`ALLOW_TARGETS`/`ALLOW_PORTS`/`ALLOW_ORIGINS`/`AUTH_TOKEN`）可选、默认关、additive。
- 带宽兜底走**不依赖调用方身份**的全局限速与出口预算；放弃 per-user 身份鉴权。
- DNS 全量校验、固定 SocketAddr、peer 复核和重定向逐跳校验必须保留。
- 不启用连接池；公网 HTTPS 由外部反向代理（Caddy）终止。
- AddressPolicy fail-closed：未知地址默认拒绝。

## 下一步

1. **打第一个 tag 发布**：`git tag v0.1.0 && git push origin v0.1.0`，
   随后在 GitHub package 设置里把可见性改为 Public。
2. 留意 `release.yml` 首次运行，特别是 `ubuntu-24.04-arm` runner 能否正常调度。
3. `RUSTSEC-2025-0134`（rustls-pemfile 未维护）为传递依赖告警，等上游升级，不阻塞。

## 阻塞与风险

- 镜像尚未发布，`docker-compose.caddy.yml` 默认引用的 `ghcr.io/kurisu994/any-proxy:0.1.0`
  在发版前拉不到，需用 `ANY_PROXY_IMAGE=any-proxy:dev` + 本地构建。
- 每次请求走完整 resolve → validate → dial，无连接池，性能有改进空间。
- 容器无法可靠发现 NAT hairpin 对应的宿主公网地址，需部署者通过 `DENY_CIDRS` 补充。
- 宿主接口地址每 60 秒刷新，网络配置变化后最多 60 秒竞态窗口。
