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

## 安全边界

any-proxy 的安全边界完全在**目标地址校验**：

- 拒绝本机、私网、链路本地、保留地址和云元数据地址
- IPv6 采用正向白名单（真 fail-closed）：仅允许 `2000::/3` 全局单播，其余一律拒绝
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
- `2000::/3` 内含 6to4、Teredo 等嵌入 IPv4 的转换地址，宿主有对应路由时构成条件式 SSRF（[TODOS](TODOS.md) E8）
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
- 删除固定 hop-by-hop 集合：`Connection`、`Keep-Alive`、`Proxy-*`、`TE`、`Trailer`、`Transfer-Encoding`、`Upgrade`
- 请求侧额外移除 `Host`、`Forwarded`、`Via`、`X-Forwarded-*`、`Cookie`
- 响应侧额外移除 `Set-Cookie`、上游 CORS headers、`Server`、`X-Powered-By`

> ⚠️ 当前实现有两处偏差：`Proxy-Connection` 等前缀未被通配清理；**trailer frame 会绕过上述全部清理**
> 并原样透传（[TODOS](TODOS.md) M2）。修复前不要依赖 trailer 侧的清理保证。

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
cargo test                          # 全部测试（126 个）
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
