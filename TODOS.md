# TODOS — any-proxy 剩余工作

来源：2026-07-15 `/autoplan` 三相评审 + 2026-07-24 `/autoplan` 全量审查（Claude 实测探针 + Codex 交叉验证）。
本文件只列**未完成**事项；已完成项的实现细节见 git 历史与 `CHANGELOG.md`。

排序原则（2026-07-24 定）：**资源边界 → 安全边界 → 产品真实性 → 部署体验**。
优先级：P1 阻塞发布 · P2 应同分支落地 · P3 后续跟进。

已完成（不再展开）：第 1 档资源边界（M1/N1）、第 2 档安全边界（M2/M3/N5/E7/E8/N12）、
第 3 档代理正确性（N2/N3/N4/N7/N15/N6/E5/E9/N13/N14）、第 4 档产品真实性（D2/D4/D5）、
第 5 档安全默认（C1 批次 1+2）、第 6 档部署体验（D3/D6/N11 + Dockerfile 两个 P0/P1）、
第 7 档测试与供应链（N10/N9/E11/MSRV/clippy 全靶/多架构镜像/CI 容器冒烟）。

---

## 需要你操作

- [ ] **首次发布 v0.1.0** — 发布流水线已就位但从未跑过，镜像尚不存在。
  ```bash
  git tag v0.1.0 && git push origin v0.1.0
  ```
  - 发布后需在 GitHub package 设置里把可见性改为 **Public**，否则匿名 `docker pull` 失败。
  - 在此之前 `docker-compose.caddy.yml` 默认引用的 `ghcr.io/kurisu994/any-proxy:0.1.0` 拉不到，
    需用 `ANY_PROXY_IMAGE=any-proxy:dev` 配合本地 `docker build` 使用。
  - ⚠️ `release.yml` 与 CI 的 `docker` 作业均为首次运行，留意 Actions 结果。

- [ ] **产品定位** — 仍开放，且它决定下面 C4/C2 值不值得做。
  Codex：「先决定是自用工具还是产品」，预估 6 个月活跃部署 `1-5`、尝试 `10-50`。
  当前决策倾向偏「自用/实验」（故 C1 走折中而非默认收紧）。
  若正式定位为产品，第一里程碑应是「10 个目标用户至少 5 个能 10 分钟内安全部署、两周后仍在用」，而非供应链成熟度。

## 待跟进

- [ ] **C4（P2）abuse kill-switch** — 进程级滥用熔断开关 + 运营手段。
  - C1 批次 2 的出口预算与全局限速已覆盖其大半范围，**剩余部分需先拆解再估**：
    预算维度、触发行为、恢复流程都还没定义。
  - ⚠️ Codex 指出原 ~1h 估算不可信（它同时覆盖 kill-switch、带宽预算和部署体验）。
  - 文件：`src/telemetry.rs`、`src/config.rs` · CC 待重估

- [ ] **C2（P2）边缘形态** — Cloudflare Worker 模板 + README 诚实对比 自托管 vs 边缘。
  - ⚠️ Codex：应先验证平台条款、目标连接能力和滥用责任，否则只是新增第二套产品。
  - 文件：`README.md`、`docs/` · CC ~2h

- [ ] **（P3）SBOM / provenance** — 后置。
  - `release.yml` 目前显式 `provenance: false`（开启会在 push-by-digest 模式下污染 manifest）。
    要做时需连带处理 attestation 与 manifest list 的合成方式。

- [ ] **（P3）Prometheus exporter** — 后置。
  Codex：「在尚无用户验证时把 M2 定义成 SBOM/provenance/Prometheus，是供应链成熟度领先于产品成熟度」。

- [ ] **（P3）rustls-pemfile 未维护告警** — `RUSTSEC-2025-0134`，仅 informational，无漏洞。
  - 它是 `rustls-native-certs v0.7.3` 的传递依赖（N9 已删除直接依赖），需升级上游才能消除。
  - 当前 `cargo audit` 报 0 漏洞，不阻塞。

## 已决定暂不做

- [ ] **E2（P2）cfg 门控测试逃生口** — `#[cfg(feature="test-util")]` 门控 `allow_all_for_test`
  与无 TLS 的 `Connector::new`。
  - 🅿️ **2026-07-24 决定暂不做**：仅影响 library 复用（二进制走可信路径）；`allow_all_for_test`
    被 `tests/` 引用，feature 门控会连带把 `cargo test` 变成必须带 `--features test-util` 并改 CI，
    与「避免不必要防御性设计」相悖。留待真正 library 化时处理。

- [ ] **E6（P2）Connector 读真实 peer_addr** — 从真实 `TcpStream` 读而非信任 `DialRecord` 自报。
  - 🅿️ **2026-07-24 决定暂不做**：同 E2，风险仅在 library 边界；`TcpDialer` 已从真实
    `TcpStream::peer_addr()` 读取，二进制不受影响。
