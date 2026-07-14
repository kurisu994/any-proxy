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
}

impl<R, D> Clone for ProxyState<R, D> {
    fn clone(&self) -> Self {
        Self {
            connector: self.connector.clone(),
            config: self.config.clone(),
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
        log_complete(&request_id, &method, &resp, start);
        return resp;
    }

    // 健康检查和首页
    if raw_path == "/healthz" {
        return error::build_healthz_response();
    }
    if raw_path == "/" || raw_path.is_empty() {
        return error::build_index_response();
    }

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
            log_complete(&request_id, &method, &resp, start);
            return resp;
        }
    };

    // OPTIONS 预检
    if proxy_method == ProxyMethod::Options {
        let resp = error::build_preflight_response(
            req_headers
                .get("access-control-request-method")
                .and_then(|v| v.to_str().ok()),
            req_headers
                .get("access-control-request-headers")
                .and_then(|v| v.to_str().ok()),
        );
        log_complete(&request_id, &method, &resp, start);
        return resp;
    }

    // 解析目标 URL
    let target = match parse_target(raw_path) {
        Ok(t) => t,
        Err(e) => {
            let resp = error::build_error_response(&e, &request_id);
            log_complete(&request_id, &method, &resp, start);
            return resp;
        }
    };

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
        Ok(resp) => {
            log_complete(&request_id, &method, &resp, start);
            resp
        }
        Err(e) => {
            let resp = error::build_error_response(&e, &request_id);
            log_complete(&request_id, &method, &resp, start);
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
    _request_id: &str,
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

        // 建立 TCP(+TLS) 连接
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

        // 驱动连接
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

        // 发送请求（带超时）
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
                    current_target = new_target;
                    continue;
                }
                RedirectAction::PassThrough => {
                    // 原样返回 3xx
                    return build_proxy_response(response, &state.config);
                }
            }
        }

        // 非重定向，返回响应
        return build_proxy_response(response, &state.config);
    }
}

/// 构建代理响应：清理 headers + 添加 CORS + 应用 idle timeout + 转换 body
fn build_proxy_response(
    response: hyper::Response<hyper::body::Incoming>,
    config: &Config,
) -> Result<Response, crate::ProxyError> {
    let (mut parts, body) = response.into_parts();

    // 清理响应 headers 并添加 CORS
    headers::clean_response_headers(&mut parts.headers);
    headers::add_cors_headers(&mut parts.headers);

    // 应用逐 frame idle timeout 到响应 body
    let axum_body = body_timeout::wrap_response_body(body, config.upstream_body_idle_timeout);

    Ok(Response::from_parts(parts, axum_body))
}

/// 记录请求完成日志（隐私过滤）
fn log_complete(
    request_id: &str,
    method: &http::Method,
    response: &Response,
    start: std::time::Instant,
) {
    let status = response.status().as_u16();
    let duration = start.elapsed();

    tracing::info!(
        request_id = %request_id,
        method = %method,
        status = status,
        duration_ms = duration.as_millis() as u64,
        "request complete"
    );
}
