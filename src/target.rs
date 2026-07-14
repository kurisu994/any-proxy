//! URL 解析、协议与端口校验
//!
//! 从 Hyper/Axum 的原始 `OriginalUri` 取得目标 URL，
//! 只剥离恰好一个前导 `/`，不折叠重复斜线，不提前 percent-decode。
//!
//! 参见 DESIGN.md Section 2（HTTP interface）和 Section 4（Target validation）。

use std::net::IpAddr;

/// 解析后的目标 URL
///
/// 包含规范化后的 scheme、host、port 和原始 query。
/// 不包含 userinfo 和 fragment（前者被拒绝，后者不会出现在 HTTP request-target 中）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `http` 或 `https`
    pub scheme: Scheme,
    /// 规范化后的主机名（IDNA 已解析，尾点已去除）或 IP literal
    pub host: Host,
    /// 显式端口（未指定时为 scheme 默认端口）
    pub port: u16,
    /// URL 路径（含前导 `/`，根路径为 `/`）
    pub path: String,
    /// 原始 query string（含前导 `?`，可能为空）
    pub query: String,
}

/// URL scheme
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    /// 默认端口
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }

    /// 是否为 TLS
    pub fn is_tls(self) -> bool {
        matches!(self, Self::Https)
    }
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => write!(f, "http"),
            Self::Https => write!(f, "https"),
        }
    }
}

/// 主机类型
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Host {
    /// 域名（已 IDNA 解析，尾点去除）
    Domain(String),
    /// IP literal
    Ip(IpAddr),
}

impl Host {
    /// 返回用于 DNS 查询或连接的主机字符串
    pub fn as_str(&self) -> String {
        match self {
            Self::Domain(d) => d.clone(),
            Self::Ip(ip) => ip.to_string(),
        }
    }

    /// 是否为 IP literal
    pub fn is_ip(&self) -> bool {
        matches!(self, Self::Ip(_))
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Domain(d) => write!(f, "{d}"),
            Self::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

/// 从原始 path_and_query 字符串解析目标 URL
///
/// 输入应为 Hyper/Axum `OriginalUri` 的 `path_and_query()` 结果，
/// 带有一个前导 `/`（例如 `/https://example.com/data?q=1`）。
///
/// # 校验规则
///
/// - 只允许 `http` 与 `https` 协议
/// - 拒绝 userinfo（`http://user:pass@host`）
/// - 拒绝空主机
/// - 拒绝非法端口（0 和 >65535）
/// - 允许端口 1..=65535
/// - 不对 percent-encoding 进行二次解码
/// - fragment 不会出现在 HTTP request-target 中
///
/// # 错误
///
/// 返回 [`ProxyError::InvalidTarget`] 当 URL 格式非法。
pub fn parse_target(raw_path_and_query: &str) -> Result<Target, crate::ProxyError> {
    // 只剥离恰好一个前导 `/`
    let url_str = match raw_path_and_query.strip_prefix('/') {
        Some(rest) => rest,
        None => raw_path_and_query,
    };

    // 使用标准 URL 解析器解析一次
    let parsed = url::Url::parse(url_str).map_err(|e| crate::ProxyError::InvalidTarget {
        message: format!("URL 解析失败: {e}"),
    })?;

    // 只允许 http 与 https
    let scheme = match parsed.scheme() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        other => {
            return Err(crate::ProxyError::InvalidTarget {
                message: format!("不允许的协议: {other}"),
            });
        }
    };

    // 拒绝 userinfo
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(crate::ProxyError::InvalidTarget {
            message: "不允许 userinfo".into(),
        });
    }

    // 拒绝空主机
    let host_str = parsed
        .host_str()
        .ok_or_else(|| crate::ProxyError::InvalidTarget {
            message: "缺少主机名".into(),
        })?;
    if host_str.is_empty() {
        return Err(crate::ProxyError::InvalidTarget {
            message: "主机名为空".into(),
        });
    }

    // 解析主机
    let host = parse_host(host_str)?;

    // 端口校验
    let port = parsed.port().unwrap_or(scheme.default_port());
    if port == 0 {
        return Err(crate::ProxyError::InvalidTarget {
            message: "端口不能为 0".into(),
        });
    }

    // query 原样保留
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();

    // path 原样保留（url crate 默认去除尾部斜线，但非根路径保留）
    let path = parsed.path().to_string();
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };

    // fragment 不会出现在 HTTP request-target 中，忽略

    Ok(Target {
        scheme,
        host,
        port,
        path,
        query,
    })
}

/// 解析主机字符串为 `Host`
///
/// 处理 IPv4 literal、IPv6 literal 和域名（含尾点去除）。
/// 整数/八进制/十六进制等非标准 IPv4 文本由标准解析器处理，
/// 如果标准解析器不接受则直接拒绝。
fn parse_host(host_str: &str) -> Result<Host, crate::ProxyError> {
    // 去除尾点
    let trimmed = host_str.trim_end_matches('.');

    // 尝试解析为 IP 地址
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(Host::Ip(ip));
    }

    // IPv6 literal 可能被方括号包裹（如 `[::1]`）
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Ok(ip) = inner.parse::<IpAddr>() {
            return Ok(Host::Ip(ip));
        }
    }

    // 域名 — 不允许空字符串
    if trimmed.is_empty() {
        return Err(crate::ProxyError::InvalidTarget {
            message: "主机名为空".into(),
        });
    }

    Ok(Host::Domain(trimmed.to_string()))
}

