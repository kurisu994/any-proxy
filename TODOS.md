# TODOS — any-proxy 剩余工作

来源：2026-07-15 `/autoplan` 三相评审 + 2026-07-24 `/autoplan` 全量审查（Claude 实测探针 + Codex 交叉验证，10 条结论 8 证实 2 部分成立 0 推翻）。

已完成不列：~~E1 同源重定向误判为环~~、~~D1 容器健康检查失效~~（`904beb9`）、~~M1 并发上限不覆盖响应流~~、~~N1 POOL_IDLE_TIMEOUT 静默截断长传输~~、~~N8 饱和无限挂起~~、~~M2 trailer 旁路~~、~~M3 debug 日志泄露 query~~、~~N2 Location 解析~~、~~N3 304 误跟随~~、~~N4 IPv6 authority~~（`0f82183`）。

2026-07-24 本批（安全边界 + 代理正确性）：~~N5 错误回显 IP~~、~~E7 host-refresh fail-open~~、~~E8 6to4/Teredo~~、~~N12 DENY_CIDRS fail-closed~~、~~N7 Vary 覆盖~~、~~E9 Proxy-\* 通配清理~~、~~N14 healthz CORS~~、~~N6 SHUTDOWN_GRACE 死配置~~、~~E5 未接线 knob（删除）~~、~~N13 遥测契约~~、~~N15 无 failover~~。E2/E6 决定暂不做（见下）。

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

## 第 2 档 — 安全边界（除 E2/E6 均完成，二者暂不做）

- [x] **M2（P1）trailer 绕过 header 清理** — `body_timeout.rs:82` 原样透传所有 frame，trailer frame **从不经过** `clean_request_headers` / `clean_response_headers`。`Set-Cookie`、`Cookie`、转发头、CORS headers 都能藏在 trailer 里穿透代理。这是清理策略旁路，不只是文档不符。
  - 文件：`src/body_timeout.rs`、`src/headers.rs` · CC ~20min
- [x] **M3（P1）debug 日志泄露完整 query** — `proxy.rs:211`、`proxy.rs:286` 记录 `full_url()`，含 query。`RUST_LOG=debug` 下 API key、签名 URL 全进日志，违反 DESIGN §9。
  - 文件：`src/proxy.rs` · CC ~5min
- [x] **N5（P2）错误响应回显已解析的 IP** — `resolver.rs:187` 把 IP 拼进 `TargetBlocked.message`，`error.rs` 原样序列化。违反 DESIGN §8 明文承诺，构成对外的 DNS 解析预言机（可探测代理视角下内网域名的解析结果）。同类：`connector.rs:218` 泄露 peer_addr。
  - 修复：对外 message 改为通用文案，被拒 IP/peer_addr 只进内部 `debug`/`warn` 日志。文件：`src/resolver.rs`、`src/connector.rs`
- [x] **E8（P2）deny 6to4/Teredo** — `2002::/16`、`2001::/32`、`2001:10::/28` 全落在 `2000::/3` 白名单内被放行。宿主配置对应转换路由时，嵌入私网 IPv4 的地址形成条件式 SSRF。双声道一致认定为真实缺口。
  - 修复：三段加入 `default_deny_cidrs`（含 ORCHID `2001:10::/28`），补断言。文件：`src/resolver.rs`
- [x] **E7（P2）host-refresh 保留旧快照** — `get_if_addrs()` 失败时用空集合覆盖旧快照（fail-open），之后宿主公网接口地址可能被放行。
  - 修复：枚举失败时保留旧快照并 warn，只有成功才替换。文件：`src/resolver.rs`
- [ ] **E2（P2）cfg 门控测试逃生口** — `#[cfg(feature="test-util")]` 门控 `allow_all_for_test` 与无 TLS 的 `Connector::new`。当前 library target 对下游没有结构性保证。
  - 文件：`src/resolver.rs`、`src/connector.rs`、`Cargo.toml` · CC ~10min
  - 🅿️ **2026-07-24 决定暂不做**：仅影响 library 复用（二进制走可信路径）；`allow_all_for_test` 被 `tests/` 集成测试引用，feature 门控会连带把 `cargo test` 变成必须带 `--features test-util` 并改 CI，与「避免不必要防御性设计」相悖。留待真正 library 化时处理。
