//! any-proxy — Rust 公网匿名 CORS Relay
//!
//! M0: 安全连接器 spike
//!
//! 模块结构：
//! - [`target`] — URL 解析、协议与端口校验
//! - [`resolver`] — DNS 解析（含 CNAME 跟踪）、公网 IP 校验（AddressPolicy）
//! - [`connector`] — 安全连接编排：resolve → validate → dial → 记录 peer_addr
//! - [`redirect`] — 重定向状态机与逐跳复查

pub mod connector;
pub mod redirect;
pub mod resolver;
pub mod target;

/// 代理错误码
///
/// 对应 DESIGN.md Section 8 的稳定错误码与 HTTP 映射。
/// M0 阶段只使用 `invalid_target`、`target_blocked`、`dns_failed`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ProxyError {
    /// URL、协议、userinfo 或端口格式非法（HTTP 400）
    InvalidTarget { message: String },
    /// 解析到非公网地址、HTTPS 降级或被安全规则拒绝（HTTP 403）
    TargetBlocked { message: String },
    /// DNS 解析失败（HTTP 502）
    DnsFailed { message: String },
    /// TCP/TLS 连接失败（HTTP 502）
    ConnectFailed { message: String },
    /// 上游协议失败（HTTP 502）
    UpstreamFailed { message: String },
    /// 连接阶段超时（HTTP 504）
    ConnectTimeout,
    /// 响应 headers 前超时（HTTP 504）
    UpstreamTimeout,
}

impl ProxyError {
    /// 返回稳定错误码字符串
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTarget { .. } => "invalid_target",
            Self::TargetBlocked { .. } => "target_blocked",
            Self::DnsFailed { .. } => "dns_failed",
            Self::ConnectFailed { .. } => "connect_failed",
            Self::UpstreamFailed { .. } => "upstream_failed",
            Self::ConnectTimeout => "connect_timeout",
            Self::UpstreamTimeout => "upstream_timeout",
        }
    }

    /// 返回 HTTP 状态码
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidTarget { .. } => 400,
            Self::TargetBlocked { .. } => 403,
            Self::DnsFailed { .. } | Self::ConnectFailed { .. } | Self::UpstreamFailed { .. } => {
                502
            }
            Self::ConnectTimeout | Self::UpstreamTimeout => 504,
        }
    }

    /// 返回人类可读的错误消息（不含敏感信息）
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidTarget { message }
            | Self::TargetBlocked { message }
            | Self::DnsFailed { message }
            | Self::ConnectFailed { message }
            | Self::UpstreamFailed { message } => message,
            Self::ConnectTimeout => "连接超时",
            Self::UpstreamTimeout => "上游响应超时",
        }
    }
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for ProxyError {}
