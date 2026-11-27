# 开发日志 — Kurisu

- 开发者：Kurisu
- 核实日期：2026-11-27
- 身份来源：`git var GIT_AUTHOR_IDENT` 与 `git config --show-origin`（`~/.gitconfig`）

## 2026-11-27 — 记忆银行迁移至四层结构并刷新代码事实

- 将旧版 6 枢纽记忆银行（`projectbrief` / `productContext` / `systemPatterns` / `techContext` / `activeContext` / `progress`）迁移为新版四层结构：`00-project.md` + 4 份按代码路径正交切分的领域规范（`10-security-connector.md`、`11-relay-http.md`、`12-runtime-resources.md`、`13-build-deploy-ci.md`）+ `active/main.md` + `journal/kurisu.md` + `archive/2026-11/progress-legacy.md`。
- 对照当前 HEAD（`d7fd4bb`）全量校准事实：补齐 C1 安全带两批（`budget.rs`、启动 gate、`ALLOW_*`、`AUTH_TOKEN`）、流生命周期并发 permit（`concurrency.rs`）、逐 frame idle timeout 与 trailer 丢弃（`body_timeout.rs`）、MSRV 1.86、Dockerfile mtime 陷阱修复、Caddy + `deploy.sh` 三段探测及 `v0.1.0` 双源多架构发布流水线。
- 安装 Claude Code（`.claude/settings.json`）与 Antigravity（`.agents/hooks.json`）路径触发注入 hook，配置 `.gitattributes` 对 `memory-bank/journal/*.md` 启用 `merge=union`，并在 `AGENTS.md` 补充 `memory-bank/README.md` 入口指引。
- 同步修正 `AGENTS.md`、`DESIGN.md`、`TODOS.md` 中过时的 MSRV/模块/测试数/M2 状态与已修复偏差描述，跑通 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与 `cargo test`（174 passed, 1 ignored）。

## 2026-11-27 — 大模型调用适配：超时调优、安全 HTTP/1.1 连接池与透明 WebSocket 升级

- 调整默认超时：`UPSTREAM_HEADERS_TIMEOUT` 与 `UPSTREAM_BODY_IDLE_TIMEOUT` 默认提升至 `180s`（适配大模型慢首包与深度思考停顿），并在 `docker-compose.caddy.yml` / `.env.example` / `deploy.sh` 同步透传配置。
- 新增 `src/pool.rs`（零新依赖）：安全 HTTP/1.1 Keep-Alive 上游连接池，按 `(is_tls, canonical_host, port)` 隔离、LIFO 复用、取出复核 `AddressPolicy`、受 `POOL_IDLE_TIMEOUT` / `POOL_MAX_LIFETIME` 约束且支持 `POOL_MAX_IDLE_PER_HOST=0` 一键关闭；发送前以 `sender.ready().await` 探测半关闭连接，让非幂等 POST 也能零副作用重建。
- 新增 `src/ws.rs`（零新依赖）：基于 Hyper 原生 HTTP/1.1 Upgrade 实现 `ws://`/`wss://` 透明全双工转发；通过 `concurrency::PermitSlot` 将并发 permit 生命周期覆盖至 WebSocket 断连，带 `WS_IDLE_TIMEOUT` 双向空闲超时与出口预算记账，仅白名单放行 `Upgrade: websocket`。
- **踩坑结论**：Hyper 1 服务端写出带精确 `Content-Length` 的响应后，写满字节数就会直接 Drop 响应体而不再多调用一次 `poll_frame()` 拿 `Poll::Ready(None)`；在 `IdleTimeoutBody` 中改用 `inner: Option<B>` 并在末帧/`Drop` 时检查 `inner.is_end_stream()` 先释放 `Incoming` 再触发连接池归还，彻底解决定长/空响应无法归还连接池的问题。
- 新增 31 个单元与端到端回归测试（含 `tests/pool_and_ws.rs` 12 项），全套 `fmt + clippy + test` 通过（205 passed, 1 ignored）。