- [ ] **E6（P2）Connector 读真实 peer_addr** — 从真实 `TcpStream` 读而非信任 `DialRecord` 自报，或密封生产 Dialer 类型。当前二进制用可信 `TcpDialer`，风险仅在 library 边界。
  - 文件：`src/connector.rs` · CC ~15min
  - 🅿️ **2026-07-24 决定暂不做**：同 E2，风险仅在 library 边界；`TcpDialer` 已从真实 `TcpStream::peer_addr()` 读取，二进制不受影响。
- [x] **N12（P3）DENY_CIDRS 解析失败只 warn** — 运维笔误导致整条防护静默失效。fail-closed 产品的安全配置非法应启动即失败。
  - 修复：`with_env_deny_cidrs` 改返回 `Result`，非法条目返回 `Err`，`main.rs` 启动即 `exit(1)`。文件：`src/resolver.rs`、`src/main.rs`

## 第 3 档 — 代理正确性（✅ 已完成）

- [x] **N2/E3（P1）Location 解析改写主机名** — `redirect.rs:165-174` 字符串拼接代替 RFC 3986 解析。实测：`next` → `https://example.comnext/`（主机名变了）、`//evil.com/x` → 拼成本机路径、`HTTPS://` 大写不识别、`?q` 丢失原 path。且文档承诺「非法 Location 原样透传 3xx」，代码却抛错。
  - 修复：`url::Url::join`（这个轮子就在依赖里）。文件：`src/redirect.rs`、`tests/relay.rs` · CC ~20min
- [x] **N3（P1）304 被当重定向跟随** — `proxy.rs:264`、`redirect.rs:93` 用 `(300..400)`。304/300/305/306 全部误处理。条件请求缓存语义失效。
  - 注：Codex 指出 304 通常不带 Location，实际触发面小于初判。
  - 修复：跟随集合限定 `301/302/303/307/308`。 · CC ~5min
- [x] **N4/E4（P1）IPv6 字面量目标损坏** — `target.rs:220` 的 `authority()` 不加方括号。实测 `[2606:4700::1]:8080` → `"2606:4700::1:8080"`，Host header 畸形、`full_url()` 非法。
  - 注：Codex 指出「环检测必然错误」是过头说法——畸形串仍是一致的 HashSet key，只是不再是合法 canonical URL。
  - 文件：`src/target.rs`、`src/headers.rs`、`tests/relay.rs` · CC ~15min
- [x] **N7（P2）上游 Vary 被删除替换** — `headers.rs:57` 删掉上游全部 `Vary`，再写入 CORS-only 的固定值。`Accept-Encoding`/`Accept-Language` 缓存维度丢失，下游缓存可能跨变体返回错误响应。违反 DESIGN §7「保留缓存控制 headers」。
  - 修复：`RESPONSE_STRIP` 移除 `vary`，`add_cors_headers` 改 `append` 追加 CORS 维度。文件：`src/headers.rs`
- [x] **N15/C9（P2）只 dial addrs[0]，无 failover** — 无 Happy Eyeballs、无地址轮换。坏 AAAA 排在可用 A 前面时整站不可用（curl 却能通）。双声道一致认为被低估。
  - 修复：`connect` 按顺序 failover 遍历已验证地址，任一成功即用，全失败返回最后错误；peer_addr 校验移入循环。文件：`src/connector.rs`
- [x] **N6（P2）SHUTDOWN_GRACE 是死配置** — `main.rs:96-108`：`axum::serve(...).await` 已完成，再给瞬时 future 包 timeout，超时永不触发，强退分支不可达。活跃连接可让优雅关闭无限等待，编排系统无法按时终止进程。
  - 修复：用 `Notify` 让「关闭已开始」可观察，看门狗只对优雅关闭阶段计时（`select!` + `biased`），超时 `exit(0)`。文件：`src/main.rs`
