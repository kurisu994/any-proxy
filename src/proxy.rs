//! 入站请求到上游请求的编排
//!
//! 参见 DESIGN.md Section 2-8。
//!
//! # 流程
//!
//! 1. 从 `OriginalUri` 解析目标 URL
//! 2. 处理 OPTIONS 预检
//! 3. 校验方法（405）
//! 4. 清理请求 headers
//! 5. 使用 Connector 建立 TCP(+TLS) 连接
//! 6. 使用 Hyper HTTP/1.1 client 发送请求
//! 7. 处理重定向（集成 RedirectMachine）
//! 8. 清理响应 headers，添加 CORS
//! 9. 返回流式响应

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::response::Response;
use http::request::Builder as RequestBuilder;

use crate::body_timeout;
use crate::config::Config;
use crate::connector::{Connector, Dialer};
use crate::error;
use crate::headers;
use crate::redirect::{Method as ProxyMethod, RedirectAction, RedirectMachine};
use crate::resolver::Resolver;
use crate::target::{parse_target, Target};
use crate::telemetry;

/// 代理共享状态
///
/// 手动实现 `Clone` 以避免 `derive(Clone)` 对 `R` 和 `D` 添加不必要的 `Clone` bound。
/// `Arc<T>` 总是 `Clone`，因此 `ProxyState` 总是 `Clone`。
pub struct ProxyState<R, D> {
    pub connector: Arc<Connector<R, D>>,
    pub config: Arc<Config>,
    pub budget: Arc<crate::budget::Budget>,
}

impl<R, D> Clone for ProxyState<R, D> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            config: self.config.clone(),
            budget: self.budget.clone(),
        }
    }
}

