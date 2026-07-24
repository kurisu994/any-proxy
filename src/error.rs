//! 稳定错误码与 HTTP 映射的 JSON 错误响应
//!
//! 参见 DESIGN.md Section 8（Error contract）。
//!
//! 只有响应 headers 发出前发生的代理错误使用稳定 JSON 结构：
//! ```json
//! {"error": {"code": "target_blocked", "message": "...", "request_id": "..."}}
//! ```
//!
//! 上游返回的合法 HTTP 4xx/5xx 按 Relay 语义原样转发，不转换成代理 JSON 错误。
//! 错误响应不返回已解析的私网 IP、完整查询参数、请求头或上游响应体。

use axum::body::Body;
use axum::response::Response;
use http::StatusCode;

use crate::headers;
use crate::ProxyError;

/// 错误响应 JSON 结构
#[derive(serde::Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(serde::Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
    request_id: String,
}

/// 从 `ProxyError` 构建 Axum JSON 错误响应
///
/// 包含 CORS headers 和 `X-Proxy-Request-Id` header。
/// 状态码来自 `ProxyError::http_status()`。
pub fn build_error_response(err: &ProxyError, request_id: &str) -> Response<Body> {
    let body = ErrorBody {
        error: ErrorDetail {
            code: err.code(),
            message: err.message().to_string(),
            request_id: request_id.to_string(),
        },
    };

    let json = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"serialize failed"}}"#.to_vec()
    });

    let mut response = Response::builder()
        .status(
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        )
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("x-proxy-request-id", request_id)
        .body(Body::from(json))
        .expect("错误响应构建不会失败");

    // 添加 CORS headers
    let headers = response.headers_mut();
    headers::add_cors_headers(headers);
    // X-Proxy-Request-Id 已经在 builder 中设置，但 CORS 的 expose-headers 包含它

    response
}

/// 构建 405 Method Not Allowed 错误响应
pub fn build_method_not_allowed_response(request_id: &str) -> Response<Body> {
    let body = ErrorBody {
        error: ErrorDetail {
            code: "method_not_allowed",
            message: "不支持的方法，允许 GET HEAD POST PUT PATCH DELETE OPTIONS".into(),
            request_id: request_id.to_string(),
        },
    };

    let json = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());

    let mut response = Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("x-proxy-request-id", request_id)
        .header(
            http::header::ALLOW,
            "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS",
        )
        .body(Body::from(json))
        .expect("错误响应构建不会失败");

    let headers = response.headers_mut();
    headers::add_cors_headers(headers);

    response
}

/// 构建 CORS 预检 204 No Content 响应
///
/// 保留旧接口（生成新 request_id），建议使用 `build_preflight_response_with_id`。
pub fn build_preflight_response(
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Response<Body> {
    build_preflight_response_with_id(request_method, request_headers, "")
}

/// 构建 CORS 预检 204 No Content 响应（带 request_id 关联）
///
/// 预检校验失败时使用传入的 request_id，确保日志可关联。
pub fn build_preflight_response_with_id(
    request_method: Option<&str>,
    request_headers: Option<&str>,
    request_id: &str,
) -> Response<Body> {
    match headers::build_preflight_headers(request_method, request_headers) {
        Ok(hdrs) => {
            let mut response = Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .expect("预检响应构建不会失败");
            *response.headers_mut() = hdrs;
            response
        }
        Err(err) => {
            // 预检校验失败返回 400 + CORS，使用传入的 request_id
            let id = if request_id.is_empty() {
                crate::telemetry::generate_request_id()
            } else {
                request_id.to_string()
            };
            let mut resp = build_error_response(&err, &id);
            // 覆盖状态码为 400
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            resp
        }
    }
}

/// 构建 / 首页响应（简短用法说明）
pub fn build_index_response() -> Response<Body> {
    let body = r#"any-proxy — 公网匿名 CORS Relay

用法：在公共 HTTP(S) URL 前加上代理前缀：
  https://proxy.example.com/https://api.example.com/data

风险：公网匿名开放代理，无额度控制。请勿用于生产环境或暴露到公网未经保护。

健康检查：GET /healthz
"#;

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .expect("首页响应构建不会失败");

    let headers = response.headers_mut();
    headers::add_cors_headers(headers);

    response
}

/// 构建 /healthz 健康检查响应
///
/// 返回 JSON `{"status":"ok","version":"<CARGO_PKG_VERSION>"}`，回显构建版本便于
/// 部署后核对与 canary（D6）。与其他响应一致带 CORS headers，便于浏览器探活（N14）。
pub fn build_healthz_response() -> Response<Body> {
    let body = format!(
        r#"{{"status":"ok","version":"{}"}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("健康检查响应构建不会失败");

    headers::add_cors_headers(response.headers_mut());

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    #[test]
    fn test_build_error_response_status() {
        let err = ProxyError::TargetBlocked {
            message: "测试".into(),
        };
        let resp = build_error_response(&err, "test-req-001");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            &HeaderValue::from_static("*")
        );
        assert_eq!(
            resp.headers().get("x-proxy-request-id").unwrap(),
            "test-req-001"
        );
    }

    #[test]
    fn test_build_error_response_body_json() {
        let err = ProxyError::DnsFailed {
            message: "DNS 失败".into(),
        };
        let resp = build_error_response(&err, "test-req-002");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            resp.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_build_method_not_allowed_response() {
        let resp = build_method_not_allowed_response("test-req-003");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(resp.headers().get(http::header::ALLOW).is_some());
    }

    #[test]
    fn test_build_preflight_response_valid() {
        let resp = build_preflight_response(Some("GET"), Some("Content-Type"));
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn test_build_preflight_response_invalid_method() {
        let resp = build_preflight_response(Some("TRACE"), None);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_build_healthz_response() {
        let resp = build_healthz_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // N14：健康检查也带 CORS，便于浏览器探活
        assert!(resp.headers().get("access-control-allow-origin").is_some());
        // D6：回显版本号，content-type 为 JSON
        assert_eq!(
            resp.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_build_index_response() {
        let resp = build_index_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("access-control-allow-origin").is_some());
    }
}