- [x] **E5（P2）未接线的 config knob** — `max_http1_buffer_bytes`/`max_headers_count` 全仓库仅在 `config.rs` 出现，从未传给 hyper server builder。DESIGN §6 文档化的内存防线不存在。接线或删除，二选一。
  - 处理（选删除）：移除两个字段/env 解析 + README 表项，DESIGN §7 改为声明走 hyper 默认值。文件：`src/config.rs`、`README.md`、`DESIGN.md`
- [x] **E9（P3）Proxy-Connection 与通配前缀清理** — `REQUEST_STRIP` 只枚举少量名字，`Proxy-Connection`、`Proxy-*`、`X-Forwarded-Client-Cert` 仍可穿透，与「移除 `Proxy-*`」的声称不一致。
  - 修复：`clean_request_headers` 增加 `proxy-` / `x-forwarded-` 前缀通配删除。文件：`src/headers.rs`
- [x] **N13/E10（P3）遥测契约未兑现** — DESIGN §9 要求日志含 scheme/hostname/port/字节计数；`proxy.rs::log_complete` 只有 4 个字段。真正实现契约的 `telemetry::log_request_complete` 与 `RequestLogFields` 是死代码。`log_stream_aborted` 硬编码 `("unknown", …, 0)`。
  - 修复：`log_complete` 补 scheme/host/port；`IdleTimeoutBody` 带 request_id + 方向 + 字节计数，流结束/中止记真实值；删除死代码 `RequestLogFields`/`log_request_complete`。文件：`src/proxy.rs`、`src/telemetry.rs`、`src/body_timeout.rs`
- [x] **N14（P3）/healthz 无 CORS headers** — 其他所有响应都带，唯独健康检查没有。
  - 修复：`build_healthz_response` 补 `add_cors_headers`。文件：`src/error.rs`

## 第 4 档 — 产品真实性（✅ 已完成）

- [x] **D2（P1）README 海外出口前提** — 首屏写明「部署在大陆 VPS 出口仍是国内，对境外 API 可达性无收益」。
  - 修复：工作原理下新增「出口位置决定它能做什么」小节，明确 CORS 有收益、可达性无收益，「是 CORS 中继不是翻墙工具」。文件：`README.md`
- [x] **D5（P2）错误码表** — README 增加 错误码→HTTP→常见原因→排查动作 对照表（含本次新增的 `503 service_overloaded`）；可选给错误响应加安全不泄密的 `reason` 字段区分 `dns_failed`/`connect_failed`。
  - 修复：HTTP 接口下新增错误码对照表（9 个 code）。`reason` 字段属可选增强，暂未做。文件：`README.md`
- [x] **D4（P2）冒烟示例** — README 增加可粘贴 `curl` 代理真实公共 API + 浏览器 `fetch()` 片段，超越 `/healthz`。
  - 修复：快速开始下新增「冒烟测试」小节（curl + fetch 代理 `api.github.com/zen`）。文件：`README.md`

## 第 5 档 — 安全默认（在 D3 之前）

- [x] **C1（P1）安全带 — 批次 1 完成** — 2026-07-24 拍板：**折中默认姿态（additive + 启动 gate）+ 四类不依赖调用方身份的访问控制**。
  - 决策（不推翻 UC-1「匿名默认」）：新增启动 gate（非 loopback 监听 + 零防护 → 拒绝启动）；纳入 `ALLOW_TARGETS`/`ALLOW_PORTS`/`ALLOW_ORIGINS`/`AUTH_TOKEN`（走 `X-Proxy-Token`，转发前删除）；修正 Premise 3（区分「调用方身份鉴权」与「目标/预算控制」）。
  - 实现：`config.rs`（字段+解析+`target_allowed`/`port_allowed`/`origin_allowed`）、`lib.rs`（`401 unauthorized`）、`main.rs`（`should_gate_startup`+单测）、`proxy.rs`（校验+Origin 回显）、`headers.rs`（strip `X-Proxy-Token`）。测试：config 单测 + relay 端到端 3 组（auth/target/origin）。文档：README/DESIGN 同步。
  - ⏳ **批次 2** = 下面的「字节与时长预算」，单独排。
