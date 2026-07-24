//! 请求/响应头清理与 CORS 响应
//!
//! 参见 DESIGN.md Section 3（CORS behavior）和 Section 7（Header and trailer policy）。
//!
//! # 规则
//!
//! 请求侧：
//! - 解析 `Connection` header 的逗号分隔 token，删除其中点名的所有 headers
//! - 删除固定 hop-by-hop 集合
//! - 删除 `Host`、`Forwarded`、`Via`、`X-Forwarded-*`、`Proxy-*`、`Cookie` 和代理控制头
//! - 由客户端根据目标 URL 重建 `Host`
//! - 不转发 request trailers
//!
//! 响应侧：
//! - 同样处理 `Connection` hop-by-hop
//! - 删除 `Set-Cookie`、上游 CORS headers 和可能暴露代理内部信息的 headers
//! - 写入统一 CORS headers
//! - 不转发 response trailers

use http::header::{HeaderName, HeaderValue};

/// 固定 hop-by-hop headers（RFC 7230 Section 6.1）
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 请求侧额外移除的 headers
const REQUEST_STRIP: &[&str] = &[
    "host",
    "forwarded",
    "via",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
    "cookie",
    "x-proxy-request-id",
    "x-proxy-token",
];

/// 响应侧额外移除的 headers
const RESPONSE_STRIP: &[&str] = &[
    "set-cookie",
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-expose-headers",
    "access-control-allow-credentials",
    "access-control-max-age",
    "server",
    "x-powered-by",
];

/// 允许的 HTTP 方法
pub const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// 固定暴露的响应头
pub const EXPOSE_HEADERS: &str =
    "Content-Type, Content-Length, Content-Encoding, ETag, Last-Modified, Cache-Control, Expires, Location, Accept-Ranges, Content-Range, X-Proxy-Request-Id";

/// CORS 预检最大缓存时间（秒）
const PREFLIGHT_MAX_AGE: &str = "600";

/// 清理请求 headers
///
/// 1. 解析 `Connection` header 的逗号分隔 token，删除其中点名的所有 headers
/// 2. 删除固定 hop-by-hop 集合
/// 3. 删除请求侧额外移除的 headers
///
/// 调用方在清理后根据目标 URL 重建 `Host` header。
pub fn clean_request_headers(headers: &mut http::HeaderMap) {
    // 1. 收集 Connection header 中点名的 header 名
    let connection_tokens: Vec<String> = headers
        .get_all(http::header::CONNECTION)
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| {
            s.split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
        })
        .collect();

    // 2. 删除 Connection header 本身和点名的 headers
    headers.remove(http::header::CONNECTION);
    for token in &connection_tokens {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(&name);
        }
    }

    // 3. 删除固定 hop-by-hop
    for name in HOP_BY_HOP {
        if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&hn);
        }
    }

    // 4. 删除请求侧额外移除的 headers
    for name in REQUEST_STRIP {
        if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&hn);
        }
    }

    // 5. 通配删除 Proxy-* 与 X-Forwarded-* 前缀 header
    //    具名列表只覆盖少数名字，Proxy-Connection、X-Forwarded-Client-Cert 等仍会穿透，
    //    与 DESIGN §7「移除 Proxy-* / X-Forwarded-*」承诺不符（E9）。
    let prefixed: Vec<HeaderName> = headers
        .keys()
        .filter(|n| {
            let s = n.as_str();
            s.starts_with("proxy-") || s.starts_with("x-forwarded-")
        })
        .cloned()
        .collect();
    for name in prefixed {
        headers.remove(&name);
    }
}

/// 清理响应 headers
///
/// 1. 解析 `Connection` header 的逗号分隔 token，删除其中点名的所有 headers
/// 2. 删除固定 hop-by-hop 集合
/// 3. 删除 `Set-Cookie`、上游 CORS headers 和可能暴露代理内部信息的 headers
///
/// 调用方在清理后添加统一 CORS headers。
pub fn clean_response_headers(headers: &mut http::HeaderMap) {
    // 1. 收集 Connection header 中点名的 header 名
    let connection_tokens: Vec<String> = headers
        .get_all(http::header::CONNECTION)
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| {
            s.split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
        })
        .collect();

    // 2. 删除 Connection header 本身和点名的 headers
    headers.remove(http::header::CONNECTION);
    for token in &connection_tokens {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(&name);
        }
    }

    // 3. 删除固定 hop-by-hop
    for name in HOP_BY_HOP {
        if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&hn);
        }
    }

    // 4. 删除响应侧额外移除的 headers
    for name in RESPONSE_STRIP {
        if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&hn);
        }
    }
}

