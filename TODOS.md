# TODOS — any-proxy 剩余工作

来源：2026-07-15 `/autoplan` 三相评审 + 2026-07-24 `/autoplan` 全量审查（Claude 实测探针 + Codex 交叉验证，10 条结论 8 证实 2 部分成立 0 推翻）。

已完成不列：~~E1 同源重定向误判为环~~、~~D1 容器健康检查失效~~（`904beb9`）、~~M1 并发上限不覆盖响应流~~、~~N1 POOL_IDLE_TIMEOUT 静默截断长传输~~、~~N8 饱和无限挂起~~（本批）。

排序原则（2026-07-24 定）：**资源边界 → 安全边界 → 产品真实性 → 部署体验**。
理由：代理转发得对不对是安全运营的前置条件；而降低公网暴露门槛（D3）必须排在安全默认之后，否则是让更多人更容易把不可运营的默认值推上公网。

优先级：P1 阻塞发布 · P2 应同分支落地 · P3 后续跟进。效率标注为 CC 预估。

---

## 第 1 档 — 资源边界（✅ 已完成）

- [x] **M1（P0）并发上限不覆盖响应流** — Tower `ConcurrencyLimitLayer` 在 handler 返回时就释放 permit，流式 Body 完全不受 `MAX_CONCURRENT_REQUESTS` 约束，可堆积远超上限的活跃流与上游连接任务。
  - 修复：自实现 `src/concurrency.rs`，permit 挂到响应 Body 上、流结束才释放；饱和立即返回 `503 service_overloaded`（顺带解决 N8）；`/healthz` 豁免（满载是「忙」不是「不健康」）。
- [x] **N1（P1）POOL_IDLE_TIMEOUT 静默截断长传输** — 它包住整个连接 future，是总时长硬上限而非空闲上限。实测：pool_idle=2s + 每 200ms 一个 chunk（全程无空闲）→ 连接 2.003s 被杀，收到 147/400 字节且无 chunked 终止符。
  - 修复：移除该 timeout；卡死由逐 frame idle timeout 兜底，连接任务总量由 M1 的 permit 兜底。移除 `POOL_IDLE_TIMEOUT` 配置项（它描述的连接池根本不存在）。
  - 回归测试：`tests/concurrency.rs`，含默认 ignore 的 35 秒真回归（已验证对旧代码在 30.003s 截断、944/1120 字节）。

## 第 2 档 — 安全边界（先做）

- [ ] **M2（P1）trailer 绕过 header 清理** — `body_timeout.rs:82` 原样透传所有 frame，trailer frame **从不经过** `clean_request_headers` / `clean_response_headers`。`Set-Cookie`、`Cookie`、转发头、CORS headers 都能藏在 trailer 里穿透代理。这是清理策略旁路，不只是文档不符。
  - 文件：`src/body_timeout.rs`、`src/headers.rs` · CC ~20min
- [ ] **M3（P1）debug 日志泄露完整 query** — `proxy.rs:211`、`proxy.rs:286` 记录 `full_url()`，含 query。`RUST_LOG=debug` 下 API key、签名 URL 全进日志，违反 DESIGN §9。
  - 文件：`src/proxy.rs` · CC ~5min
- [ ] **N5（P2）错误响应回显已解析的 IP** — `resolver.rs:187` 把 IP 拼进 `TargetBlocked.message`，`error.rs` 原样序列化。违反 DESIGN §8 明文承诺，构成对外的 DNS 解析预言机（可探测代理视角下内网域名的解析结果）。同类：`connector.rs:218` 泄露 peer_addr。
  - 文件：`src/resolver.rs`、`src/connector.rs` · CC ~10min
- [ ] **E8（P2）deny 6to4/Teredo** — `2002::/16`、`2001::/32`、`2001:10::/28` 全落在 `2000::/3` 白名单内被放行。宿主配置对应转换路由时，嵌入私网 IPv4 的地址形成条件式 SSRF。双声道一致认定为真实缺口。
  - 文件：`src/resolver.rs` · CC ~20min
- [ ] **E7（P2）host-refresh 保留旧快照** — `get_if_addrs()` 失败时用空集合覆盖旧快照（fail-open），之后宿主公网接口地址可能被放行。
  - 文件：`src/resolver.rs` · CC ~10min