- [x] **C1 批次 2（P2）字节与时长预算** — 全局出口字节预算 + 令牌桶限速，为带宽账单兜底（Codex：「流式传输解决内存，不解决带宽账单」）。
  - 实现：新增 `budget.rs`（`Budget` + `TokenBucket`）；config 加 `MAX_EGRESS_BYTES`/`RATE_LIMIT_RPS`；`ProxyState` 注入 `Budget`，proxy 准入检查（429/503）；`body_timeout` 逐 frame 把请求体+响应体出口字节累加进预算；新增 `429 rate_limited`/`503 budget_exceeded` 错误码。测试：budget 单测 3 + relay 端到端 2（限速/预算）。文档：README/DESIGN 同步。
  - 说明：出口预算为进程累计软上限（准入时检查，重启重置），未做每日窗口；`per-client` 限速在匿名模型下无稳定 client 身份，故做全局限速。
- [ ] **C4（P2）abuse kill-switch** — 进程级滥用熔断开关 + 运营手段。
  - ⚠️ Codex 指出原 ~1h 估算不可信：它同时覆盖 kill-switch、带宽预算和部署体验，且未定义预算维度、触发行为与恢复流程。需要先拆解。
  - 文件：`.github/workflows/`、`src/telemetry.rs`、`src/config.rs` · CC 待重估

## 第 6 档 — 部署体验（安全默认已就位，本档解锁）

- [x] **D3（P1→降序）Caddy TLS 示例** — `docker-compose.caddy.yml` + `Caddyfile`，一键起公网 HTTPS。
  - ⚠️ **顺序已调整**：原本排在 A 档最前。Codex 指出它会让服务更容易公网暴露，必须排在安全默认（C1）之后，否则等于加速推广一个不可运营的默认配置。
  - 实现（2026-07-25）：`Caddyfile`（自动 ACME、`flush_interval -1` 关闭响应缓冲、不设上游响应超时以免重新引入 N1 式截断）；`docker-compose.caddy.yml`（any-proxy 不映射宿主端口只走内部网络；`DOMAIN` 用 `${VAR:?}` 强制并给中文提示；两容器均加固，Caddy 保留 `NET_BIND_SERVICE`）。
  - **默认收紧**：Caddy 编排默认带安全带 `ALLOW_TARGETS=api.github.com` / `ALLOW_PORTS=80,443` / `RATE_LIMIT_RPS=10` / `MAX_EGRESS_BYTES=10GiB`，放宽是显式选择——这正是排在 C1 之后的意义。
  - 顺带确认的部署前提：反代会把 `//` 折叠成 `/`，`parse_target` 依赖 WHATWG「special scheme 跳过任意数量斜杠」仍能正确解析，已加回归测试 `target::tests::test_collapsed_slashes_from_reverse_proxy` 锁定，故不需要 Caddy 侧重写规则。
  - ⚠️ 未验证：本机 Docker daemon 未运行，`Caddyfile` 语法与镜像构建**未实跑**，只做了 `docker compose config` 客户端校验。首次部署请留意 Caddy 启动日志。
- [x] **（P1）Dockerfile 的 Rust 版本低于 MSRV** — 上一批把真实 MSRV 从 1.75 修正为 1.86 时漏改 Dockerfile，构建镜像仍用 `rust:1.83-bookworm`，低于 `url`/`icu` 传递依赖要求，`docker build` 必然在依赖预编译阶段失败（且 CI 不构建镜像，无人拦截）。
  - 修复：`rust:1.83-bookworm` → `rust:1.86-bookworm`，并加注释说明必须 >= `Cargo.toml` 的 `rust-version`。文件：`Dockerfile`
  - 遗留：CI 仍不构建镜像，这类漂移下次仍无自动拦截——由「多架构镜像」项一并解决。