/// 向响应 headers 添加统一 CORS headers
///
/// - `Access-Control-Allow-Origin: *`
/// - `Access-Control-Expose-Headers: <固定列表>`
/// - `Vary: Access-Control-Request-Method, Access-Control-Request-Headers`
pub fn add_cors_headers(headers: &mut http::HeaderMap) {
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-expose-headers",
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
    // 追加而非覆盖：保留上游 Vary 的缓存维度（如 Accept-Encoding / Accept-Language），
    // 只补充 CORS 预检相关维度，避免下游缓存跨变体返回错误响应（N7，DESIGN §7）。
    headers.append(
        "vary",
        HeaderValue::from_static("Access-Control-Request-Method, Access-Control-Request-Headers"),
    );
}

/// 构建 CORS 预检响应 headers
///
/// 返回 `(204 No Content, headers)`，调用方组装为完整响应。
///
/// - `Access-Control-Allow-Origin: *`
/// - `Access-Control-Allow-Methods: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS`
/// - `Access-Control-Allow-Headers: <回显通过校验的请求 header 名>`
/// - `Access-Control-Max-Age: 600`
/// - `Vary: Access-Control-Request-Method, Access-Control-Request-Headers`
pub fn build_preflight_headers(
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Result<http::HeaderMap, crate::ProxyError> {
    // 校验 Access-Control-Request-Method
    if let Some(method) = request_method {
        let method_upper = method.to_uppercase();
        if !ALLOWED_METHODS.contains(&method_upper.as_str()) {
            return Err(crate::ProxyError::InvalidTarget {
                message: format!("不允许的预检方法: {method}"),
            });
        }
    }

    // 校验 Access-Control-Request-Headers 总长度不超过 8 KiB
    if let Some(hdrs) = request_headers {
        if hdrs.len() > 8192 {
            return Err(crate::ProxyError::InvalidTarget {
                message: "预检请求 headers 超过 8 KiB 上限".into(),
            });
        }
        // 每个 header 名按 RFC token 解析
        for token in hdrs.split(',') {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            if HeaderName::from_bytes(trimmed.as_bytes()).is_err() {
                return Err(crate::ProxyError::InvalidTarget {
                    message: format!("预检请求 header 名非法: {trimmed}"),
                });
            }
        }
    }

    let mut headers = http::HeaderMap::new();
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    // 回显通过校验的请求 header 名（原样回显，不修改大小写）
    let allow_headers = request_headers.unwrap_or("");
    headers.insert(
        "access-control-allow-headers",
        if allow_headers.is_empty() {
            HeaderValue::from_static("*")
        } else {
            HeaderValue::from_str(allow_headers).map_err(|e| crate::ProxyError::InvalidTarget {
                message: format!("预检 allow-headers 构建失败: {e}"),
            })?
        },
    );
    headers.insert(
        "access-control-max-age",
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    headers.insert(
        "vary",
        HeaderValue::from_static("Access-Control-Request-Method, Access-Control-Request-Headers"),
    );

    Ok(headers)
}

/// 根据目标 URL 重建 Host header
///
/// 默认端口省略，非默认端口包含在 Host 中。
pub fn build_host_header(target: &crate::target::Target) -> HeaderValue {
    let host_str = target.authority();
    HeaderValue::from_str(&host_str).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_request_removes_hop_by_hop() {
        let mut headers = http::HeaderMap::new();
        headers.insert("connection", "keep-alive, X-Custom".parse().unwrap());
        headers.insert("keep-alive", "timeout=30".parse().unwrap());
        headers.insert("x-custom", "value".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        clean_request_headers(&mut headers);

        assert!(headers.get("connection").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("x-custom").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("content-type").is_some()); // 保留
    }

    #[test]
    fn test_clean_request_removes_proxy_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert("host", "proxy.example.com".parse().unwrap());
        headers.insert("cookie", "session=abc".parse().unwrap());
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("forwarded", "for=1.2.3.4".parse().unwrap());
        headers.insert("via", "1.1 proxy".parse().unwrap());
        headers.insert("authorization", "Bearer token".parse().unwrap());

        clean_request_headers(&mut headers);

        assert!(headers.get("host").is_none());
        assert!(headers.get("cookie").is_none());
        assert!(headers.get("x-forwarded-for").is_none());
        assert!(headers.get("x-forwarded-proto").is_none());
        assert!(headers.get("forwarded").is_none());
        assert!(headers.get("via").is_none());
        assert!(headers.get("authorization").is_some()); // Authorization 保留
    }

    #[test]
    fn test_clean_response_removes_set_cookie_and_cors() {
        let mut headers = http::HeaderMap::new();
        headers.insert("set-cookie", "session=abc".parse().unwrap());
        headers.insert(
            "access-control-allow-origin",
            "https://evil.com".parse().unwrap(),
        );
        headers.insert("access-control-allow-methods", "GET".parse().unwrap());
        headers.insert("server", "nginx/1.21".parse().unwrap());
        headers.insert("x-powered-by", "PHP/8.0".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("etag", "\"abc123\"".parse().unwrap());

        clean_response_headers(&mut headers);

        assert!(headers.get("set-cookie").is_none());
        assert!(headers.get("access-control-allow-origin").is_none());
        assert!(headers.get("access-control-allow-methods").is_none());
        assert!(headers.get("server").is_none());
        assert!(headers.get("x-powered-by").is_none());
        assert!(headers.get("content-type").is_some()); // 保留
        assert!(headers.get("etag").is_some()); // 保留
    }

    #[test]
    fn test_clean_response_removes_connection_named_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert("connection", "close, X-Custom-Resp".parse().unwrap());
        headers.insert("x-custom-resp", "value".parse().unwrap());
        headers.insert("content-length", "42".parse().unwrap());

        clean_response_headers(&mut headers);

        assert!(headers.get("connection").is_none());
        assert!(headers.get("x-custom-resp").is_none());
        assert!(headers.get("content-length").is_some()); // 保留
    }

    #[test]
    fn test_add_cors_headers() {
        let mut headers = http::HeaderMap::new();
        add_cors_headers(&mut headers);

        assert_eq!(
            headers.get("access-control-allow-origin").unwrap(),
            &HeaderValue::from_static("*")
        );
        assert!(headers.get("access-control-expose-headers").is_some());
        assert!(headers.get("vary").is_some());
    }

    #[test]
    fn test_clean_response_preserves_upstream_vary() {
        // N7：上游 Vary 的缓存维度必须保留，不能被 CORS-only 值覆盖
        let mut headers = http::HeaderMap::new();
        headers.insert("vary", "Accept-Encoding".parse().unwrap());
        clean_response_headers(&mut headers);
        assert_eq!(headers.get("vary").unwrap(), "Accept-Encoding");

        // 追加 CORS 维度后，上游维度仍在（存在多个 Vary 字段）
        add_cors_headers(&mut headers);
        let varys: Vec<_> = headers
            .get_all("vary")
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(varys.iter().any(|v| v.contains("Accept-Encoding")));
        assert!(varys
            .iter()
            .any(|v| v.contains("Access-Control-Request-Method")));
    }

    #[test]
    fn test_clean_request_removes_proxy_and_forwarded_prefixes() {
        // E9：Proxy-* 与 X-Forwarded-* 前缀通配清理
        let mut headers = http::HeaderMap::new();
        headers.insert("proxy-connection", "keep-alive".parse().unwrap());
        headers.insert("proxy-foo", "bar".parse().unwrap());
        headers.insert("x-forwarded-client-cert", "cert".parse().unwrap());
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        clean_request_headers(&mut headers);

        assert!(headers.get("proxy-connection").is_none());
        assert!(headers.get("proxy-foo").is_none());
        assert!(headers.get("x-forwarded-client-cert").is_none());
        assert!(headers.get("x-forwarded-for").is_none());
        assert!(headers.get("content-type").is_some()); // 保留
    }

    #[test]
    fn test_build_preflight_headers_valid() {
        let headers =
            build_preflight_headers(Some("GET"), Some("Content-Type, Authorization")).unwrap();

        assert_eq!(
            headers.get("access-control-allow-origin").unwrap(),
            &HeaderValue::from_static("*")
        );
        assert_eq!(
            headers.get("access-control-allow-methods").unwrap(),
            &HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS")
        );
        assert_eq!(
            headers.get("access-control-allow-headers").unwrap(),
            "Content-Type, Authorization"
        );
        assert_eq!(
            headers.get("access-control-max-age").unwrap(),
            &HeaderValue::from_static("600")
        );
    }

    #[test]
    fn test_build_preflight_headers_empty_headers() {
        let headers = build_preflight_headers(Some("POST"), None).unwrap();
        assert_eq!(
            headers.get("access-control-allow-headers").unwrap(),
            &HeaderValue::from_static("*")
        );
    }

    #[test]
    fn test_build_preflight_headers_invalid_method() {
        let result = build_preflight_headers(Some("TRACE"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_preflight_headers_invalid_header_name() {
        let result = build_preflight_headers(Some("GET"), Some("Content-Type, invalid header!"));
        assert!(result.is_err());
    }

    #[test]
    fn test_build_preflight_headers_too_large() {
        let large = "X".repeat(9000);
        let result = build_preflight_headers(Some("GET"), Some(&large));
        assert!(result.is_err());
    }

    #[test]
    fn test_build_host_header_default_port() {
        let target = crate::target::Target {
            scheme: crate::target::Scheme::Https,
            host: crate::target::Host::Domain("example.com".into()),
            port: 443,
            path: "/".into(),
            query: String::new(),
        };
        let host = build_host_header(&target);
        assert_eq!(host, "example.com");
    }

    #[test]
    fn test_build_host_header_custom_port() {
        let target = crate::target::Target {
            scheme: crate::target::Scheme::Https,
            host: crate::target::Host::Domain("example.com".into()),
            port: 8443,
            path: "/".into(),
            query: String::new(),
        };
        let host = build_host_header(&target);
        assert_eq!(host, "example.com:8443");
    }
}