/// 核心代理处理器
///
/// 处理所有入站请求，根据路径和方法进行路由：
/// - `GET /healthz` → 健康检查
/// - `GET /` → 首页
/// - `OPTIONS /<url>` → CORS 预检
/// - `GET|HEAD|POST|PUT|PATCH|DELETE /<url>` → 代理转发
/// - 其他方法 → 405
pub async fn proxy_handler<R, D>(
    State(state): State<ProxyState<R, D>>,
    OriginalUri(original_uri): OriginalUri,
    method: http::Method,
    req_headers: http::HeaderMap,
    body: Body,
) -> Response
where
    R: Resolver + 'static,
    D: Dialer + 'static,
{
    let request_id = telemetry::generate_request_id();
    let start = std::time::Instant::now();

    let raw_path = original_uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("");

    // URI 大小检查
    if raw_path.len() > state.config.max_uri_bytes {
        let resp = error::build_error_response(
            &crate::ProxyError::InvalidTarget {
                message: "URI 超过最大长度限制".into(),
            },
            &request_id,
        );
        log_complete(&request_id, &method, &resp, start, None, None);
        return resp;
    }

    // 健康检查和首页
    if raw_path == "/healthz" {
        return error::build_healthz_response();
    }
    if raw_path == "/" || raw_path.is_empty() {
        return error::build_index_response();
    }

    // C1 访问控制：healthz / index 已豁免，以下对所有代理类请求（含预检）生效

    // AUTH_TOKEN 校验：配置后要求携带正确的 X-Proxy-Token
    if let Some(expected) = &state.config.auth_token {
        let provided = req_headers
            .get("x-proxy-token")
            .and_then(|v| v.to_str().ok());
        if provided != Some(expected.as_str()) {
            let resp = error::build_error_response(&crate::ProxyError::Unauthorized, &request_id);
            log_complete(
                &request_id,
                &method,
                &resp,
                start,
                Some("unauthorized"),
                None,
            );
            return resp;
        }
    }

    // Origin allowlist 校验，并计算成功响应要回显的 origin（未配置时用默认 `*`）
    let cors_origin: Option<String> = if state.config.allow_origins.is_empty() {
        None
    } else {
        match req_headers.get("origin").and_then(|v| v.to_str().ok()) {
            Some(o) if state.config.origin_allowed(o) => Some(o.to_string()),
            Some(_) => {
                // Origin 存在但不匹配：拒绝。message 不回显具体 origin。
                let resp = error::build_error_response(
                    &crate::ProxyError::TargetBlocked {
                        message: "Origin 不被允许".into(),
                    },
                    &request_id,
                );
                log_complete(
                    &request_id,
                    &method,
                    &resp,
                    start,
                    Some("target_blocked"),
                    None,
                );
                return resp;
            }
            // 无 Origin（非浏览器请求）：CORS 层不强制，用默认 `*`
            None => None,
        }
    };

    // 方法校验
    let proxy_method = match method.as_str() {
        "GET" => ProxyMethod::Get,
        "HEAD" => ProxyMethod::Head,
        "POST" => ProxyMethod::Post,
        "PUT" => ProxyMethod::Put,
        "PATCH" => ProxyMethod::Patch,
        "DELETE" => ProxyMethod::Delete,
        "OPTIONS" => ProxyMethod::Options,
        _ => {
            let resp = error::build_method_not_allowed_response(&request_id);
            log_complete(&request_id, &method, &resp, start, None, None);
            return resp;
        }
    };

    // OPTIONS 预检
    if proxy_method == ProxyMethod::Options {
        let mut resp = error::build_preflight_response_with_id(
            req_headers
                .get("access-control-request-method")
                .and_then(|v| v.to_str().ok()),
            req_headers
                .get("access-control-request-headers")
                .and_then(|v| v.to_str().ok()),
            &request_id,
        );
        apply_cors_origin(&mut resp, &cors_origin);
        log_complete(&request_id, &method, &resp, start, None, None);
        return resp;
    }

    // 解析目标 URL
    let target = match parse_target(raw_path) {
        Ok(t) => t,
        Err(e) => {
            let resp = error::build_error_response(&e, &request_id);
            log_complete(&request_id, &method, &resp, start, None, None);
            return resp;
        }
    };

    // C1 目标 host / 端口 allowlist（未配置时不限）
    if !state.config.target_allowed(&target.host.as_str())
        || !state.config.port_allowed(target.port)
    {
        // 被拒目标只进内部日志，对外 message 不回显具体 host/port
        tracing::debug!(host = %target.host, port = target.port, "目标不在 allowlist");
        let resp = error::build_error_response(
            &crate::ProxyError::TargetBlocked {
                message: "目标不在允许列表".into(),
            },
            &request_id,
        );
        log_complete(
            &request_id,
            &method,
            &resp,
            start,
            Some("target_blocked"),
            Some(&target),
        );
        return resp;
    }

    // C1 批次 2：准入检查（限速 429 + 出口预算 503），只对真正转发的请求生效
    if let Err(e) = state.budget.check_admission() {
        let code = e.code();
        let resp = error::build_error_response(&e, &request_id);
        log_complete(
            &request_id,
            &method,
            &resp,
            start,
            Some(code),
            Some(&target),
        );
        return resp;
    }

    // 对上传 body 应用 idle timeout（并把出口字节计入全局预算）
    let body = body_timeout::wrap_request_body(
        body,
        state.config.upload_idle_timeout,
        Arc::from(request_id.as_str()),
        state.budget.clone(),
    );

    // 执行代理请求（含重定向跟随）
    let result = forward_request(
        &state,
        &target,
        proxy_method,
        method.clone(),
        req_headers,
        body,
        &request_id,
    )
    .await;

    match result {
        Ok(mut resp) => {
            apply_cors_origin(&mut resp, &cors_origin);
            let error_code = if resp.status().is_server_error() {
                Some("upstream_error")
            } else {
                None
            };
            log_complete(
                &request_id,
                &method,
                &resp,
                start,
                error_code,
                Some(&target),
            );
            resp
        }
        Err(e) => {
            let code = e.code();
            let resp = error::build_error_response(&e, &request_id);
            log_complete(
                &request_id,
                &method,
                &resp,
                start,
                Some(code),
                Some(&target),
            );
            resp
        }
    }
}

