# any-proxy

> Rust 公网匿名 CORS Relay — URL 前缀式 HTTP(S) 代理

## ⚠️ 风险提示

**any-proxy 是公网匿名开放代理，无额度控制。**

- 部署后任何人都可通过你的服务器向任意公共 HTTP(S) URL 发起请求
- 不要求 Token，不校验调用方 Origin，不限制目标域名
- 没有按调用方的请求额度、流量预算或响应大小配额
- 长期大流量可能耗尽部署者带宽
- 请使用云厂商账单告警、出口防火墙和实例级网络策略控制风险
- **不要在无防护的公网实例上运行生产业务**

## 工作原理

用户只需把目标 URL 加到代理前缀：

```text
原始请求：
https://api.example.com/data?city=shanghai

代理请求：
https://proxy.your-server.com/https://api.example.com/data?city=shanghai
```

请求从部署服务器的网络出口发出，浏览器可读取返回的 CORS 响应。

### 出口位置决定它能做什么

代理请求从**部署服务器所在网络**发出，因此它解决的是**跨域（CORS）**问题，而不是**网络可达性**问题：

- ✅ **有收益**：浏览器因同源策略无法直接读取的公共 API，经代理转发后带上 CORS 头即可读取。
- ❌ **无收益**：若部署在大陆 VPS，出口仍是国内网络，对境外 API 的可达性不会改善——被墙的目标依旧不可达。要访问境外 API，代理必须部署在能直连目标的网络位置。

一句话：**它是 CORS 中继，不是翻墙工具。**

## 快速开始

### 从源码构建

```bash
git clone https://github.com/kurisu994/any-proxy.git
cd any-proxy
cargo build --release
./target/release/any-proxy
```

服务默认监听 `0.0.0.0:8080`，通过 `LISTEN_ADDR` 环境变量可修改。

### Docker

```bash
docker compose up -d
```

或手动构建：

```bash
docker build -t any-proxy .
docker run -d -p 8080:8080 any-proxy
```

健康检查：

```bash
curl http://localhost:8080/healthz
# ok
```

### 冒烟测试

代理一个真实公共 API（`-i` 可看到代理补上的 CORS 响应头）：

```bash
curl -i http://localhost:8080/https://api.github.com/zen
# 返回一句随机格言，响应头带 access-control-allow-origin: *
```

浏览器中跨域读取（把目标 URL 拼在代理前缀后即可）：

```js
const proxy = "http://localhost:8080/";
const target = "https://api.github.com/zen";
fetch(proxy + target)
  .then((r) => r.text())
  .then(console.log);
```

## 安全边界

any-proxy 的安全边界完全在**目标地址校验**：

- 拒绝本机、私网、链路本地、保留地址和云元数据地址
- IPv6 采用正向白名单（真 fail-closed）：仅允许 `2000::/3` 全局单播，并显式拒绝其中的 6to4 / Teredo / ORCHID 等嵌入 IPv4 的转换地址
- IPv4 采用穷举 denylist：不在 deny 列表中的地址视为公网（因 `Ipv4Addr::is_global()` 尚未稳定，这是取舍）
- DNS 解析与实际 TCP 连接在同一个 Connector 内原子绑定，防止 DNS rebinding
- HTTPS 目标在 TCP 连接建立后进行 TLS 握手，SNI 和证书校验使用原始规范化主机名
- 重定向后逐跳重新验证目标地址
- 阻止 HTTPS 降级到 HTTP
- 不读取 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 环境变量

### 已知残余风险

- 容器无法自动发现 NAT hairpin 对应的宿主公网地址，需通过 `DENY_CIDRS` 补充
- 宿主接口地址每 60 秒刷新，网络配置变化后最多 60 秒竞态窗口
- 允许任意公共端口 1-65535，实例可被用于端口扫描
- 公网匿名无额度控制的风险没有技术消除
- 新分配的 IANA IPv4 特殊用途段在补进 deny 列表前会被放行

