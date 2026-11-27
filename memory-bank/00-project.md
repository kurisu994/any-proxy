# any-proxy 项目总览

## 项目定位

`any-proxy`（`https://github.com/kurisu994/any-proxy`）是开源、自托管的 URL 前缀式 HTTP(S) CORS Relay，使用 Rust 2021 edition 单 crate 实现（含 `lib` 与 `bin` target），当前版本 `0.1.0`，MSRV Rust `1.86`，MIT 协议。

使用者将目标绝对 URL 直接拼在代理地址之后，请求从部署服务器出口发出，代理清理敏感头并补充统一 CORS 响应头，使浏览器可跨域读取结果：

```text
目标：https://api.example.com/data?q=1
代理：https://proxy.example.com/https://api.example.com/data?q=1
```

**出口语义**：本服务只解决浏览器**跨域（CORS）**问题，不是翻墙工具。请求完全依赖部署服务器所在网络的可达性。

## 项目定位拍板（2026-07-25）

**自用 / 实验项目，不按商业产品运作。**

这是所有范围与工程投入的取舍依据：

- 不面向外部商业用户，无 SLA、兼容包袱与运营值班需求，判断标准为「自托管部署时是否好用、是否会坑到自己」。
- 供应链与可观测性不再追求领先于实际用量：C2 边缘形态（Cloudflare Worker）、SBOM / provenance、Prometheus exporter、E2/E6 库级测试逃生口门控、C4 abuse kill-switch 均按此定位**正式关闭**（重开条件为「长期面向他人公网开放」）。
- **安全边界不因此放松**：SSRF 防护、地址校验、启动 gate 与逐跳重定向校验均不可退让，因为公网暴露风险与用户数无关。
- 交付状态保持 `DONE_WITH_CONCERNS`：公网匿名无按调用方硬额度的风险没有技术消除，需靠 C1 可选安全带、全局流量兜底与部署侧网络策略管理。

## 能力范围

- **支持**：公网 HTTP/HTTPS 目标的 `GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE` 流式转发；`ws://`/`wss://` WebSocket 透明升级转发（适配 OpenAI Realtime / Gemini Live 等实时接口）；`OPTIONS` 预检（固定 204）；`/healthz` 存活与版本探活；`/` 用法与风险说明首页。
- **不做**：TCP/UDP/SOCKS、HTTP `CONNECT`/`TRACE`、非 WebSocket 协议升级（如 h2c）、gRPC、整机代理、内容改写、自动解压、管理 UI、账户系统、按调用方配额、官方公共实例。
- **默认姿态与安全带**：核心保持零配置匿名可用，默认允许公共端口 `1..=65535`；但新增**启动 gate**（非 loopback 监听且未配置任何防护、也未设 `PUBLIC_MODE=1` 时拒绝启动）。提供可选、默认关、可组合的 C1 安全带（`ALLOW_TARGETS`、`ALLOW_PORTS`、`ALLOW_ORIGINS`、`AUTH_TOKEN`）及不依赖调用方身份的全局兜底（`RATE_LIMIT_RPS`、`MAX_EGRESS_BYTES`）。
- **部署与连接形态**：服务内部只监听 HTTP，公网 HTTPS 由前置反向代理（如 Caddy）终止；可通过 `cargo run`、Docker 或 `deploy.sh` 运行，以 `curl` 或浏览器调用。HTTP/1.1 上游采用安全的 Keep-Alive 连接池（`src/pool.rs`，按 origin 隔离、取出复核 `AddressPolicy`、受 `POOL_MAX_LIFETIME` 约束、`POOL_MAX_IDLE_PER_HOST=0` 可一键关闭）；WebSocket 始终建立新鲜连接，不入池。

## 核心使用流程