- [x] **D6（P2→提前）版本端点与预构建镜像** — 版本端点 `/healthz` 回显 `{"status":"ok","version":"…"}`；预构建镜像 + SemVer tag + 固定版本 compose 已随「多架构镜像」一并完成（见第 7 档）。
  - ⚠️ Codex 建议提前：「目标用户不该先本地编译 Rust」，这是当前最大的分发摩擦之一。
  - 文件：`src/error.rs`、`docker-compose.caddy.yml`、`.github/workflows/release.yml`、`README.md`
- [x] **N11（P3）容器加固** — compose 补 `read_only`、`cap_drop`、`no-new-privileges`、内存/pids 上限。对无额度开放代理，资源上限是唯一兜底。另：Dockerfile 依赖缓存层是坏的（只造 dummy `src/main.rs`，但 crate 还有 lib target，预构建必失败且被 `|| true` 吞掉，每次都全量重编）。
  - 修复：compose 补齐 4 项加固；Dockerfile 占位同时造 lib.rs+main.rs、去掉吞错误的 `|| true`、加 `--locked`。文件：`Dockerfile`、`docker-compose.yml`

## 第 7 档 — 测试与供应链

- [ ] **N10（P2）测试基础设施与覆盖缺口** — 详见 `~/.gstack/projects/kurisu994-any-proxy/kurisu-main-autoplan-test-plan-20260724.md`。本批已补齐核心盲区，剩余留后续：
  - ✅ **录制式假上游先决条件**：`tests/fixture.rs::RecordingServer` 完整解析并记录请求（method/path/headers/body，含 chunked 解码），可编程响应（支持 3xx）
  - ✅ **POST/PUT/PATCH/DELETE 端到端**：`test_methods_with_body_forwarded` 断言 method/path/body 转发正确与 Host 重建
  - ✅ **端到端重定向**：`test_redirect_followed_end_to_end` 是首个真正产生 3xx 的端到端测试，兼验 N2 相对 Location 解析
  - ✅ trailer 丢弃测试（`body_timeout::test_trailer_frame_dropped`）、body 字节计数测试（本批 N13）
  - ⏳ 剩余：`src/proxy.rs` 仍缺针对性单测；`test_streaming_256mib` 仍只数字节不测内存；`test_redirect_chain_multiple_connects` 名不副实，待重构/重命名
- [x] **（P3）CI 盲区：clippy 未覆盖测试代码** — CI 跑 `cargo clippy -- -D warnings`，不带 `--all-targets`，因此 `tests/` 里的 lint 长期未被拦截。应改为 `cargo clippy --all-targets -- -D warnings`。
  - 文件：`.github/workflows/ci.yml` · CC ~2min
- [x] **（P3）CI 无 MSRV 作业** — `rust-version = "1.75"` 从未被验证，CI 只跑 stable。
  - 修复：实测 1.75/1.85 均因 `icu`/`idna` 传递依赖不可行，真实 MSRV = **1.86**；更新 `rust-version` 与 README，CI 新增 `msrv` 作业（`dtolnay/rust-toolchain@1.86` + `cargo check --locked`）锁定。文件：`Cargo.toml`、`README.md`、`.github/workflows/ci.yml`
- [x] **N9（P2）供应链** — `hyper-rustls`、`rustls-pemfile`、`futures-util` 零引用（`tower` 已随本批移除）；`hyper-util` 只需 `tokio` feature。Dockerfile 与 CI 均未用 `--locked`，锁文件可静默漂移。
  - 修复：删除三个零引用依赖，`hyper-util` 收窄为 `["tokio"]`；Dockerfile 两处 build 与 CI clippy/test/check 全部加 `--locked`。文件：`Cargo.toml`、`Cargo.lock`、`Dockerfile`、`.github/workflows/ci.yml`
- [x] **E11（P3）CI 依赖安全检查** — `cargo deny` / `cargo audit`。
  - 修复：CI 新增 `audit` 作业（`rustsec/audit-check@v2`）。文件：`.github/workflows/ci.yml`
