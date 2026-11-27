---
name: 12-runtime-resources
description: 运行时资源边界规范：环境变量配置与启动 gate、流生命周期并发 permit、逐 frame idle timeout、全局预算限速与优雅关闭
paths:
  - "src/main.rs"
  - "src/config.rs"
  - "src/concurrency.rs"
  - "src/body_timeout.rs"
  - "src/budget.rs"
  - "tests/concurrency.rs"
  - "tests/streaming_memory.rs"
---

# 运行时配置、资源边界与生命周期规范

本组模块负责配置解析、启动安全门控、进程级并发上限、逐 frame 流空闲超时、全局流量兜底与优雅关闭。

## 模块职责

- `src/main.rs`：进程入口、`health-check` 子命令、启动 gate、宿主接口刷新调度与带强退看门狗的 SIGTERM/SIGINT 优雅关闭。
- `src/config.rs`：环境变量解析、上下界校验、`Config` 结构体及 allowlist 匹配方法。
- `src/concurrency.rs`：基于 `Semaphore` 与 `GuardedBody` 的进程级并发上限中间件。
- `src/body_timeout.rs`：`IdleTimeoutBody<B>` 逐 frame 空闲超时、trailer 丢弃、字节统计与流完成/中止遥测。
- `src/budget.rs`：进程级全局出口字节预算与令牌桶限速（C1 批次 2）。
- `tests/concurrency.rs`：并发上限跨响应流持有、满载 503、`/healthz` 豁免、逐 frame idle timeout 与 35 秒长传输回归（默认 `#[ignore]`）。
- `tests/streaming_memory.rs`：独立 test binary，安装自定义 `#[global_allocator]` 验证 256 MiB 传输峰值堆增量 < 32 MiB。

## 环境变量配置 (`src/config.rs`)

所有数值与时长（接受纯秒数或 `s` 后缀）均有明确默认值与校验上下界，非法配置 fail-closed 直接终止启动：

| 分类 | 环境变量 | 默认值 | 校验边界 / 语义 |
|---|---|---|---|
| 基础 | `LISTEN_ADDR` | `0.0.0.0:8080` | 合法 `SocketAddr` |
| 基础 | `MAX_CONCURRENT_REQUESTS` | `256` | `1..=1_000_000` |
| 基础 | `MAX_URI_BYTES` | `16384` | `256..=1_048_576` |
| 安全带 | `ALLOW_TARGETS` | 空（不限） | 逗号分隔；精确 `api.x.com` 或后缀 `.x.com`（同时匹配子域与裸域） |
| 安全带 | `ALLOW_ORIGINS` | 空（`*`） | 逗号分隔；命中回显 origin + `Vary: Origin`，否则 403 |
| 安全带 | `AUTH_TOKEN` | 空（不要求） | 走 `X-Proxy-Token` header，转发前删除 |
| 安全带 | `ALLOW_PORTS` | 空（1-65535） | 逗号分隔 `1..=65535`，`0` 或非法值 fail-closed |
| 安全带 | `PUBLIC_MODE` | `false` | `1`/`true`/`yes`/`on` 视为真 |
| 兜底 | `MAX_EGRESS_BYTES` | 空（不限） | 正整数，进程累计出口软上限，重启重置 |
| 兜底 | `RATE_LIMIT_RPS` | 空（不限） | 正整数，全局令牌桶上限 |
| 超时 | `DNS_TIMEOUT` / `CONNECT_TIMEOUT` / `TLS_TIMEOUT` | `5s` / `10s` / `10s` | 各 `1..=300s` |
| 超时 | `UPSTREAM_HEADERS_TIMEOUT` / `UPLOAD_IDLE_TIMEOUT` | `30s` / `30s` | 各 `1..=600s` |
| 超时 | `UPSTREAM_BODY_IDLE_TIMEOUT` | `60s` | `1..=3600s` |
| 周期 | `SHUTDOWN_GRACE` / `HOST_REFRESH_INTERVAL` | `30s` / `60s` | `1..=300s` / `1..=3600s` |
| 其他 | `DENY_CIDRS` / `RUST_LOG` | 空 / `info` | 分别由 `resolver.rs`（非法启动失败）和 `telemetry.rs` 读取 |