- [ ] **E2（P2）cfg 门控测试逃生口** — `#[cfg(feature="test-util")]` 门控 `allow_all_for_test` 与无 TLS 的 `Connector::new`。当前 library target 对下游没有结构性保证。
  - 文件：`src/resolver.rs`、`src/connector.rs`、`Cargo.toml` · CC ~10min
- [ ] **E6（P2）Connector 读真实 peer_addr** — 从真实 `TcpStream` 读而非信任 `DialRecord` 自报，或密封生产 Dialer 类型。当前二进制用可信 `TcpDialer`，风险仅在 library 边界。
  - 文件：`src/connector.rs` · CC ~15min
- [ ] **N12（P3）DENY_CIDRS 解析失败只 warn** — 运维笔误导致整条防护静默失效。fail-closed 产品的安全配置非法应启动即失败。
  - 文件：`src/resolver.rs` · CC ~5min

## 第 3 档 — 代理正确性（P1，与安全边界同批）

- [ ] **N2/E3（P1）Location 解析改写主机名** — `redirect.rs:165-174` 字符串拼接代替 RFC 3986 解析。实测：`next` → `https://example.comnext/`（主机名变了）、`//evil.com/x` → 拼成本机路径、`HTTPS://` 大写不识别、`?q` 丢失原 path。且文档承诺「非法 Location 原样透传 3xx」，代码却抛错。
  - 修复：`url::Url::join`（这个轮子就在依赖里）。文件：`src/redirect.rs`、`tests/relay.rs` · CC ~20min
- [ ] **N3（P1）304 被当重定向跟随** — `proxy.rs:264`、`redirect.rs:93` 用 `(300..400)`。304/300/305/306 全部误处理。条件请求缓存语义失效。
  - 注：Codex 指出 304 通常不带 Location，实际触发面小于初判。
  - 修复：跟随集合限定 `301/302/303/307/308`。 · CC ~5min
- [ ] **N4/E4（P1）IPv6 字面量目标损坏** — `target.rs:220` 的 `authority()` 不加方括号。实测 `[2606:4700::1]:8080` → `"2606:4700::1:8080"`，Host header 畸形、`full_url()` 非法。
  - 注：Codex 指出「环检测必然错误」是过头说法——畸形串仍是一致的 HashSet key，只是不再是合法 canonical URL。
  - 文件：`src/target.rs`、`src/headers.rs`、`tests/relay.rs` · CC ~15min
- [ ] **N7（P2）上游 Vary 被删除替换** — `headers.rs:57` 删掉上游全部 `Vary`，再写入 CORS-only 的固定值。`Accept-Encoding`/`Accept-Language` 缓存维度丢失，下游缓存可能跨变体返回错误响应。违反 DESIGN §7「保留缓存控制 headers」。
  - 修复：追加而非覆盖。 · CC ~10min
- [ ] **N15/C9（P2）只 dial addrs[0]，无 failover** — 无 Happy Eyeballs、无地址轮换。坏 AAAA 排在可用 A 前面时整站不可用（curl 却能通）。双声道一致认为被低估。
  - 文件：`src/connector.rs` · CC ~30min
- [ ] **N6（P2）SHUTDOWN_GRACE 是死配置** — `main.rs:96-108`：`axum::serve(...).await` 已完成，再给瞬时 future 包 timeout，超时永不触发，强退分支不可达。活跃连接可让优雅关闭无限等待，编排系统无法按时终止进程。
  - 文件：`src/main.rs` · CC ~15min
- [ ] **E5（P2）未接线的 config knob** — `max_http1_buffer_bytes`/`max_headers_count` 全仓库仅在 `config.rs` 出现，从未传给 hyper server builder。DESIGN §6 文档化的内存防线不存在。接线或删除，二选一。
  - 文件：`src/config.rs`、`src/main.rs`、`src/app.rs` · CC ~15min
- [ ] **E9（P3）Proxy-Connection 与通配前缀清理** — `REQUEST_STRIP` 只枚举少量名字，`Proxy-Connection`、`Proxy-*`、`X-Forwarded-Client-Cert` 仍可穿透，与「移除 `Proxy-*`」的声称不一致。
  - 文件：`src/headers.rs` · CC ~10min