- [x] **（P2）多架构镜像** — Linux amd64 + arm64 由 CI 构建，附 SHA-256 校验。
  - 实现（2026-07-25）：`.github/workflows/release.yml`，tag `v*` 触发。两个架构**各自在原生 runner 上构建**（amd64 用 `ubuntu-latest`，arm64 用公共仓库免费的 `ubuntu-24.04-arm`），按 digest 推送后合成 manifest list——避开 QEMU 模拟下 Rust 交叉编译数十分钟的代价。
  - SemVer tag：`{{version}}` / `{{major}}.{{minor}}` / `latest`（`0.x` 不生成 major tag）。SHA-256 以镜像 digest 形式输出到 Actions summary，README 说明如何按 digest 固定。
  - 发布前置校验：git tag 与 `Cargo.toml` version 必须一致（`/healthz` 回显的就是后者），否则作业失败——防止发出「自称版本与 tag 不符」的镜像。
  - 固定版本 compose：`docker-compose.caddy.yml` 改为默认拉 `ghcr.io/kurisu994/any-proxy:0.1.0`，可用 `ANY_PROXY_IMAGE` 覆盖为本地镜像或 digest。
  - ⏳ **待你操作**：镜像需先打第一个 tag（`v0.1.0`）才存在；发布后还要在 GitHub package 设置里把可见性改为 Public，否则匿名 `docker pull` 失败。在此之前 caddy 编排需用 `ANY_PROXY_IMAGE=any-proxy:dev` 配合本地构建。
- [x] **（P2）CI 镜像构建与容器冒烟** — 此前 CI 完全不碰 Dockerfile，导致 MSRV 1.75→1.86 修正时漏改构建镜像版本、`docker build` 静默坏掉无人拦截（本轮才发现）。
  - 实现：`ci.yml` 新增 `docker` 作业——构建镜像 → 断言启动 gate 拒绝「无防护 + 非 loopback 监听」 → 带 `ALLOW_TARGETS` 启动后校验 `/healthz` 状态与版本号 → 校验 `health-check` 子命令（compose healthcheck 依赖它）。
  - 顺带覆盖了原第 7 档 P3「容器启动测试」的主体。
- [ ] **（P3）SBOM / provenance** — 后置。容器启动测试已由上面的 CI `docker` 作业覆盖。
  - 注：`release.yml` 目前显式 `provenance: false`（开启会在 push-by-digest 模式下污染 manifest）。要做时需连带处理 attestation 与 manifest list 的合成方式。
- [ ] **（P3）Prometheus exporter** — 后置。Codex：「在尚无用户验证时把 M2 定义成 SBOM/provenance/Prometheus，是供应链成熟度领先于产品成熟度」。

## 第 8 档 — 分发形态（最后）

- [ ] **C2（P2）边缘形态** — Cloudflare Worker 模板 + README 诚实对比 自托管 vs 边缘。
  - ⚠️ Codex：应先验证平台条款、目标连接能力和滥用责任，否则只是新增第二套产品。
  - 文件：`README.md`、`docs/` · CC ~2h

---

## 未决项（需你拍板）

- [x] **C1 默认姿态** — 2026-07-24 定：**折中方案**（保留匿名默认、additive，新增启动 gate），不推翻 UC-1。见第 5 档 C1。
- [x] **访问控制的「错误二分」** — 2026-07-24 定：部分采纳 Codex。**目标 allowlist / 全局预算不依赖调用方身份**，不受「前端存不住密钥」制约，已纳入 C1；per-user 身份鉴权仍放弃。DESIGN §4 已记修正。
- [ ] **产品定位** — 仍开放。Codex：「先决定是自用工具还是产品」，预估 6 个月活跃部署 `1-5`、尝试 `10-50`。决策倾向偏「自用/实验」（故 C1 走折中而非默认收紧）。若正式定位为产品，第一里程碑应是「10 个目标用户至少 5 个能 10 分钟内安全部署、两周后仍在用」，而非 M2 供应链。