- **已移除历史配置**（勿重新引入假承诺）：`MAX_HTTP1_BUFFER_BYTES`、`MAX_HEADERS_COUNT`（使用 hyper 默认值）、`POOL_IDLE_TIMEOUT`（曾误作连接总时长上限截断长流，已于 2026-07-24 删除）。
- **测试约定**：修改进程环境变量的单测必须通过 `config::tests::ENV_LOCK` 串行化，防止并行测试串扰。

## 启动 gate、健康检查与优雅关闭 (`src/main.rs`)

- **`health-check` 子命令**：进程第一个参数为 `health-check` 时，用阻塞式 `std::net::TcpStream` 连接 `127.0.0.1:<LISTEN_ADDR 端口>` 发 `GET /healthz`，响应行含 ` 200` 退出 0，否则退出 1。使运行镜像无需安装 `wget`/`curl`。
- **启动 gate (`should_gate_startup`)**：非 loopback 监听 **且** 未开启 `PUBLIC_MODE` **且** `ALLOW_TARGETS` 与 `AUTH_TOKEN` 均未配置时，输出「拒绝启动」指引并 `exit(1)`。
- **优雅关闭与强退看门狗**：收到 SIGTERM/SIGINT 后由 `Notify` 唤醒看门狗，仅对「关闭等待活跃连接」阶段计时 `SHUTDOWN_GRACE`，超时强制 `exit(0)`。绝不对整个 `axum::serve` 套 timeout。

## 并发上限与流生命周期 (`src/concurrency.rs`)

- **为什么不用 Tower `ConcurrencyLimitLayer`**：它在 handler 返回 `Response` 时就释放 permit，而流式响应 Body 的传输才刚开始，会导致进程级并发上限形同虚设。
- **当前实现**：`limit_concurrency` 用 `Semaphore::try_acquire_owned()` 立即抢占 permit，饱和时直接返回 503 `service_overloaded`（不排队）；成功后把 `OwnedSemaphorePermit` 装入 `GuardedBody<B>`，直到响应流读完或被 Drop 才归还。
- **`/healthz` 豁免**：不占用 permit、不受满载拒绝，避免满载时被容器健康检查误杀在途请求。

## 逐 frame 空闲超时与零缓冲流式 (`src/body_timeout.rs`)

- **无总时长上限**：只要数据持续流动，任意长度传输都不被主动中断；卡死由相邻 frame 间隔超时兜底（上传 `UPLOAD_IDLE_TIMEOUT`，下载 `UPSTREAM_BODY_IDLE_TIMEOUT`）。
- `IdleTimeoutBody<B>` 每次收到 frame 重置 `tokio::time::Sleep`，并顺带执行三项关键职责：
  1. **丢弃所有 trailer frame**（不透传，防绕过 header 清理）。
  2. **逐 frame 累加字节数**，并向 `Budget::add_egress` 记账（双向计入请求体与响应体）。
  3. 流结束打 `stream complete` debug 日志；超时或 frame 出错调 `telemetry::log_stream_aborted`（携带真实 `request_id` 与字节数）。

## 全局预算与限速 (`src/budget.rs`)

- 进程级 `Arc<Budget>`，重启即重置（不做时间窗口持久化）。
- `check_admission()` 顺序：先令牌桶（`RATE_LIMIT_RPS`，容量与每秒补充速率均为 `rps`，锁中毒时放行不自毁 → 429 `rate_limited`），再出口预算（`egress_used >= max_egress_bytes` → 503 `budget_exceeded`）。
- 出口预算为软上限（准入时检查、传输中逐 frame 累加），足够为带宽账单兜底。

## 回归测试约束

- `tests/concurrency.rs` 含 1 个默认 `#[ignore]` 的 35 秒持续长流回归，按需执行 `cargo test --test concurrency -- --ignored`。
- `tests/streaming_memory.rs` 必须保持为独立 test binary，避免 `#[global_allocator]` 被其他并行测试的堆分配污染。