- [ ] **N13/E10（P3）遥测契约未兑现** — DESIGN §9 要求日志含 scheme/hostname/port/字节计数；`proxy.rs::log_complete` 只有 4 个字段。真正实现契约的 `telemetry::log_request_complete` 与 `RequestLogFields` 是死代码。`log_stream_aborted` 硬编码 `("unknown", …, 0)`。
  - 文件：`src/proxy.rs`、`src/telemetry.rs`、`src/body_timeout.rs` · CC ~20min
- [ ] **N14（P3）/healthz 无 CORS headers** — 其他所有响应都带，唯独健康检查没有。
  - 文件：`src/error.rs` · CC ~2min

## 第 4 档 — 产品真实性（立即做，成本极低）

- [ ] **D2（P1）README 海外出口前提** — 首屏写明「部署在大陆 VPS 出口仍是国内，对境外 API 可达性无收益」。
  - Codex 评价当前 README 是「法律式准确、产品式不诚实」：只说「从部署服务器网络出口发出」，没说清这对目标用户意味着什么。CORS 价值成立，可达性价值不成立。
  - 文件：`README.md` · CC ~5min
- [ ] **D5（P2）错误码表** — README 增加 错误码→HTTP→常见原因→排查动作 对照表（含本次新增的 `503 service_overloaded`）；可选给错误响应加安全不泄密的 `reason` 字段区分 `dns_failed`/`connect_failed`。
  - 文件：`README.md`、`src/error.rs` · CC ~15min
- [ ] **D4（P2）冒烟示例** — README 增加可粘贴 `curl` 代理真实公共 API + 浏览器 `fetch()` 片段，超越 `/healthz`。
  - 文件：`README.md` · CC ~10min

## 第 5 档 — 安全默认（在 D3 之前）

- [ ] **C1（P1）安全带** — 增加 `ALLOW_ORIGINS` / `ALLOW_TARGETS` / `AUTH_TOKEN`，默认端口限 `80/443`，显式 `unsafe-open` 才全开。
  - ⚠️ **需重新拍板**：现措辞是「可选、默认关」，Codex 指出这自相矛盾——标题叫「安全默认」却不改变任何默认值，加了一堆开关后默认姿态仍是不可运营。它建议默认应为**私有监听或必须配置目标 allowlist**，`unsafe-open` 才允许匿名任意目标。这与 2026-07-15 接受 UC-1 时「保留匿名默认、additive」的决定冲突，需要你重新定。
  - 文件：`src/config.rs`、`src/headers.rs`、`src/target.rs`、`src/proxy.rs`、`README.md` · CC ~2h
- [ ] **（P2）字节与时长预算** — 请求/响应字节上限、全局每日出口预算、per-client 限速。Codex：「流式传输解决内存，不解决带宽账单」。原 DESIGN 把这些排除在外，但无任何预算意味着带宽耗尽风险没有技术兜底。
  - 文件：`src/config.rs`、`src/proxy.rs` · CC ~2h
- [ ] **C4（P2）abuse kill-switch** — 进程级滥用熔断开关 + 运营手段。
  - ⚠️ Codex 指出原 ~1h 估算不可信：它同时覆盖 kill-switch、带宽预算和部署体验，且未定义预算维度、触发行为与恢复流程。需要先拆解。
  - 文件：`.github/workflows/`、`src/telemetry.rs`、`src/config.rs` · CC 待重估

## 第 6 档 — 部署体验（在安全默认之后）

- [ ] **D3（P1→降序）Caddy TLS 示例** — `docker-compose.caddy.yml` + `Caddyfile`，一键起公网 HTTPS。
  - ⚠️ **顺序已调整**：原本排在 A 档最前。Codex 指出它会让服务更容易公网暴露，必须排在安全默认（C1）之后，否则等于加速推广一个不可运营的默认配置。
  - 文件：`docker-compose.caddy.yml`（新建）、`Caddyfile`（新建）、`README.md` · CC ~20min
- [ ] **D6（P2→提前）版本端点与预构建镜像** — `/healthz` 回显版本号；发布预构建镜像 + SemVer tag + 固定版本 compose。
  - ⚠️ Codex 建议提前：「目标用户不该先本地编译 Rust」，这是当前最大的分发摩擦之一。
  - 文件：`src/error.rs`、`docker-compose.yml`、`.github/workflows/` · CC ~30min
