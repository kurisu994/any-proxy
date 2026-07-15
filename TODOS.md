# TODOS — any-proxy 剩余工作

来源：2026-07-15 `/autoplan` 三相评审（Claude + Codex 双声道）+ 最终门接受的 3 个 User Challenge。
已完成不列：~~E1 同源重定向误判为环~~、~~D1 容器健康检查失效~~（commit `904beb9`）。
完整审计与失败模式登记见 `DESIGN.md`；对应里程碑追踪见 GitHub issue #4（M2）。

优先级：P1 阻塞发布 · P2 应同分支落地 · P3 后续跟进。效率标注为 CC 预估。

---

## A 档 — 安全默认与部署体验（先做）

- [ ] **C1（P1）安全带** — 增加可选、默认关的 `ALLOW_ORIGINS` / `ALLOW_TARGETS` / `AUTH_TOKEN`，默认端口限 `80/443`，显式 `unsafe-open` 才全开。保留匿名默认，additive。
  - 为什么：公网匿名+无额度+全端口作为默认不可运营（会上 Shodan、被封禁、承担带宽/法律风险）。双模型 critical 共识，用户已接受。
  - 文件：`src/config.rs`、`src/headers.rs`、`src/target.rs`、`src/proxy.rs`、`README.md` · CC ~2h
- [ ] **C4（P3→提前）abuse kill-switch** — 进程级滥用熔断开关 + 带宽/请求预算运营手段 + 5 分钟部署体验。
  - 为什么：Prometheus 只会精确展示实例如何被滥用却不止损；先给能止损的开关。
  - 文件：`.github/workflows/`、`src/telemetry.rs`、`src/config.rs` · CC ~1h
- [ ] **D3（P1）Caddy TLS 示例** — 随仓库交付 `docker-compose.caddy.yml` + `Caddyfile`，一键起公网 HTTPS。
  - 为什么：HTTPS 页面调 `http://vps:8080` 会被 mixed-content 拦截，TLS 是魔法时刻前置条件，不能推迟到 M2。DESIGN 承诺过 4 次但仓库未交付。
  - 文件：`docker-compose.caddy.yml`（新建）、`Caddyfile`（新建）、`README.md` · CC ~20min
- [ ] **D2（P1）README 海外出口前提** — 首屏写明"部署在大陆 VPS 出口仍是国内，对境外 API 可达性无收益"。
  - 为什么：产品价值先决条件缺失，是最贵的静默失败。
  - 文件：`README.md` · CC ~5min
- [ ] **D4（P2）冒烟示例** — README 增加可粘贴 `curl` 代理真实公共 API + 浏览器 `fetch()` 片段，超越 `/healthz`。
  - 文件：`README.md` · CC ~10min
- [ ] **D5（P2）错误码表** — README 增加 错误码→HTTP→常见原因→排查动作 对照表；可选给错误响应加安全不泄密的 `reason` 字段区分 `dns_failed`/`connect_failed`。
  - 文件：`README.md`、`src/error.rs` · CC ~15min

## B 档 — 剩余代码修正（评审发现，P2）

- [ ] **E2（P1）cfg 门控测试逃生口** — 用 `#[cfg(feature="test-util")]` 门控 `allow_all_for_test` 与接受任意 verifier 的 `with_tls`，防止逃生口进入生产 API。
  - 文件：`src/resolver.rs`、`src/connector.rs`、`Cargo.toml` · CC ~10min
- [ ] **E3（P1）RFC Location 解析** — 用 `url::Url::join` 处理 `//host`、无斜线相对路径、大小写 scheme；非法 Location 按承诺原样透传 3xx；限定跟随 `301/302/303/307/308`（排除 304）。
  - 文件：`src/redirect.rs`、`tests/relay.rs` · CC ~20min
- [ ] **E4（P2）IPv6 authority 方括号** — `Target::authority` 对 IPv6 literal 加 `[...]`，修复 Host header 与 Location 拼接。
  - 文件：`src/target.rs`、`src/headers.rs`、`tests/relay.rs` · CC ~10min
- [ ] **E5（P2）接线未用 config knob** — 把 `max_http1_buffer_bytes`/`max_headers_count` 接到 hyper server builder，否则删除这两个 config 与文档承诺（当前 DESIGN §6 文档化上限形同虚设）。
  - 文件：`src/config.rs`、`src/main.rs`、`src/app.rs` · CC ~15min
- [ ] **E6（P2）Connector 读真实 peer_addr** — 让 Connector 从真实 `TcpStream` 读 `peer_addr`（而非信任 `DialRecord` 自报），或密封生产 Dialer 类型。
  - 文件：`src/connector.rs` · CC ~15min
- [ ] **E7（P2）host-refresh 保留旧快照** — `refresh_host_addresses` 枚举失败时不用空集合覆盖（当前是 fail-open 降级，会清空宿主地址 deny set）。
  - 文件：`src/resolver.rs` · CC ~10min
- [ ] **E8（P2）deny 6to4/Teredo** — 补 IPv6 `2002::/16`、`2001::/32`、`2001:10::/28` deny，防止嵌入私网 IPv4 的地址在有对应路由的宿主上可达内网（fail-closed 模型的 SSRF 缺口）。
  - 文件：`src/resolver.rs` · CC ~20min
- [ ] **E9（P3）trailer 与 Proxy-Connection** — `body_timeout` 清理/丢弃上游 trailer frame，与模块声明一致；补 `Proxy-Connection` 到 `REQUEST_STRIP`。
  - 文件：`src/body_timeout.rs`、`src/headers.rs` · CC ~15min
- [ ] **E10（P3）stream_aborted 遥测** — 日志携带真实 `request_id` 与已传字节数；Body 超时后熔断避免重复日志。
  - 文件：`src/body_timeout.rs`、`src/telemetry.rs` · CC ~5min

## C 档 — 分发与供应链（后做）

- [ ] **C2（P2）边缘形态** — Cloudflare Worker/边缘部署模板 + README 诚实对比 自托管 vs 边缘。保留 Rust，只新增分发目标。
  - 文件：`README.md`、`docs/` · CC ~2h
- [ ] **D6（P2）版本端点** — `/healthz` 回显版本号；发布预构建镜像 + SemVer tag + 固定版本 compose，替代全量重编译升级。
  - 文件：`src/error.rs`、`docker-compose.yml`、`.github/workflows/` · CC ~10min
- [ ] **（P2）多架构镜像** — Linux amd64 + arm64 由 CI 构建，附 SHA-256 校验文件（保留原 M2 范围）。
  - 文件：`.github/workflows/` · CC ~30min
- [ ] **E11（P3）CI 依赖安全检查** — 增加 `cargo deny` / `cargo audit`（DESIGN §10 已承诺 M2 用 cargo deny，前移到常规 CI）。
  - 文件：`.github/workflows/ci.yml` · CC ~5min
- [ ] **（P3）SBOM / provenance / 容器启动测试** — 后置，不改 M1 代理语义。
- [ ] **（P3）Prometheus exporter** — 后置。

## 未决项（需你拍板）

- [ ] **C3 陈旧文档** — 是否修复 `DESIGN.md` premise #7 与 The Assignment 的"目录为空 / 只做 M0"措辞（与 M0+M1 已完成冲突）。同时是历史 office-hours 记录，最终门未单独决策，故留给你定：改写更新 vs 保留为历史快照。