1. 部署者启动服务并配置公网 HTTPS 反向代理。
2. 调用方把目标绝对 URL 放在代理路径中；服务从原始 `path_and_query` 只剥离恰好一个前导 `/`，不二次解码。
3. 依次通过 URI 长度校验、可选访问控制与限速/预算准入后，规范化目标并经 `Connector` 执行 DNS 全量校验与固定 IP 连接（HTTPS 再行 TLS 握手）。
4. Hyper HTTP/1.1 client 流式转发请求体；若上游返回可跟随 3xx，`GET`/`HEAD` 按策略逐跳重新解析校验，带 Body 方法原样透传。
5. 清理响应头、写入统一 CORS 响应头后，流式返回响应体。

## 领域术语

- `Target`：规范化后的目标 scheme、host、port、path 与 query。
- `AddressPolicy`：地址校验策略，IPv6 仅放行 `2000::/3`（真 fail-closed），IPv4 穷举特殊用途 deny 列表，外加 `DENY_CIDRS` 与定期刷新的宿主接口地址。
- `Resolver`：可注入的 DNS 解析边界，返回本次查询的全部 A/AAAA IP。
- `Dialer`：可注入的 TCP 连接边界，返回实际 `peer_addr` 与 `BoxStream`。
- `Connector`：单次调用内原子完成 resolve → validate → dial → (TLS) 并复核 peer 的安全边界。
- `BoxStream`：统一包装 TCP 或 TLS stream 的异步读写类型。
- `PassThrough`：重定向状态机决定不自动跟随，将上游 3xx 原样留给调用方处理。
- **零 dial**：任何策略拒绝必须在建立上游连接前返回，保证 `Dialer` 调用次数为 0。
- **连接固定**：实际连接地址必须属于同一次已验证的 DNS 结果集合，禁止由高层客户端重新解析 hostname。
- **fail-closed**：未知、非法或枚举失败时默认拒绝（或保留安全快照），不静默降级放行。

## 里程碑现状

| 里程碑 | 状态 | 摘要 |
|---|---|---|
| M0 安全连接器 | ✅ 完成 | URL 解析、地址策略、可注入 `Resolver`/`Dialer`、`Connector`、重定向状态机、本地 HTTP/TLS 测试 |
| M1 完整 Relay | ✅ 完成 | Axum Relay、流生命周期并发上限、逐 frame idle timeout、Header/CORS/Trailer 清理、C1 安全带两批、优雅关闭、流式内存上界验证 |
| M2 发布供应链 | ✅ 收敛 | 多架构预构建镜像双推 Docker Hub + GHCR、CI 容器冒烟、`v0.1.0` 已打 tag、Caddy + `deploy.sh` 一键部署；SBOM / provenance / Prometheus 按定位关闭 |

## 已知残余风险

- 容器无法可靠发现 NAT hairpin 对应的宿主公网地址，需通过 `DENY_CIDRS` 或出口防火墙补充。
- 宿主接口地址每 `HOST_REFRESH_INTERVAL`（默认 60s）刷新一次，接口变更存在最长一个周期的竞态窗口。
- 默认允许全部公共端口 `1..=65535`，实例可被用于公共端口扫描（可用 `ALLOW_PORTS` 收紧）。
- IPv4 采用穷举 denylist，IANA 新分配的特殊用途段在补表前会被放行。
- 浏览器前端携带的目标 API `Authorization` 对终端用户可见且会经自托管代理转发。

## 仓库核心文档

- `README.md`：用户定位、风险提示、部署方式（预构建镜像 / `deploy.sh` / Caddy / 源码）、配置表与错误码排查。
- `DESIGN.md`：当前已实现的安全模型、模块边界、资源边界、错误契约与已知偏差清单。
- `AGENTS.md`：开发命令、编码规范、安全不变量与提交指南。
- `TODOS.md`：项目定位拍板、少量剩余待办与明确关闭项。
- `CHANGELOG.md`：历史变更记录。
- `docs/adr/0001-any-proxy-relay-design.md`：立项设计推演（历史快照，不再维护）。
