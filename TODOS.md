# TODOS — any-proxy 剩余工作

来源：2026-07-15 `/autoplan` 三相评审 + 2026-07-24 `/autoplan` 全量审查（Claude 实测探针 + Codex 交叉验证）。
本文件只列**未完成**事项；已完成项的实现细节见 git 历史与 `CHANGELOG.md`。

## 项目定位（2026-07-25 拍板）

**自用 / 实验项目，不按商业产品运作。**

这条定位是取舍依据，直接决定了下面哪些事不做：

- 没有外部用户承诺，就没有兼容性包袱、SLA 和运营值班需求。
- 判断标准从「产品成熟度」换成「我自己部署时够不够用、会不会坑到自己」。
- 供应链与可观测性的成熟度**不再追求领先于实际用量**——Codex 早先的提醒
  （「在尚无用户验证时把 M2 定义成 SBOM/provenance/Prometheus，是供应链成熟度领先于产品成熟度」）
  在此定位下正式采纳。
- 安全边界**不因此放松**：地址校验、fail-closed 策略、启动 gate 照旧，
  因为公网暴露的风险与用户数无关。

已完成（不再展开）：第 1 档资源边界（M1/N1）、第 2 档安全边界（M2/M3/N5/E7/E8/N12）、
第 3 档代理正确性（N2/N3/N4/N7/N15/N6/E5/E9/N13/N14）、第 4 档产品真实性（D2/D4/D5）、
第 5 档安全默认（C1 批次 1+2）、第 6 档部署体验（D3/D6/N11 + Dockerfile 两个 P0/P1）、
第 7 档测试与供应链（N10/N9/E11/MSRV/clippy 全靶/多架构镜像/CI 容器冒烟）。

---

## 待办

- [ ] **首次发布 v0.1.0** — 发布流水线已就位但从未跑过，镜像尚不存在。
  ```bash
  git tag v0.1.0 && git push origin v0.1.0
  ```
  - 发布后需在 GitHub package 设置里把可见性改为 **Public**，否则匿名 `docker pull` 失败。
  - 在此之前 `docker-compose.caddy.yml` 默认引用的 `ghcr.io/kurisu994/any-proxy:0.1.0` 拉不到，
    需用 `ANY_PROXY_IMAGE=any-proxy:dev` 配合本地 `docker build` 使用。
  - ⚠️ `release.yml` 为首次运行，留意 `ubuntu-24.04-arm` runner 能否正常调度。
  - 自用定位下这件事仍然值得做：省掉在小内存 VPS 上编译 Rust（容易直接 OOM）。

- [ ] **（P3）rustls-pemfile 未维护告警** — `RUSTSEC-2025-0134`，仅 informational，无漏洞。
  - 它是 `rustls-native-certs v0.7.3` 的传递依赖（N9 已删除直接依赖），需升级上游才能消除。
  - 当前 `cargo audit` 报 0 漏洞，不阻塞。等上游自己升级即可。

## 按定位关闭（自用/实验，不做）

以下都不是「以后再做」，而是**在当前定位下明确不做**。若哪天转为面向外部用户的产品，再重新开。

- **C2 边缘形态**（Cloudflare Worker 模板）— 自托管已够自用；这等于新增并长期维护第二套产品形态。
- **SBOM / provenance** — 供应链证明的受众是第三方消费者，自用无人消费。
- **Prometheus exporter** — 单实例自用，结构化日志（已含 request_id / scheme / host / port / 字节计数）足够排查。
- **E2 cfg 门控测试逃生口** — 风险只在把本项目当 library 复用时；自用不存在这个场景。
- **E6 Connector 读真实 peer_addr** — 同上，且 `TcpDialer` 本就从真实 `TcpStream::peer_addr()` 读，二进制不受影响。
- **C4 abuse kill-switch** — 其原范围已被 C1 批次 2 的全局限速（`RATE_LIMIT_RPS`）与
  出口预算（`MAX_EGRESS_BYTES`）覆盖大半，剩下的熔断/恢复流程属于运营需求。
  自用场景下真遇到滥用，直接改配置或停容器即可。
  🔁 **重开条件**：若把实例长期公网开放给他人使用。