- [ ] **N11（P3）容器加固** — compose 补 `read_only`、`cap_drop`、`no-new-privileges`、内存/pids 上限。对无额度开放代理，资源上限是唯一兜底。另：Dockerfile 依赖缓存层是坏的（只造 dummy `src/main.rs`，但 crate 还有 lib target，预构建必失败且被 `|| true` 吞掉，每次都全量重编）。
  - 文件：`Dockerfile`、`docker-compose.yml` · CC ~20min

## 第 7 档 — 测试与供应链

- [ ] **N10（P2）测试基础设施与覆盖缺口** — 详见 `~/.gstack/projects/kurisu994-any-proxy/kurisu-main-autoplan-test-plan-20260724.md`
  - **先决条件**：`tests/fixture.rs` 的假上游从不解析请求（读一次就回定长响应），因此没有任何测试能断言「代理实际发给上游的是什么」——N1/N2/N3 全在这个盲区。需要一个录制式假上游。CC ~40min
  - `src/proxy.rs`（343 行核心编排）0 单测；`src/body_timeout.rs`（117 行）0 单测
  - **POST/PUT/PATCH/DELETE 无任何端到端测试**（M1 标准 2 要求 6 个 method，只测了 GET/HEAD）
  - **全仓库没有端到端重定向测试**；`test_redirect_chain_multiple_connects` 名不副实，只是顺序连了两个 server，从未产生 3xx —— E1 能进主干的根因
  - `test_streaming_256mib` 只数字节数从不测内存，M1 标准 3 处于「宣称完成、从未验证」
  - 无 trailer 测试（M1 标准 5 要求）
- [ ] **（P3）CI 盲区：clippy 未覆盖测试代码** — CI 跑 `cargo clippy -- -D warnings`，不带 `--all-targets`，因此 `tests/` 里的 lint 长期未被拦截。应改为 `cargo clippy --all-targets -- -D warnings`。
  - 文件：`.github/workflows/ci.yml` · CC ~2min
- [ ] **（P3）CI 无 MSRV 作业** — `rust-version = "1.75"` 从未被验证，CI 只跑 stable。
  - 文件：`.github/workflows/ci.yml` · CC ~5min
- [ ] **N9（P2）供应链** — `hyper-rustls`、`rustls-pemfile`、`futures-util` 零引用（`tower` 已随本批移除）；`hyper-util` 只需 `tokio` feature。Dockerfile 与 CI 均未用 `--locked`，锁文件可静默漂移。
  - 文件：`Cargo.toml`、`Dockerfile`、`.github/workflows/ci.yml` · CC ~20min
- [ ] **E11（P3）CI 依赖安全检查** — `cargo deny` / `cargo audit`。
  - 文件：`.github/workflows/ci.yml` · CC ~5min
- [ ] **（P2）多架构镜像** — Linux amd64 + arm64 由 CI 构建，附 SHA-256 校验文件。
  - 文件：`.github/workflows/` · CC ~30min
- [ ] **（P3）SBOM / provenance / 容器启动测试** — 后置。
- [ ] **（P3）Prometheus exporter** — 后置。Codex：「在尚无用户验证时把 M2 定义成 SBOM/provenance/Prometheus，是供应链成熟度领先于产品成熟度」。

## 第 8 档 — 分发形态（最后）

- [ ] **C2（P2）边缘形态** — Cloudflare Worker 模板 + README 诚实对比 自托管 vs 边缘。
  - ⚠️ Codex：应先验证平台条款、目标连接能力和滥用责任，否则只是新增第二套产品。
  - 文件：`README.md`、`docs/` · CC ~2h

---

## 未决项（需你拍板）

- [ ] **C1 默认姿态** — 见第 5 档。「可选默认关」vs「默认私有/必须配 allowlist，`unsafe-open` 才全开」。后者推翻 2026-07-15 接受 UC-1 时的决定。
- [ ] **产品定位** — Codex 直言：「先决定是自用工具还是产品」。当前形态下它预估未来 6 个月活跃部署者 `1-5`、尝试构建 `10-50`。若定位为产品，第一里程碑不该是 M2 供应链，而应是「10 个目标用户中至少 5 个能在 10 分钟内安全部署，两周后仍在用」。
- [ ] **访问控制的「错误二分」** — DESIGN 的核心 EUREKA 是「前端存不住长期密钥 → 放弃调用方访问控制」。Codex 认为这是错误二分：目标 allowlist、短期签名、同源后端换票、全局预算、可轮换 token 都仍然有效。若成立，动摇 Premise 3 与整个 C1 设计。
