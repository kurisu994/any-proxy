# any-proxy 当前设计

> **这份文档描述系统「现在实际是什么样」。**
>
> 它只写已经实现并有代码或测试支撑的行为。尚未实现的东西写在 [TODOS.md](TODOS.md)，
> 不在这里以承诺形式出现。
>
> 立项时的设计推演、备选方案与原始 premises 已归档为
> [docs/adr/0001-any-proxy-relay-design.md](docs/adr/0001-any-proxy-relay-design.md)（历史快照，不再维护）。
>
> 最后校准：2026-07-24（对照代码逐条核验）

---

## 1. 它是什么

URL 前缀式的 HTTP(S) CORS Relay。把目标 URL 拼在代理地址后面：

```text
https://proxy.example.com/https://api.example.com/data?city=shanghai
```

请求从部署服务器的网络出口发出，浏览器可以读取返回的 CORS 响应。

**不做**：TCP/UDP/SOCKS、HTTP CONNECT、整机代理、WebSocket、协议升级、内容改写、自动解压。

**部署形态**：单个 Rust 二进制 + 单容器。服务内部只监听 HTTP，公网 HTTPS 由部署者的反向代理终止。

## 2. 当前状态

| 里程碑 | 状态 |
|--------|------|
| M0 安全连接器 | ✅ 完成 |
| M1 完整 Relay | ✅ 完成（存在已知偏差，见 §10） |
| M2 发布供应链 | 待做 |

**交付状态：`DONE_WITH_CONCERNS`。** 默认公网匿名、无调用方额度的风险没有技术消除。

## 3. 模块边界

```
                       ┌──────────────┐
   inbound HTTP  ──────▶│   app.rs     │  Router
                       └──────┬───────┘
                              ▼
                    ┌────────────────────┐
                    │  concurrency.rs    │  permit 覆盖整个响应流
                    │  饱和 → 503        │  /healthz 豁免
                    └─────────┬──────────┘
                              ▼
                       ┌──────────────┐
                       │  proxy.rs    │  编排：解析→清洗→连接→重定向→回写
                       └──┬───┬───┬───┘
             ┌────────────┘   │   └────────────┐
             ▼                ▼                ▼
      ┌────────────┐   ┌────────────┐   ┌──────────────┐
      │ target.rs  │   │headers.rs  │   │ redirect.rs  │
      │ URL 规范化  │   │ 头清洗+CORS │   │ 状态机+环检测 │
      └────────────┘   └────────────┘   └──────────────┘
                              │
                              ▼
      ┌──────────────────────────────────────┐
      │           connector.rs               │  ← 安全边界核心
      │  resolve → validate → dial → TLS     │
      └──────────────┬───────────────────────┘
                     ▼
              ┌────────────┐        ┌────────────────┐
              │resolver.rs │        │ body_timeout.rs│
              │AddressPolicy│       │  逐 frame idle │
              └────────────┘        └────────────────┘
```

| 模块 | 职责 |
|------|------|
| `main.rs` | 进程入口、配置加载、宿主接口刷新调度、`health-check` 子命令、SIGTERM 关闭 |
| `app.rs` | Axum Router 组装 |
| `concurrency.rs` | 进程级并发上限，permit 生命周期绑定到响应流结束 |
| `proxy.rs` | 核心编排 |
| `config.rs` | 环境变量解析与校验 |
| `headers.rs` | 请求/响应 header 清理、CORS 预检与响应头 |
| `error.rs` | 稳定 JSON 错误响应、健康检查、首页 |
| `telemetry.rs` | tracing 初始化、request_id 生成 |
| `target.rs` | 目标 URL、协议、主机、端口解析与规范化 |
| `resolver.rs` | DNS 解析、地址策略 |
| `connector.rs` | 一次调用内完成 resolve、全量校验、dial、TLS 握手 |
| `redirect.rs` | 重定向状态机、降级拦截、逐跳复查 |
| `body_timeout.rs` | 逐 frame 空闲超时 Body wrapper |

## 4. 安全边界（核心不变量）

这是本项目唯一不可退让的部分。以下每条都有对应测试。

