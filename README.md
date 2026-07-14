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

### Docker（M2 里程碑后可用）

```bash
docker run -d -p 8080:8080 ghcr.io/kurisu994/any-proxy:latest
```

## 安全边界

any-proxy 的安全边界完全在**目标地址校验**：

- 拒绝本机、私网、链路本地、保留地址和云元数据地址
- DNS 解析与实际 TCP 连接在同一个 Connector 内原子绑定，防止 DNS rebinding
- 重定向后逐跳重新验证目标地址
- 阻止 HTTPS 降级到 HTTP
- 不读取 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 环境变量

### 已知残余风险

- 容器无法自动发现 NAT hairpin 对应的宿主公网地址，需通过 `DENY_CIDRS` 补充
- 宿主接口地址每 60 秒刷新，网络配置变化后最多 60 秒竞态窗口
- 允许任意公共端口 1-65535，实例可被用于端口扫描
- 公网匿名无额度控制的风险没有技术消除

## 配置

通过环境变量配置：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `DENY_CIDRS` | (空) | 额外拒绝的 CIDR 列表，逗号分隔 |
| `MAX_CONCURRENT_REQUESTS` | 256 | 进程级并发上限 |
| `MAX_URI_BYTES` | 16384 | URI 最大字节数 |
| `DNS_TIMEOUT` | 5s | DNS 解析超时 |
| `CONNECT_TIMEOUT` | 10s | TCP 连接超时 |
| `TLS_TIMEOUT` | 10s | TLS 握手超时 |
| `UPSTREAM_HEADERS_TIMEOUT` | 30s | 等待上游响应 headers 超时 |
| `UPLOAD_IDLE_TIMEOUT` | 30s | 上传空闲超时 |
| `UPSTREAM_BODY_IDLE_TIMEOUT` | 60s | 下载空闲超时 |
| `SHUTDOWN_GRACE` | 30s | 优雅关闭等待时间 |

## HTTP 接口

| 路径 | 方法 | 说明 |
|------|------|------|
| `/<absolute-http-or-https-url>` | GET/HEAD/POST/PUT/PATCH/DELETE | 核心代理入口 |
| `/healthz` | GET | 进程存活检查 |
| `/` | GET | 用法说明与风险提示 |
| (任意) | OPTIONS | CORS 预检 |
| (任意) | CONNECT/TRACE | 405 拒绝 |

### CORS 行为

- `Access-Control-Allow-Origin: *`（不启用 credentials）
- 不转发浏览器 `Cookie`，不返回目标 `Set-Cookie`
- 允许转发目标 API 所需的 `Authorization`

> **注意：** 放入浏览器 JavaScript 的目标 API 凭证对终端用户可见。凭证会经过自托管代理转发。

## 开发

### 环境要求

- Rust 1.75+（MSRV）
- Cargo 1.75+

### 构建

```bash
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

### 手动公网验证（非阻塞）

```bash
E2E_PUBLIC=1 cargo test -- --ignored
```

仅在网络环境允许公网访问时运行，不是合并门槛。

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M0 | ✅ 完成 | 安全连接器 spike（URL 解析、地址策略、Connector、重定向） |
| M1 | 待做 | 完整 Relay（Axum 接入、CORS、流式转发、Docker） |
| M2 | 待做 | 发布供应链（多架构镜像、SBOM、provenance、Prometheus） |

## 设计文档

完整设计文档见 [DESIGN.md](DESIGN.md)，包含问题陈述、约束、安全模型、模块边界、测试计划和残余风险。

## 许可证

[MIT](LICENSE)