impl Target {
    /// 返回 authority 字符串（host + 非默认端口）
    ///
    /// 默认端口（HTTP 80 / HTTPS 443）省略，用于重定向环检测和 Location 拼接。
    pub fn authority(&self) -> String {
        let port_str = if self.port == self.scheme.default_port() {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!("{}{}", self.host, port_str)
    }

    /// 返回 HTTP request-target（path + query）
    ///
    /// 用于发送给上游的 HTTP/1.1 请求行，如 `/data?city=shanghai`。
    pub fn request_target(&self) -> String {
        format!("{}{}", self.path, self.query)
    }

    /// 返回完整的规范化 URL（scheme://authority/path?query）
    ///
    /// 用于日志和调试，不含 userinfo 和 fragment。
    pub fn full_url(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme,
            self.authority(),
            self.request_target()
        )
    }
}

/// 主机名的规范化形式（用于 DNS 查询、Host header 和 TLS SNI）
///
/// 对域名返回小写形式；对 IP 返回标准字符串表示。
pub fn canonical_host(host: &Host) -> String {
    match host {
        Host::Domain(d) => d.to_lowercase(),
        Host::Ip(ip) => ip.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_url() {
        let target = parse_target("/https://example.com/data?city=shanghai").unwrap();
        assert_eq!(target.scheme, Scheme::Https);
        assert_eq!(target.host, Host::Domain("example.com".into()));
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/data");
        assert_eq!(target.query, "?city=shanghai");
    }

    #[test]
    fn test_parse_http_scheme() {
        let target = parse_target("/http://example.com/").unwrap();
        assert_eq!(target.scheme, Scheme::Http);
        assert_eq!(target.port, 80);
    }

    #[test]
    fn test_parse_explicit_port() {
        let target = parse_target("/https://example.com:8443/").unwrap();
        assert_eq!(target.port, 8443);
    }

    #[test]
    fn test_parse_port_1_and_65535() {
        assert!(parse_target("/http://example.com:1/").is_ok());
        assert!(parse_target("/http://example.com:65535/").is_ok());
    }

    #[test]
    fn test_reject_port_0() {
        assert!(parse_target("/http://example.com:0/").is_err());
    }

    #[test]
    fn test_reject_userinfo() {
        assert!(parse_target("/https://user:pass@example.com/").is_err());
    }

    #[test]
    fn test_reject_empty_host() {
        // url crate 可能将 `https:///path` 的 host 解析为 None 而非空字符串
        // 测试多种空主机形式
        assert!(parse_target("/https://%/path").is_err());
    }

    #[test]
    fn test_reject_non_http_scheme() {
        assert!(parse_target("/ftp://example.com/").is_err());
        assert!(parse_target("/ws://example.com/").is_err());
    }

    #[test]
    fn test_ipv4_literal() {
        let target = parse_target("/https://1.2.3.4/").unwrap();
        assert_eq!(target.host, Host::Ip("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn test_ipv6_literal() {
        let target = parse_target("/https://[::1]/").unwrap();
        assert_eq!(target.host, Host::Ip("::1".parse().unwrap()));
    }

    #[test]
    fn test_trailing_dot_domain() {
        let target = parse_target("/https://example.com./").unwrap();
        assert_eq!(target.host, Host::Domain("example.com".into()));
    }

    #[test]
    fn test_no_double_decode() {
        // /https%3A%2F%2Fexample.com 不会被二次解码
        assert!(parse_target("/https%3A%2F%2Fexample.com").is_err());
    }

    #[test]
    fn test_query_preserved() {
        let target = parse_target("/https://example.com/path?a=1&b=2").unwrap();
        assert_eq!(target.path, "/path");
        assert_eq!(target.query, "?a=1&b=2");
    }

    #[test]
    fn test_no_query() {
        let target = parse_target("/https://example.com/path").unwrap();
        assert_eq!(target.path, "/path");
        assert_eq!(target.query, "");
    }

    #[test]
    fn test_scheme_default_port() {
        assert_eq!(Scheme::Http.default_port(), 80);
        assert_eq!(Scheme::Https.default_port(), 443);
        assert!(Scheme::Https.is_tls());
        assert!(!Scheme::Http.is_tls());
    }

    #[test]
    fn test_request_target() {
        let target = parse_target("/https://example.com/data?q=1").unwrap();
        assert_eq!(target.request_target(), "/data?q=1");
    }

    #[test]
    fn test_full_url() {
        let target = parse_target("/https://example.com:8443/data?q=1").unwrap();
        assert_eq!(target.full_url(), "https://example.com:8443/data?q=1");
    }
}