> 完整的已知偏差清单见 [DESIGN.md 第 10 节](DESIGN.md#10-已知偏差汇总)。

## 配置

通过环境变量配置，所有值有默认值和校验边界：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `LISTEN_ADDR` | `0.0.0.0:8080` | 监听地址 |
| `DENY_CIDRS` | (空) | 额外拒绝的 CIDR 列表，逗号分隔 |
| `MAX_CONCURRENT_REQUESTS` | 256 | 进程级并发上限，达到上限立即返回 `503 service_overloaded` |
| `MAX_URI_BYTES` | 16384 | URI 最大字节数 |
| `DNS_TIMEOUT` | 5s | DNS 解析超时 |
| `CONNECT_TIMEOUT` | 10s | TCP 连接超时 |
| `TLS_TIMEOUT` | 10s | TLS 握手超时 |
| `UPSTREAM_HEADERS_TIMEOUT` | 30s | 等待上游响应 headers 超时 |
| `UPLOAD_IDLE_TIMEOUT` | 30s | 上传空闲超时 |
| `UPSTREAM_BODY_IDLE_TIMEOUT` | 60s | 下载空闲超时 |
| `SHUTDOWN_GRACE` | 30s | 优雅关闭等待时间 |
| `HOST_REFRESH_INTERVAL` | 60s | 宿主接口地址刷新间隔 |
| `RUST_LOG` | info | 日志级别 |

## HTTP 接口

| 路径 | 方法 | 说明 |
|------|------|------|
| `/<absolute-http-or-https-url>` | GET/HEAD/POST/PUT/PATCH/DELETE | 核心代理入口 |
| `/<absolute-http-or-https-url>` | OPTIONS | CORS 预检（固定 204） |
| `/healthz` | GET | 进程存活检查（不占用并发配额） |
| `/` | GET | 用法说明与风险提示 |
| (任意) | CONNECT/TRACE | 405 拒绝 |

### 错误码

响应头发出前的代理错误返回稳定 JSON：`{"error":{"code":"...","message":"...","request_id":"..."}}`。上游返回的合法 4xx/5xx 按原样转发，不转成代理错误。

| code | HTTP | 常见原因 | 排查动作 |
|------|------|----------|----------|
| `invalid_target` | 400 | URL 非法、带 userinfo、端口越界、URI 超长 | 检查代理前缀后的目标 URL 格式 |
| `target_blocked` | 403 | 目标解析到私网/保留/宿主地址，或 HTTPS 降级 | 确认目标是公网地址；是否命中 `DENY_CIDRS` |
| `method_not_allowed` | 405 | 使用了 CONNECT/TRACE 等不支持的方法 | 仅用 GET/HEAD/POST/PUT/PATCH/DELETE/OPTIONS |
| `dns_failed` | 502 | 域名无法解析或返回空答案 | 确认域名可解析；检查代理所在网络 DNS |
| `connect_failed` | 502 | 所有候选地址 TCP/TLS 均失败 | 目标端口是否开放；TLS 证书是否可信 |
| `upstream_failed` | 502 | 上游 HTTP 协议错误 | 确认目标返回合法 HTTP |
| `service_overloaded` | 503 | 活跃传输数达 `MAX_CONCURRENT_REQUESTS` | 稍后重试；或上调并发上限 / 扩容 |
| `connect_timeout` | 504 | DNS / TCP / TLS 阶段超时 | 目标是否慢；按需上调 `DNS_TIMEOUT` / `CONNECT_TIMEOUT` / `TLS_TIMEOUT` |
| `upstream_timeout` | 504 | 等待上游响应头超时 | 上调 `UPSTREAM_HEADERS_TIMEOUT` |

> 响应头一旦发出，后续 body 错误无法改写状态码：连接中止，调用方看到截断 body。

### 并发与超时语义

- **没有总请求时长上限。** 只要数据在流动，多长的传输都不会被代理主动中断。
- 卡死由**逐 frame 空闲超时**兜底：上传看 `UPLOAD_IDLE_TIMEOUT`，下载看 `UPSTREAM_BODY_IDLE_TIMEOUT`。
- **并发配额覆盖整个响应流的生命周期**，不是只到响应头发出为止。因此 `MAX_CONCURRENT_REQUESTS`
  限制的是「同时活跃的传输数」，这也是进程 socket 与上游连接任务的真实上界。
- 达到上限时**立即返回 `503 service_overloaded`**，不排队。调用方应当看到「过载」而不是「卡死」。
- 上游连接不复用：每次请求都走完整的 resolve → 全量校验 → 固定 IP → dial。

### CORS 行为

- 预检返回 `204 No Content`、`Access-Control-Allow-Origin: *`（不启用 credentials）
- `Access-Control-Allow-Methods` 固定为 `GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS`
- `Access-Control-Allow-Headers` 回显通过校验的请求 header 名
- `Access-Control-Expose-Headers` 固定暴露常用安全响应头
- 不转发浏览器 `Cookie`，不返回目标 `Set-Cookie`
- 允许转发目标 API 所需的 `Authorization`（跨 origin 重定向时自动删除）
- 所有代理生成的错误响应均添加 CORS headers

> **注意：** 放入浏览器 JavaScript 的目标 API 凭证对终端用户可见。凭证会经过自托管代理转发。

### Header 清理

- 请求/响应都处理 `Connection` header 的逗号分隔 token，删除其中点名的 headers
- 删除固定 hop-by-hop 集合：`Connection`、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Transfer-Encoding`、`Upgrade`
- 请求侧额外移除 `Host`、`Forwarded`、`Via`、`Cookie`，并通配清理 `Proxy-*` 与 `X-Forwarded-*` 前缀
- 响应侧额外移除 `Set-Cookie`、上游 CORS headers、`Server`、`X-Powered-By`；保留上游 `Vary` 并追加 CORS 缓存维度
- **trailer frame 一律丢弃、不透传**，避免 `Set-Cookie` / `Cookie` / CORS 借 trailer 绕过上述清理

## 开发

### 环境要求

- Rust 1.75+（MSRV）
- Cargo 1.75+

### 构建

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### 运行

```bash
cargo run --release
# 或指定监听地址
LISTEN_ADDR=127.0.0.1:9090 cargo run --release
```

### 测试

```bash
cargo test                          # 全部测试
cargo test --test relay             # M1 端到端集成测试
cargo test --test integration       # Connector 集成测试
cargo test --test tls_spike         # TLS spike 测试
cargo test --test concurrency       # 并发上限与流生命周期回归
cargo test --test concurrency -- --ignored   # 35 秒长传输回归，默认跳过
```

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M0 | ✅ 完成 | 安全连接器 spike（URL 解析、地址策略、Connector、重定向） |
| M1 | ✅ 完成 | 完整 Relay（Axum 接入、CORS、流式转发、Docker、优雅关闭）。存在已知偏差，见 [DESIGN.md 第 10 节](DESIGN.md#10-已知偏差汇总) |
| M2 | 待做 | 发布供应链（多架构镜像、SBOM、provenance、Prometheus） |

## 设计文档

当前设计见 [DESIGN.md](DESIGN.md)：安全模型、模块边界、流式与资源边界、错误契约，以及一份**已知偏差清单**。

立项时的设计推演与备选方案已归档为 [docs/adr/0001-any-proxy-relay-design.md](docs/adr/0001-any-proxy-relay-design.md)（历史快照，不再维护）。

待办与优先级见 [TODOS.md](TODOS.md)。

## 许可证

[MIT](LICENSE)
