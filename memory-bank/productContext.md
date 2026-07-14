# 产品上下文

## 用户问题

浏览器调用第三方公共 API 时，用户可能同时遇到 CORS 限制和本地出口不可达。any-proxy 让请求从自托管服务器的网络出口发出，并统一添加允许浏览器读取响应的 CORS header。

服务只承诺"从部署服务器发起请求"，不承诺目标站点在任意地区可达，也不隐藏放在前端 JavaScript 中的 API 凭证。

## 核心使用流程

1. 部署者运行 any-proxy 服务（`cargo run` 或 Docker）并配置公网 HTTPS 入口（由外部反向代理终止）。
2. 调用方把公共 HTTP(S) 绝对 URL 放在代理域名后的 path 中。
3. 服务从原始 `path_and_query` 提取目标，只剥离一个前导 `/`，不二次解码。
4. 服务规范化目标，解析全部地址并进行 fail-closed 公网策略校验。
5. Connector 使用本次已验证的固定 IP 建立 TCP 连接；HTTPS 在 TCP 之上进行 TLS 握手，SNI 和证书校验使用原始 hostname。
6. Hyper HTTP/1.1 client 发送请求，Body 流式转发。
7. 若上游重定向，GET/HEAD 按策略逐跳重新校验；其他方法不自动重放 Body。
8. 响应 header 清理后添加统一 CORS headers，Body 流式返回调用方。

M1 已实现完整流程，用户可通过 `curl` 或浏览器实际调用代理服务。

## 产品决策

- 默认匿名开放，不要求 Token，不校验调用方 Origin，不限制公共目标域名。
- 不提供按调用方请求数、流量、响应大小的硬额度；进程级并发和超时仅用于资源回收。
- 允许任意公共端口 `1..=65535`，接受实例可能被用于公共端口扫描的残余风险。
- 浏览器凭证可转发到目标 API，但不转发 Cookie，不返回 Set-Cookie；公开前端中的凭证对终端用户可见。
- 服务内部只监听 HTTP，生产 HTTPS 由 Caddy、Nginx 或云负载均衡器终止。
- 不运营官方公共实例，由部署者自行设置账单告警、出口防火墙和实例网络策略。
- 地址策略 fail-closed：未知地址默认拒绝。

## 领域术语

- `Target`：规范化后的目标 scheme、host、port、path 和 query。
- `AddressPolicy`：fail-closed 地址策略，IPv6 仅允许 `2000::/3`，IPv4 穷举 deny 列表。
- `Resolver`：可替换的 DNS 解析边界，返回本次查询的全部 IP。
- `Connector`：在一次调用中执行 resolve → validate → dial → (TLS) 并核对 peer 的安全边界。
- `BoxStream`：包装 TCP 或 TLS stream 的统一异步读写类型。
- "零 dial"：目标被策略拒绝时，`Dialer` 调用次数必须为零。
- "连接固定"：实际连接地址必须属于同一次 DNS 结果的已验证集合，不能让下游客户端重新解析 hostname。
- "fail-closed"：未知或无法分类的地址默认拒绝，不因未命中 deny 列表而允许。
- `PassThrough`：重定向状态机不自动跟随，把上游 3xx 留给调用方处理。

## 风险与约束

- 公网匿名代理可能被滥用并耗尽带宽，项目不把它描述为生产安全方案。
- 容器无法可靠发现 NAT hairpin 对应的宿主公网地址，部署者需用 `DENY_CIDRS` 或出口防火墙补充。
- 本机接口列表变化会产生策略刷新窗口（60 秒），部署者应通过 `DENY_CIDRS` 显式补充。
- 目标 API 的 Authorization 经过自托管服务，日志和错误不得泄露 query、凭证、headers 或 Body。
- M1 默认关闭连接池，每次请求走完整连接流程，性能有改进空间。
