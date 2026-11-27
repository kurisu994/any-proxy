# 开发日志 — Kurisu

- 开发者：Kurisu
- 核实日期：2026-11-27
- 身份来源：`git var GIT_AUTHOR_IDENT` 与 `git config --show-origin`（`~/.gitconfig`）

## 2026-11-27 — 记忆银行迁移至四层结构并刷新代码事实

- 将旧版 6 枢纽记忆银行（`projectbrief` / `productContext` / `systemPatterns` / `techContext` / `activeContext` / `progress`）迁移为新版四层结构：`00-project.md` + 4 份按代码路径正交切分的领域规范（`10-security-connector.md`、`11-relay-http.md`、`12-runtime-resources.md`、`13-build-deploy-ci.md`）+ `active/main.md` + `journal/kurisu.md` + `archive/2026-11/progress-legacy.md`。
- 对照当前 HEAD（`d7fd4bb`）全量校准事实：补齐 C1 安全带两批（`budget.rs`、启动 gate、`ALLOW_*`、`AUTH_TOKEN`）、流生命周期并发 permit（`concurrency.rs`）、逐 frame idle timeout 与 trailer 丢弃（`body_timeout.rs`）、MSRV 1.86、Dockerfile mtime 陷阱修复、Caddy + `deploy.sh` 三段探测及 `v0.1.0` 双源多架构发布流水线。
- 安装 Claude Code（`.claude/settings.json`）与 Antigravity（`.agents/hooks.json`）路径触发注入 hook，配置 `.gitattributes` 对 `memory-bank/journal/*.md` 启用 `merge=union`，并在 `AGENTS.md` 补充 `memory-bank/README.md` 入口指引。
- 同步修正 `AGENTS.md`、`DESIGN.md`、`TODOS.md` 中过时的 MSRV/模块/测试数/M2 状态与已修复偏差描述，跑通 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与 `cargo test`（174 passed, 1 ignored）。

