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

### 预构建镜像（推荐，无需编译）

CI 在打 SemVer tag 时构建 `linux/amd64` + `linux/arm64` 多架构镜像并推送到 GHCR：

```bash
docker run --rm -p 8080:8080 \
  -e ALLOW_TARGETS=api.github.com \
  ghcr.io/kurisu994/any-proxy:0.1.0
```

版本请固定，不要用 `latest`——tag 可被覆盖，只有固定版本（或 digest）才能保证「重建即同一份产物」。要求更强时按 digest 固定：

```bash
docker pull ghcr.io/kurisu994/any-proxy@sha256:<digest>
```

每次发布的 digest 打印在对应 Actions 运行的 summary 里。

> 上面示例带了 `ALLOW_TARGETS`：监听非 loopback 地址且零防护时进程会拒绝启动，详见[安全带](#安全带可选默认全关)。

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

健康检查（回显构建版本号）：

```bash
curl http://localhost:8080/healthz
# {"status":"ok","version":"0.1.0"}
```

### 公网 HTTPS（Caddy 自动 TLS）

仓库自带 `docker-compose.caddy.yml` + `Caddyfile`，一条命令起一个带证书的公网实例：

```bash
DOMAIN=proxy.example.com docker compose -f docker-compose.caddy.yml up -d
```

前提：`DOMAIN` 的 A/AAAA 记录已指向本机公网 IP，且 80/443 可从公网访问（Caddy 走 ACME 挑战签发 Let's Encrypt 证书）。

这套编排与单机 `docker-compose.yml` 有四点不同，都是刻意的：

- **默认拉预构建镜像**（固定版本，非 `latest`），不在 VPS 上编译 Rust——小内存机器编译本项目容易直接 OOM。要跑本地改动：`docker build -t any-proxy:dev . && ANY_PROXY_IMAGE=any-proxy:dev docker compose -f docker-compose.caddy.yml up -d`。
- **any-proxy 不映射宿主端口**，只在 compose 内部网络可达，公网入口只有 Caddy。
- **默认带安全带**：`ALLOW_TARGETS=api.github.com`、`ALLOW_PORTS=80,443`、`RATE_LIMIT_RPS=10`、`MAX_EGRESS_BYTES=10GiB`。公网 HTTPS 降低了暴露门槛，所以默认必须收紧，放宽是显式选择：

  ```bash
  DOMAIN=proxy.example.com ALLOW_TARGETS=.example.com,api.github.com \
    docker compose -f docker-compose.caddy.yml up -d
  ```

- **Caddy 关闭响应缓冲**（`flush_interval -1`）且不设上游响应超时，保持流式语义与「没有总时长上限」的约定。

反向代理会把请求路径里的 `//` 折叠成 `/`，因此到达本服务的可能是 `/https:/api.github.com/zen`。这是被支持的形态（回归测试 `target::tests::test_collapsed_slashes_from_reverse_proxy` 锁定），无需额外重写规则。Caddy 注入的 `X-Forwarded-*` 也会被请求 header 清理通配删除，不会转发给上游。

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
- 默认允许任意公共端口 1-65535，实例可被用于端口扫描（可用 `ALLOW_PORTS` 收紧到 `80,443`）
- 公网匿名无额度控制的风险没有技术消除（可用 `ALLOW_TARGETS` / `AUTH_TOKEN` 收紧访问面，`MAX_EGRESS_BYTES` / `RATE_LIMIT_RPS` 为带宽账单兜底）
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
| `ALLOW_TARGETS` | (空) | 目标 host allowlist，逗号分隔；空=不限，支持后缀 `.x.com` 匹配子域 |
| `ALLOW_ORIGINS` | (空) | Origin allowlist，逗号分隔；空=回显 `*`，命中回显具体 origin，否则 403 |
| `AUTH_TOKEN` | (空) | 共享代理令牌；配置后请求需带 `X-Proxy-Token`，否则 401 |
| `ALLOW_PORTS` | (空) | 目标端口 allowlist，逗号分隔；空=不限（1-65535） |
| `PUBLIC_MODE` | false | 显式确认公网匿名开放；非 loopback 监听且无其它防护时必须开启才允许启动 |
| `MAX_EGRESS_BYTES` | (空) | 全局累计出口字节上限；达到后新请求返回 `503 budget_exceeded`（进程重启重置） |
| `RATE_LIMIT_RPS` | (空) | 全局请求速率上限（令牌桶）；超过返回 `429 rate_limited` |
| `DNS_TIMEOUT` | 5s | DNS 解析超时 |
| `CONNECT_TIMEOUT` | 10s | TCP 连接超时 |
| `TLS_TIMEOUT` | 10s | TLS 握手超时 |
| `UPSTREAM_HEADERS_TIMEOUT` | 30s | 等待上游响应 headers 超时 |
| `UPLOAD_IDLE_TIMEOUT` | 30s | 上传空闲超时 |
| `UPSTREAM_BODY_IDLE_TIMEOUT` | 60s | 下载空闲超时 |
| `SHUTDOWN_GRACE` | 30s | 优雅关闭等待时间 |
| `HOST_REFRESH_INTERVAL` | 60s | 宿主接口地址刷新间隔 |
| `RUST_LOG` | info | 日志级别 |

### 安全带（可选，默认全关）

以上 `ALLOW_TARGETS` / `ALLOW_ORIGINS` / `AUTH_TOKEN` / `ALLOW_PORTS` 都是**可选、默认关**的访问控制，只在显式配置时生效，不改变零配置的匿名默认行为。它们是 additive 的，可任意组合。

**启动 gate**：唯一改变默认体验的是——当监听地址**不是 loopback**（如默认的 `0.0.0.0`）**且**未配置任何 `ALLOW_TARGETS` / `AUTH_TOKEN`、也未设 `PUBLIC_MODE=1` 时，进程**拒绝启动**。这是为了堵住「无意识地把开放代理推上公网」。解决办法四选一：

```bash
# 1) 显式确认公网匿名开放的风险
PUBLIC_MODE=1 ./any-proxy
# 2) 限制可代理的目标
ALLOW_TARGETS=api.github.com,.example.com ./any-proxy
# 3) 要求调用方令牌
AUTH_TOKEN=your-secret ./any-proxy   # 请求需带 X-Proxy-Token: your-secret
# 4) 仅本机监听（不触发 gate）
LISTEN_ADDR=127.0.0.1:8080 ./any-proxy
```

> 说明：`ALLOW_TARGETS` 与 `ALLOW_PORTS` 不依赖调用方身份，是最有效的收紧手段；`X-Proxy-Token` 在转发前会被删除，不会泄漏给上游；纯前端场景令牌对终端用户可见，价值有限。

**流量兜底**：`MAX_EGRESS_BYTES`（全局累计出口字节上限，进程重启重置）与 `RATE_LIMIT_RPS`（全局令牌桶限速）为带宽账单提供技术兜底，同样默认关、不依赖调用方身份。出口字节统计请求体与响应体两个方向。

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
| `unauthorized` | 401 | 配置 `AUTH_TOKEN` 后缺少/错误的 `X-Proxy-Token` | 请求带上正确的 `X-Proxy-Token` header |
| `target_blocked` | 403 | 目标非公网/降级，或不在 `ALLOW_TARGETS`/`ALLOW_PORTS`/`ALLOW_ORIGINS` | 确认目标合规；检查 allowlist 配置 |
| `rate_limited` | 429 | 请求速率超过 `RATE_LIMIT_RPS` | 降低请求频率或上调速率上限 |
| `method_not_allowed` | 405 | 使用了 CONNECT/TRACE 等不支持的方法 | 仅用 GET/HEAD/POST/PUT/PATCH/DELETE/OPTIONS |
| `dns_failed` | 502 | 域名无法解析或返回空答案 | 确认域名可解析；检查代理所在网络 DNS |
| `connect_failed` | 502 | 所有候选地址 TCP/TLS 均失败 | 目标端口是否开放；TLS 证书是否可信 |
| `upstream_failed` | 502 | 上游 HTTP 协议错误 | 确认目标返回合法 HTTP |
| `service_overloaded` | 503 | 活跃传输数达 `MAX_CONCURRENT_REQUESTS` | 稍后重试；或上调并发上限 / 扩容 |
| `budget_exceeded` | 503 | 累计出口字节达 `MAX_EGRESS_BYTES` | 重启重置计数；或上调预算 |
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

- Rust 1.86+（MSRV，受 `url` / `icu` 传递依赖约束，CI 有专门作业锁定）
- Cargo 1.86+

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

### 发布

镜像由 tag 触发发布，流程固定为三步：

```bash
# 1. 改 Cargo.toml 的 version（/healthz 回显的就是它）
# 2. 提交
git commit -am "release: v0.2.0"
# 3. 打 tag 推送，CI 自动构建多架构镜像并推 GHCR
git tag v0.2.0 && git push origin main v0.2.0
```

CI 会先校验 git tag 与 `Cargo.toml` 的 version 一致，不一致直接失败——否则会发布出「自称版本与 tag 不符」的镜像。两个架构各自在原生 runner 上构建后合成 manifest list，tag 规则为 `0.2.0` / `0.2` / `latest`（`0.x` 不生成 major tag，因为 0.x 不承诺兼容性）。

> 首次发布后需在 GitHub 的 package 设置里把可见性改为 Public，否则匿名用户无法 `docker pull`。

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M0 | ✅ 完成 | 安全连接器 spike（URL 解析、地址策略、Connector、重定向） |
| M1 | ✅ 完成 | 完整 Relay（Axum 接入、CORS、流式转发、Docker、优雅关闭）。存在已知偏差，见 [DESIGN.md 第 10 节](DESIGN.md#10-已知偏差汇总) |
| M2 | ✅ 收敛 | 发布供应链。多架构预构建镜像与 CI 容器冒烟已就位；SBOM / provenance / Prometheus 按项目定位关闭，见 [TODOS.md](TODOS.md) |

## 设计文档

当前设计见 [DESIGN.md](DESIGN.md)：安全模型、模块边界、流式与资源边界、错误契约，以及一份**已知偏差清单**。

立项时的设计推演与备选方案已归档为 [docs/adr/0001-any-proxy-relay-design.md](docs/adr/0001-any-proxy-relay-design.md)（历史快照，不再维护）。

待办与优先级见 [TODOS.md](TODOS.md)。

## 许可证

[MIT](LICENSE)