/// 转发请求到上游，处理重定向
///
/// 重定向规则参见 DESIGN.md Section 5。
/// GET/HEAD 的重定向跟随使用空 body（不重放已消费的流式请求体）。
async fn forward_request<R, D>(
    state: &ProxyState<R, D>,
    initial_target: &Target,
    proxy_method: ProxyMethod,
    http_method: http::Method,
    mut req_headers: http::HeaderMap,
    body: Body,
    request_id: &str,
) -> Result<Response, crate::ProxyError>
where
    R: Resolver + 'static,
    D: Dialer + 'static,
{
    let mut redirect_machine = RedirectMachine::new();
    let mut current_target = initial_target.clone();

    // 首次访问记录
    redirect_machine.visit(&current_target);

    // 第一次使用原始 body，后续重定向使用空 body
    let mut body_opt = Some(body);

    loop {
        // 清理请求 headers
        let mut cleaned_headers = req_headers.clone();
        headers::clean_request_headers(&mut cleaned_headers);
        cleaned_headers.insert(
            http::header::HOST,
            headers::build_host_header(&current_target),
        );

        tracing::debug!(
            request_id = %request_id,
            target = %current_target.display_safe(),
            "连接上游"
        );

        // 建立 TCP(+TLS) 连接（Connector 内部有各阶段超时）
        let conn = state.connector.connect(&current_target).await?;

        // 包装 stream 为 Hyper Io
        let io = hyper_util::rt::TokioIo::new(conn.stream);

        // HTTP/1.1 握手
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| crate::ProxyError::UpstreamFailed {
                    message: format!("HTTP 握手失败: {e}"),
                })?;

        // 驱动连接直到自然结束。
        //
        // 这里**不能**再包一层总时长 timeout：连接 future 覆盖整个请求，
        // 给它设总上限等于给「任何一次代理传输」设总上限，
        // 会把持续有数据流动的长下载/长上传静默截断（客户端拿到 200 + 半截 body）。
        // 卡死场景由 body_timeout 的逐 frame idle timeout 兜底；
        // 连接任务的总量由 concurrency::limit_concurrency 的 permit 兜底
        // （permit 持有到响应 Body 结束，覆盖整个流的生命周期）。
        tokio::spawn(async move {
            let _ = connection.await;
        });

        // 构建上游请求
        let request_target = current_target.request_target();
        let req_body = body_opt.take().unwrap_or_else(Body::empty);

        let mut upstream_req = RequestBuilder::new()
            .method(http_method.clone())
            .uri(&request_target)
            .version(http::Version::HTTP_11)
            .body(req_body)
            .map_err(|e| crate::ProxyError::UpstreamFailed {
                message: format!("构建上游请求失败: {e}"),
            })?;

        *upstream_req.headers_mut() = cleaned_headers;

        // 发送请求并等待响应 headers（带超时）
        let response = tokio::time::timeout(
            state.config.upstream_headers_timeout,
            sender.send_request(upstream_req),
        )
        .await
        .map_err(|_| crate::ProxyError::UpstreamTimeout)?
        .map_err(|e| crate::ProxyError::UpstreamFailed {
            message: format!("上游请求失败: {e}"),
        })?;

        let status = response.status().as_u16();

        // 重定向处理
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok());

            match redirect_machine.handle_redirect(
                proxy_method,
                status,
                location,
                &current_target,
            )? {
                RedirectAction::Follow {
                    target: new_target,
                    strip_authorization,
                } => {
                    if strip_authorization {
                        req_headers.remove(http::header::AUTHORIZATION);
                    }
                    tracing::debug!(
                        request_id = %request_id,
                        status = status,
                        new_target = %new_target.display_safe(),
                        "跟随重定向"
                    );
                    current_target = new_target;
                    continue;
                }
                RedirectAction::PassThrough => {
                    // 原样返回 3xx
                    return build_proxy_response(
                        response,
                        &state.config,
                        Arc::from(request_id),
                        state.budget.clone(),
                    );
                }
            }
        }

        // 非重定向，返回响应
        return build_proxy_response(
            response,
            &state.config,
            Arc::from(request_id),
            state.budget.clone(),
        );
    }
}

