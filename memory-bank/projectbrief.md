# 项目简介

## 项目定位

any-proxy 是面向中国大陆前端开发者和独立开发者的开源、自托管 HTTP(S) CORS Relay。使用者把目标绝对 URL 追加到代理服务地址之后，由部署服务器代发请求，并让浏览器能够读取带 CORS 响应头的结果。

```text
目标：https://api.example.com/data?q=1
代理：https://proxy.example.com/https://api.example.com/data?q=1
```

项目使用 Rust 实现，最终目标是单进程服务与版本化容器镜像。核心价值不是提供通用网络隧道，而是在一个可部署组件内组合 URL 前缀调用方式、流式 HTTP 转发、CORS 处理和逐跳公网目标校验。

## 目标用户与交付形态

- 需要从浏览器调用第三方公共 HTTP API，但受跨域或本地网络出口限制的开发者。
- 希望自行选择服务器出口、自行承担带宽和滥用风险的部署者。
- 首发形态为开源自托管工具，不运营官方公共代理实例。

## 范围

首版只代理公共 HTTP/HTTPS URL。规划支持 GET、HEAD、POST、PUT、PATCH、DELETE，并在代理入口处理 OPTIONS 预检。不支持 TCP、UDP、SOCKS、CONNECT、TRACE、WebSocket、整机代理、管理 UI、账户系统或按调用方配额。

安全底线是阻止 SSRF：私网、本机、链路本地、保留地址、云元数据地址和自定义拒绝网段必须在发生上游连接之前被拒绝。DNS 解析、地址校验和实际 dial 必须绑定，重定向必须逐跳复查。

## 里程碑

- M0：安全连接器 spike。当前仓库代码已完成 URL/地址策略、可注入 Resolver/Dialer、Connector、重定向状态机及本地 HTTP(S)/TLS 测试。
- M1：完整 Relay。尚未实现 Axum 入口、CORS/header 策略、流式 Body、配置、日志、取消与 Docker。
- M2：发布供应链。尚未实现多架构镜像、SBOM、provenance、Prometheus 和发布产物。

## 完成标准

项目只有在危险目标产生零次 dial、实际 peer 属于同一次已验证地址集合、逐跳重定向维持安全边界之后，才能继续实现完整 Relay。即使功能完成，公网匿名且无硬额度的风险仍存在，交付状态保持 `DONE_WITH_CONCERNS`。