- 只允许规范化后的 HTTP/HTTPS 目标；拒绝 userinfo、非法端口。
- **DNS 解析、全部 A/AAAA 地址校验、地址选择与实际 TCP dial 绑定在同一个 `Connector` 调用内。** 禁止先校验 hostname、再由其他客户端重新解析（DNS rebinding 防护）。
- **DNS 答案只要混入一个非公网地址就整体拒绝**，不挑选其中的公网地址继续。
- 实际 `peer_addr` 必须属于本次已验证的地址集合。
- 任何策略失败都在建立上游连接前返回错误（**零次 dial**）。
- 每次重定向重新解析并校验；拒绝重定向环、超限跳转和 HTTPS 降级到 HTTP。
- TLS 证书校验和 SNI 使用原始规范化 hostname，TCP 使用已固定的 IP；生产代码不接受任意证书。系统根证书加载为空时进程直接退出，不「假活着」。
- 不读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`。
- 上游连接**不复用**：每次请求都走一次完整的 resolve → 全量校验 → 固定 IP → dial。

### 地址策略的真实形态

文档此前笼统称为「fail-closed」，实际是两种不同机制：

- **IPv6：正向白名单（真 fail-closed）** — 只允许 `2000::/3` 全局单播，其余一律拒绝；`2000::/3` 内嵌入 IPv4 的 6to4（`2002::/16`）、Teredo（`2001::/32`）、ORCHID（`2001:10::/28`）已显式加入 deny 列表。
- **IPv4：穷举 denylist（fail-open）** — 不在 deny 列表中的地址视为公网。
  - 这是取舍而非疏忽：Rust 的 `Ipv4Addr::is_global()` 仍未稳定。
  - 代价：新分配的 IANA 特殊用途段在补进列表前会被放行。

deny 列表覆盖：私网、链路本地（含云元数据 `169.254.169.254`）、环回、多播、广播、未指定、CGN、Class E 保留、文档/基准测试段、IETF 协议分配、6to4 中继任播、IPv4-mapped/compatible IPv6、NAT64、discard 前缀。

宿主网络接口地址每 `HOST_REFRESH_INTERVAL` 刷新一次并加入 deny set。

### 残余风险

- 容器无法自动发现 NAT hairpin 对应的宿主公网地址，需通过 `DENY_CIDRS` 补充。
- 宿主接口地址刷新之间存在最长一个刷新周期的竞态窗口。
- 默认允许任意公共端口 1-65535，实例可被用于端口扫描（可用 `ALLOW_PORTS` 收紧）。
- 公网匿名无额度控制的风险没有技术消除（访问面可用下方 C1 安全带收紧，全局流量预算属未做的批次 2）。

### 可选访问控制（C1）

逐跳内网地址防护是**不可退让**的核心不变量；在它之上，C1 提供一层**可选、默认关**的访问控制，只在显式配置时生效，不改变零配置的匿名默认：

- `ALLOW_TARGETS`（目标 host allowlist，支持后缀匹配）、`ALLOW_PORTS`（端口 allowlist）
- `ALLOW_ORIGINS`（Origin allowlist，命中回显具体 origin 并加 `Vary: Origin`，否则 403）
- `AUTH_TOKEN`（共享令牌，走 `X-Proxy-Token`，转发前删除）
- `MAX_EGRESS_BYTES` / `RATE_LIMIT_RPS`（全局出口字节预算与令牌桶限速，为带宽账单与请求洪水兜底；批次 2）
- **启动 gate**：非 loopback 监听且未配置任何上述防护、也未设 `PUBLIC_MODE=1` 时拒绝启动，堵住无意识公网暴露。

> **对 ADR Premise 3 的修正**：原 EUREKA「放弃调用方访问控制」只对**依赖调用方身份**的手段（per-user token）成立——纯前端存不住长期密钥。但 `ALLOW_TARGETS` 与全局流量预算**不看调用方是谁**，不受该前提制约，因此纳入 C1。此修正不推翻「匿名默认、additive」的 UC-1 决定，只新增 gate 与不依赖身份的收紧手段。

## 5. HTTP 接口

| 路径 | 方法 | 行为 |
|------|------|------|
| `/<absolute-http-or-https-url>` | GET/HEAD/POST/PUT/PATCH/DELETE | 代理转发 |
| `/<absolute-http-or-https-url>` | OPTIONS | CORS 预检，固定 204 |
| `/healthz` | GET | 存活检查，不占用并发配额 |
| `/` | GET | 用法说明与风险提示 |
| (任意) | 其他方法 | 405 |

只剥离恰好一个前导 `/`，不折叠重复斜线，不提前 percent-decode。

## 6. CORS 与 header 策略

- 预检返回 `204`、`Access-Control-Allow-Origin: *`（不启用 credentials）。
- `Access-Control-Allow-Methods` 固定；`Access-Control-Allow-Headers` 回显通过校验的请求 header 名。
- 不转发浏览器 `Cookie`，不返回目标 `Set-Cookie`。
- 允许转发 `Authorization`，跨 origin 重定向时删除。
- 所有代理生成的错误响应都带 CORS headers（`/healthz` 除外，见 TODOS N14）。

请求与响应都先解析 `Connection` header 的逗号分隔 token 并删除其中点名的 headers，再删除固定 hop-by-hop 集合。请求侧额外移除 `Host`、`Forwarded`、`Via`、`X-Forwarded-*`、`Cookie`；响应侧额外移除 `Set-Cookie`、上游 CORS headers、`Server`、`X-Powered-By`。

> ⚠️ 两处已知偏差：`Proxy-Connection` 等前缀未被通配清理（TODOS E9）；
> **trailer frame 完全绕过上述清理**（TODOS M2）。

## 7. 流式传输与资源边界

- 请求与响应 Body 都逐帧转发，不完整缓冲。不做内容自动解压。
- 客户端断开后取消上游工作。
- **没有总请求时长上限。** 只要数据在流动，多长的传输都不会被代理主动中断。
- 卡死由**逐 frame 空闲超时**兜底：上传 `UPLOAD_IDLE_TIMEOUT`，下载 `UPSTREAM_BODY_IDLE_TIMEOUT`。
- **并发配额覆盖整个响应流的生命周期**（permit 挂在响应 Body 上，流结束才释放），因此 `MAX_CONCURRENT_REQUESTS` 是进程 socket 与上游连接任务的真实上界。
- 达到上限时**立即返回 `503 service_overloaded`**，不排队：调用方需要知道「过载」而不是看到「卡死」。
- HTTP/1 解析层的 buffer 与 header 数量上限使用 hyper 默认值，不额外暴露配置。
- 失败连接按解析出的地址顺序 failover，直到成功或耗尽全部候选地址。
- 可选的 `MAX_EGRESS_BYTES`（全局累计出口字节软上限，准入时检查、body 逐 frame 累加）与 `RATE_LIMIT_RPS`（令牌桶限速）为带宽账单与请求洪水兜底；默认关，详见 §4 C1。
- 首版禁用所有自动 retry。

> 设计说明：早期版本给整个连接 future 包了一层总时长 timeout（`POOL_IDLE_TIMEOUT`），
> 那会把持续有数据流动的长传输静默截断；而当时的并发 permit 在 handler 返回时就释放，
> 根本不覆盖流的生命周期，进程级资源上限形同虚设。两者互相掩盖，
> 已于 2026-07-24 一并修正，回归测试见 `tests/concurrency.rs`。

## 8. 错误契约

响应 headers 发出前的代理错误使用稳定 JSON 结构：

```json
{"error": {"code": "target_blocked", "message": "...", "request_id": "..."}}
```

| HTTP | code |
|------|------|
| 400 | `invalid_target` |
| 401 | `unauthorized` |
| 403 | `target_blocked` |
| 405 | `method_not_allowed` |
| 429 | `rate_limited` |
| 502 | `dns_failed` / `connect_failed` / `upstream_failed` |
| 503 | `service_overloaded` / `budget_exceeded` |
| 504 | `connect_timeout` / `upstream_timeout` |

上游返回的合法 4xx/5xx 按 Relay 语义原样转发，不转成代理 JSON 错误。

响应 headers 一旦发出，后续 Body 错误或 idle timeout 无法改写状态码：中止流，调用方看到截断 Body。

错误消息不回显解析出的目标 IP / peer_addr，仅记入内部日志，避免成为 DNS 解析预言机。

## 9. 可观测性与隐私

请求完成日志记录 `request_id`、method、scheme、hostname、port、status、duration；
流式传输结束或中止时按同一 `request_id` 记录方向与字节计数。

**不得记录** URL query、userinfo、headers、Cookie、Authorization、Body。

## 10. 已知偏差汇总

本文档不隐藏实现与意图的差距。以下是尚未修复的项，全部在 [TODOS.md](TODOS.md) 有条目：

| 领域 | 偏差 | 条目 |
|------|------|------|
| 库边界 | 测试逃生口是公开 API（仅影响 library 复用，二进制走可信路径） | E2 / E6 |

> 2026-07-24 一批安全边界与代理正确性偏差（M2 / M3 / N2 / N3 / N4 / N5 / N7 / E7 / E8 / E9 / N6 / N12 / N13 / N14 / N15 及 E5 的假承诺）已修复，不再列此表。

## 11. 容器与进程

- 多阶段构建，运行镜像只含二进制、CA 证书和非 root 用户。
- 只写 stdout/stderr，不依赖可写文件系统。
- 健康检查用二进制自带的 `health-check` 子命令，不依赖镜像内的 wget/curl。
- 支持 SIGTERM 优雅关闭；关闭开始后最多再等 `SHUTDOWN_GRACE`，超时则强制退出，避免活跃连接拖住进程。
- README 在运行命令之前显示公网匿名开放代理的风险说明。

## 12. 配置

见 [README.md](README.md#配置) 的完整表格。所有值有默认值和校验边界，非法值导致启动失败。

## 13. 测试

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --test concurrency -- --ignored   # 35 秒长传输回归，默认跳过
```

测试使用本地确定性 fixture 与注入式假 `Resolver`/`Dialer`，不依赖公网 DNS 或第三方服务。安全修复至少覆盖成功路径、拒绝路径和「未发生 dial」断言。

> ⚠️ 已知覆盖缺口见 TODOS 第 7 档：核心编排模块无单测、4 个 method 无端到端测试、
> 无端到端重定向测试、fixture 假上游不解析请求。

## 14. 里程碑标准的验证状态

原始 M0/M1/M2 成功标准见归档文档。M1 的以下标准当前**宣称完成但未被测试验证**：

- 「GET/HEAD/POST/PUT/PATCH/DELETE 六个 method 转发」— 只有 GET/HEAD 有端到端测试
- 「256 MiB 流式传输常驻内存不线性增长」— 测试只数字节数，从不测内存
- 「无 trailers 有集成测试」— 该测试不存在，且实现本身就转发 trailer（见 M2）