/// 构建代理响应：清理 headers + 添加 CORS + 应用 idle timeout + 转换 body
fn build_proxy_response(
    response: hyper::Response<hyper::body::Incoming>,
    config: &Config,
    request_id: Arc<str>,
    budget: Arc<crate::budget::Budget>,
) -> Result<Response, crate::ProxyError> {
    let (mut parts, body) = response.into_parts();

    // 清理响应 headers 并添加 CORS
    headers::clean_response_headers(&mut parts.headers);
    headers::add_cors_headers(&mut parts.headers);

    // 应用逐 frame idle timeout 到响应 body（并把出口字节计入全局预算）
    let axum_body = body_timeout::wrap_response_body(
        body,
        config.upstream_body_idle_timeout,
        request_id,
        budget,
    );

    Ok(Response::from_parts(parts, axum_body))
}

/// 配置了 Origin allowlist 时，把响应的 `Access-Control-Allow-Origin` 从默认 `*`
/// 覆盖为回显的具体 origin，并追加 `Vary: Origin`（C1）。`None` 时保持默认。
fn apply_cors_origin(resp: &mut Response, origin: &Option<String>) {
    if let Some(o) = origin {
        if let Ok(v) = http::HeaderValue::from_str(o) {
            resp.headers_mut().insert("access-control-allow-origin", v);
            resp.headers_mut()
                .append("vary", http::HeaderValue::from_static("Origin"));
        }
    }
}

/// 记录请求完成日志（隐私过滤）
///
/// 记录 request_id、method、scheme、host、port、status、duration_ms 和可选 error_code。
/// scheme/host/port 仅在存在解析后的目标时填充（健康检查/预检/405 无目标，留空）。
/// 不记录 query、headers、cookie 或 body（DESIGN §9，N13）。
/// 流式字节计数由 `body_timeout` 在流结束/中止时按同一 request_id 记录。
fn log_complete(
    request_id: &str,
    method: &http::Method,
    response: &Response,
    start: std::time::Instant,
    error_code: Option<&str>,
    target: Option<&Target>,
) {
    let status = response.status().as_u16();
    let duration = start.elapsed();
    let scheme = target.map(|t| t.scheme.to_string()).unwrap_or_default();
    let host = target.map(|t| t.host.as_str()).unwrap_or_default();
    let port = target.map(|t| t.port).unwrap_or(0);

    tracing::info!(
        request_id = %request_id,
        method = %method,
        scheme = %scheme,
        host = %host,
        port = port,
        status = status,
        error_code = ?error_code,
        duration_ms = duration.as_millis() as u64,
        "request complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{BoxStream, DialRecord};
    use crate::resolver::{AddressPolicy, ResolveResult};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 假 Resolver：固定解析到 loopback，并记录调用次数
    struct CountingResolver {
        calls: Arc<AtomicUsize>,
    }

    impl Resolver for CountingResolver {
        async fn resolve(&self, _host: &str) -> Result<ResolveResult, crate::ProxyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResolveResult {
                addresses: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
            })
        }
    }

    /// 假 Dialer：在内存 duplex 的对端扮演上游，回放预设响应并记录收到的请求原文。
    /// `dials` 用于断言「某类请求根本没有触达网络」。
    #[derive(Clone)]
    struct ScriptedDialer {
        dials: Arc<AtomicUsize>,
        response: Arc<str>,
        seen_requests: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedDialer {
        fn new(response: &str) -> Self {
            Self {
                dials: Arc::new(AtomicUsize::new(0)),
                response: Arc::from(response),
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Dialer for ScriptedDialer {
        async fn dial(&self, addr: SocketAddr) -> Result<DialRecord, crate::ProxyError> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let (client, mut server) = tokio::io::duplex(16 * 1024);
            let response = self.response.clone();
            let seen = self.seen_requests.clone();

            tokio::spawn(async move {
                // 读到 header 结束即可（本组测试的请求都无 body）
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while let Ok(n) = server.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let _ = server.write_all(response.as_bytes()).await;
                let _ = server.flush().await;
            });

            Ok(DialRecord {
                peer_addr: addr,
                stream: BoxStream::new(client),
            })
        }
    }

    fn state_with(
        config: Config,
        dialer: ScriptedDialer,
    ) -> (
        ProxyState<CountingResolver, ScriptedDialer>,
        Arc<AtomicUsize>,
    ) {
        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let resolver = CountingResolver {
            calls: resolve_calls.clone(),
        };
        let connector = Arc::new(Connector::new(
            resolver,
            AddressPolicy::allow_all_for_test(),
            dialer,
        ));
        let budget = Arc::new(crate::budget::Budget::new(
            config.max_egress_bytes,
            config.rate_limit_rps,
        ));
        (
            ProxyState {
                connector,
                config: Arc::new(config),
                budget,
            },
            resolve_calls,
        )
    }

    /// 驱动一次 proxy_handler 调用
    async fn call(
        state: ProxyState<CountingResolver, ScriptedDialer>,
        method: &str,
        path: &str,
        headers: Vec<(&str, &str)>,
    ) -> Response {
        let mut hm = http::HeaderMap::new();
        for (k, v) in headers {
            hm.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        proxy_handler(
            State(state),
            OriginalUri(path.parse::<http::Uri>().unwrap()),
            http::Method::from_bytes(method.as_bytes()).unwrap(),
            hm,
            Body::empty(),
        )
        .await
    }

    const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";

    /// 未授权请求必须在触碰网络之前就被拒绝。
    ///
    /// 这是安全属性而非性能优化：若 401 之前就发生 DNS 解析或 dial，
    /// 未授权调用方即可拿代理当 DNS 预言机 / 端口探测器（对照 N5 的同类问题）。
    #[tokio::test]
    async fn test_unauthorized_request_never_resolves_or_dials() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            auth_token: Some("secret".into()),
            ..Default::default()
        };
        let (state, resolve_calls) = state_with(config, dialer.clone());

        let resp = call(state, "GET", "/http://example.com/x", vec![]).await;

        assert_eq!(resp.status(), 401);
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 0, "不应发生 DNS 解析");
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0, "不应发生 dial");
    }

    /// 不在 allowlist 的目标同样必须在解析/连接之前拒绝
    #[tokio::test]
    async fn test_blocked_target_never_resolves_or_dials() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            allow_targets: vec!["allowed.example".into()],
            ..Default::default()
        };
        let (state, resolve_calls) = state_with(config, dialer.clone());

        let resp = call(state, "GET", "/http://blocked.example/x", vec![]).await;

        assert_eq!(resp.status(), 403);
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 0, "不应发生 DNS 解析");
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0, "不应发生 dial");
    }

    /// 限速拒绝也发生在触网之前（C1 批次 2 准入检查的位置）
    #[tokio::test]
    async fn test_rate_limited_never_dials() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            rate_limit_rps: Some(1),
            ..Default::default()
        };
        let (state, _) = state_with(config, dialer.clone());

        // 第一次消耗掉令牌桶容量后，后续请求应被限速
        let mut last = call(state.clone(), "GET", "/http://example.com/a", vec![]).await;
        for _ in 0..5 {
            if last.status() == 429 {
                break;
            }
            last = call(state.clone(), "GET", "/http://example.com/a", vec![]).await;
        }

        assert_eq!(last.status(), 429, "耗尽令牌后应返回 429");
        let dials = dialer.dials.load(Ordering::SeqCst);
        assert!(dials >= 1, "被放行的请求应有 dial");
        // 被限速的那次不得 dial：dial 次数必须严格少于总请求数
        assert!(dials < 6, "被限速的请求不应触发 dial，实际 dial {dials} 次");
    }

    /// `/healthz` 豁免 AUTH_TOKEN：监控探针通常无法携带自定义 header，
    /// 若健康检查也要鉴权，编排系统会把存活实例误判为不健康。
    #[tokio::test]
    async fn test_healthz_exempt_from_auth_token() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            auth_token: Some("secret".into()),
            ..Default::default()
        };
        let (state, _) = state_with(config, dialer);

        let resp = call(state, "GET", "/healthz", vec![]).await;
        assert_eq!(resp.status(), 200, "/healthz 不应被 AUTH_TOKEN 拦截");
    }

    /// 配置 ALLOW_ORIGINS 且 Origin 命中时，回显具体 origin 并追加 `Vary: Origin`。
    /// 回显具体值而非 `*` 时缺少 Vary，会让共享缓存把 A 站的响应发给 B 站。
    #[tokio::test]
    async fn test_allowed_origin_echoed_with_vary() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            allow_origins: vec!["https://app.example".into()],
            ..Default::default()
        };
        let (state, _) = state_with(config, dialer);

        let resp = call(
            state,
            "GET",
            "/http://example.com/x",
            vec![("origin", "https://app.example")],
        )
        .await;

        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "https://app.example",
            "应回显具体 origin 而非 *"
        );
        let vary: Vec<_> = resp
            .headers()
            .get_all("vary")
            .iter()
            .map(|v| v.to_str().unwrap().to_ascii_lowercase())
            .collect();
        assert!(
            vary.iter().any(|v| v.contains("origin")),
            "回显具体 origin 时必须带 Vary: Origin，实际 {vary:?}"
        );
    }

    /// 不匹配的 Origin 被拒，且错误 message 不回显 origin 原值（对照 N5）
    #[tokio::test]
    async fn test_disallowed_origin_blocked_without_echo() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            allow_origins: vec!["https://app.example".into()],
            ..Default::default()
        };
        let (state, _) = state_with(config, dialer.clone());

        let resp = call(
            state,
            "GET",
            "/http://example.com/x",
            vec![("origin", "https://evil.example")],
        )
        .await;

        assert_eq!(resp.status(), 403);
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0, "不应发生 dial");

        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("evil.example"),
            "错误响应不应回显 origin 原值: {text}"
        );
    }

    /// 转发给上游的 Host 必须按目标重建，而不是透传入站 Host。
    /// 透传会让上游看到代理自己的主机名，虚拟主机路由随之出错。
    #[tokio::test]
    async fn test_host_header_rebuilt_from_target() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let (state, _) = state_with(Config::default(), dialer.clone());

        let resp = call(
            state,
            "GET",
            "/http://upstream.example:8080/x",
            vec![("host", "proxy.local")],
        )
        .await;
        assert_eq!(resp.status(), 200);

        let seen = dialer.seen_requests.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "应恰好触达上游一次");
        let lower = seen[0].to_ascii_lowercase();
        assert!(
            lower.contains("host: upstream.example:8080"),
            "Host 应按目标重建，实际请求:\n{}",
            seen[0]
        );
        assert!(
            !lower.contains("proxy.local"),
            "不应透传入站 Host，实际请求:\n{}",
            seen[0]
        );
    }

    /// 不支持的方法在解析目标之前就返回 405，且不触网
    #[tokio::test]
    async fn test_trace_method_rejected_before_dial() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let (state, resolve_calls) = state_with(Config::default(), dialer.clone());

        let resp = call(state, "TRACE", "/http://example.com/x", vec![]).await;

        assert_eq!(resp.status(), 405);
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 0);
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
    }

    /// URI 超长在任何解析动作之前被拒（防止超大输入进入解析路径）
    #[tokio::test]
    async fn test_oversized_uri_rejected_before_parse() {
        let dialer = ScriptedDialer::new(OK_RESPONSE);
        let config = Config {
            max_uri_bytes: 32,
            ..Default::default()
        };
        let (state, resolve_calls) = state_with(config, dialer.clone());

        let long_path = format!("/http://example.com/{}", "a".repeat(200));
        let resp = call(state, "GET", &long_path, vec![]).await;

        assert_eq!(resp.status(), 400);
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 0);
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
    }
}
