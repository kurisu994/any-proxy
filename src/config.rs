//! 运行时配置：环境变量解析、校验与 typed Config
//!
//! 参见 DESIGN.md Section 6（Streaming, pooling and lifecycle）。
//!
//! 所有环境变量可调但必须为正数，并设置文档化上限。

use std::net::SocketAddr;
use std::time::Duration;

/// 运行时配置
///
/// 从环境变量解析，所有值有默认值和校验边界。
#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址
    pub listen_addr: SocketAddr,
    /// 最大并发请求数
    pub max_concurrent_requests: usize,
    /// URI 字节上限（进入目标解析前检查）
    pub max_uri_bytes: usize,
    /// 目标 host allowlist（空=不限）；支持精确 `api.x.com` 与后缀 `.x.com`
    pub allow_targets: Vec<String>,
    /// Origin allowlist（空=`*`）；命中回显该 origin，否则拒绝
    pub allow_origins: Vec<String>,
    /// 共享代理令牌（None=不要求）；走 `X-Proxy-Token` header
    pub auth_token: Option<String>,
    /// 目标端口 allowlist（空=不限，1-65535）
    pub allow_ports: Vec<u16>,
    /// 公网监听确认开关：非 loopback 监听且无任何防护时需显式开启
    pub public_mode: bool,
    /// DNS 解析超时
    pub dns_timeout: Duration,
    /// TCP 连接超时
    pub connect_timeout: Duration,
    /// TLS 握手超时
    pub tls_timeout: Duration,
    /// 上游响应 headers 等待超时
    pub upstream_headers_timeout: Duration,
    /// 上传空闲超时（相邻 frame 间隔）
    pub upload_idle_timeout: Duration,
    /// 上游响应 body 空闲读取超时
    pub upstream_body_idle_timeout: Duration,
    /// 优雅关闭等待时间
    pub shutdown_grace: Duration,
    /// 宿主接口地址刷新间隔
    pub host_refresh_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8080".parse().unwrap(),
            max_concurrent_requests: 256,
            max_uri_bytes: 16384,
            allow_targets: Vec::new(),
            allow_origins: Vec::new(),
            auth_token: None,
            allow_ports: Vec::new(),
            public_mode: false,
            dns_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            tls_timeout: Duration::from_secs(10),
            upstream_headers_timeout: Duration::from_secs(30),
            upload_idle_timeout: Duration::from_secs(30),
            upstream_body_idle_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(30),
            host_refresh_interval: Duration::from_secs(60),
        }
    }
}

/// 配置解析错误
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub env: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.env, self.message)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// 从环境变量解析配置
    ///
    /// 缺失的变量使用默认值；存在但非法的变量返回错误。
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(v) = env_str("LISTEN_ADDR") {
            config.listen_addr = v
                .parse()
                .map_err(|e| ConfigError::new("LISTEN_ADDR", format!("无效地址: {e}")))?;
        }

        config.max_concurrent_requests = env_usize(
            "MAX_CONCURRENT_REQUESTS",
            config.max_concurrent_requests,
            1,
            1_000_000,
        )?;

        config.max_uri_bytes = env_usize("MAX_URI_BYTES", config.max_uri_bytes, 256, 1_048_576)?;

        // C1 安全带：以下全部默认关（空/false），仅在显式配置时生效
        config.allow_targets = env_list("ALLOW_TARGETS");
        config.allow_origins = env_list("ALLOW_ORIGINS");
        config.auth_token = env_str("AUTH_TOKEN");
        config.allow_ports = env_ports("ALLOW_PORTS")?;
        config.public_mode = env_bool("PUBLIC_MODE");

        config.dns_timeout = env_duration("DNS_TIMEOUT", config.dns_timeout, 1, 300)?;
        config.connect_timeout = env_duration("CONNECT_TIMEOUT", config.connect_timeout, 1, 300)?;
        config.tls_timeout = env_duration("TLS_TIMEOUT", config.tls_timeout, 1, 300)?;
        config.upstream_headers_timeout = env_duration(
            "UPSTREAM_HEADERS_TIMEOUT",
            config.upstream_headers_timeout,
            1,
            600,
        )?;
        config.upload_idle_timeout =
            env_duration("UPLOAD_IDLE_TIMEOUT", config.upload_idle_timeout, 1, 600)?;
        config.upstream_body_idle_timeout = env_duration(
            "UPSTREAM_BODY_IDLE_TIMEOUT",
            config.upstream_body_idle_timeout,
            1,
            3600,
        )?;
        config.shutdown_grace = env_duration("SHUTDOWN_GRACE", config.shutdown_grace, 1, 300)?;
        config.host_refresh_interval = env_duration(
            "HOST_REFRESH_INTERVAL",
            config.host_refresh_interval,
            1,
            3600,
        )?;

        Ok(config)
    }
}

impl Config {
    /// 目标 host 是否被 allowlist 允许（空列表=不限）
    ///
    /// 支持精确匹配 `api.x.com` 与后缀匹配 `.x.com`（匹配任意子域）。
    pub fn target_allowed(&self, host: &str) -> bool {
        if self.allow_targets.is_empty() {
            return true;
        }
        let host = host.to_lowercase();
        self.allow_targets.iter().any(|entry| {
            if let Some(suffix) = entry.strip_prefix('.') {
                // `.x.com` 匹配 `a.x.com`，也匹配裸 `x.com`
                host == suffix || host.ends_with(entry)
            } else {
                host == *entry
            }
        })
    }

    /// 目标端口是否被 allowlist 允许（空列表=不限）
    pub fn port_allowed(&self, port: u16) -> bool {
        self.allow_ports.is_empty() || self.allow_ports.contains(&port)
    }

    /// Origin 是否被 allowlist 允许（空列表=允许所有）
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allow_origins.is_empty() || self.allow_origins.contains(&origin.to_lowercase())
    }
}

impl ConfigError {
    fn new(env: &str, message: String) -> Self {
        Self {
            env: env.to_string(),
            message,
        }
    }
}

/// 读取环境变量字符串
fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// 读取逗号分隔列表，去空白、转小写、丢弃空项（用于 host / origin allowlist）
fn env_list(key: &str) -> Vec<String> {
    match env_str(key) {
        Some(s) => s
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// 读取逗号分隔端口列表，非法端口返回错误（fail-closed）
fn env_ports(key: &str) -> Result<Vec<u16>, ConfigError> {
    match env_str(key) {
        Some(s) => {
            let mut ports = Vec::new();
            for tok in s.split(',') {
                let t = tok.trim();
                if t.is_empty() {
                    continue;
                }
                let p: u16 = t
                    .parse()
                    .map_err(|e| ConfigError::new(key, format!("无效端口 `{t}`: {e}")))?;
                if p == 0 {
                    return Err(ConfigError::new(key, "端口不能为 0".into()));
                }
                ports.push(p);
            }
            Ok(ports)
        }
        None => Ok(Vec::new()),
    }
}

/// 读取布尔环境变量（`1`/`true`/`yes`/`on` 视为真，大小写不敏感）
fn env_bool(key: &str) -> bool {
    match env_str(key) {
        Some(s) => matches!(s.to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => false,
    }
}

/// 解析 usize 环境变量，带上下界校验
fn env_usize(key: &str, default: usize, min: usize, max: usize) -> Result<usize, ConfigError> {
    match env_str(key) {
        Some(s) => {
            let v: usize = s
                .parse()
                .map_err(|e| ConfigError::new(key, format!("无效整数: {e}")))?;
            if v < min {
                return Err(ConfigError::new(key, format!("值 {v} 小于最小值 {min}")));
            }
            if v > max {
                return Err(ConfigError::new(key, format!("值 {v} 超过最大值 {max}")));
            }
            Ok(v)
        }
        None => Ok(default),
    }
}

/// 解析 Duration 环境变量（秒），带上下界校验
///
/// 接受纯数字（秒）或带 `s` 后缀的格式，如 `30` 或 `30s`。
fn env_duration(
    key: &str,
    default: Duration,
    min_secs: u64,
    max_secs: u64,
) -> Result<Duration, ConfigError> {
    match env_str(key) {
        Some(s) => {
            let trimmed = s.trim_end_matches('s');
            let secs: u64 = trimmed
                .parse()
                .map_err(|e| ConfigError::new(key, format!("无效时长: {e}")))?;
            if secs < min_secs {
                return Err(ConfigError::new(
                    key,
                    format!("值 {secs}s 小于最小值 {min_secs}s"),
                ));
            }
            if secs > max_secs {
                return Err(ConfigError::new(
                    key,
                    format!("值 {secs}s 超过最大值 {max_secs}s"),
                ));
            }
            Ok(Duration::from_secs(secs))
        }
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量测试串行化锁（防止并行测试间 env 串扰）
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.max_concurrent_requests, 256);
        assert_eq!(config.dns_timeout, Duration::from_secs(5));
        assert_eq!(config.shutdown_grace, Duration::from_secs(30));
    }

    #[test]
    fn test_from_env_defaults() {
        let _lock = env_lock();
        // 清理可能残留的变量
        std::env::remove_var("MAX_CONCURRENT_REQUESTS");
        std::env::remove_var("CONNECT_TIMEOUT");
        let config = Config::from_env().unwrap();
        assert_eq!(config.max_concurrent_requests, 256);
    }

    #[test]
    fn test_parse_duration_with_s_suffix() {
        let _lock = env_lock();
        std::env::set_var("TEST_DUR", "15s");
        let d = env_duration("TEST_DUR", Duration::from_secs(5), 1, 300).unwrap();
        std::env::remove_var("TEST_DUR");
        assert_eq!(d, Duration::from_secs(15));
    }

    #[test]
    fn test_parse_duration_without_suffix() {
        let _lock = env_lock();
        std::env::set_var("TEST_DUR", "20");
        let d = env_duration("TEST_DUR", Duration::from_secs(5), 1, 300).unwrap();
        std::env::remove_var("TEST_DUR");
        assert_eq!(d, Duration::from_secs(20));
    }

    #[test]
    fn test_parse_duration_below_min() {
        let _lock = env_lock();
        std::env::set_var("TEST_DUR", "0");
        let result = env_duration("TEST_DUR", Duration::from_secs(5), 1, 300);
        std::env::remove_var("TEST_DUR");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_duration_above_max() {
        let _lock = env_lock();
        std::env::set_var("TEST_DUR", "999");
        let result = env_duration("TEST_DUR", Duration::from_secs(5), 1, 300);
        std::env::remove_var("TEST_DUR");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_usize_below_min() {
        let _lock = env_lock();
        std::env::set_var("TEST_USIZE", "0");
        let result = env_usize("TEST_USIZE", 100, 1, 1000);
        std::env::remove_var("TEST_USIZE");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_usize_above_max() {
        let _lock = env_lock();
        std::env::set_var("TEST_USIZE", "99999");
        let result = env_usize("TEST_USIZE", 100, 1, 1000);
        std::env::remove_var("TEST_USIZE");
        assert!(result.is_err());
    }

    #[test]
    fn test_from_env_custom_values() {
        let _lock = env_lock();
        std::env::set_var("MAX_CONCURRENT_REQUESTS", "512");
        std::env::set_var("CONNECT_TIMEOUT", "15s");
        let config = Config::from_env().unwrap();
        std::env::remove_var("MAX_CONCURRENT_REQUESTS");
        std::env::remove_var("CONNECT_TIMEOUT");
        assert_eq!(config.max_concurrent_requests, 512);
        assert_eq!(config.connect_timeout, Duration::from_secs(15));
    }

    #[test]
    fn test_target_allowed() {
        let mut c = Config::default();
        assert!(c.target_allowed("anything.com"), "空列表应不限");
        c.allow_targets = vec!["api.example.com".into(), ".trusted.com".into()];
        assert!(c.target_allowed("api.example.com"), "精确匹配");
        assert!(!c.target_allowed("evil.com"));
        assert!(c.target_allowed("a.trusted.com"), "后缀匹配子域");
        assert!(c.target_allowed("trusted.com"), "后缀匹配裸域");
        assert!(!c.target_allowed("nottrusted.com"), "不应误匹配");
        assert!(c.target_allowed("API.EXAMPLE.COM"), "大小写不敏感");
    }

    #[test]
    fn test_port_allowed() {
        let mut c = Config::default();
        assert!(c.port_allowed(12345), "空列表应不限");
        c.allow_ports = vec![80, 443];
        assert!(c.port_allowed(443));
        assert!(!c.port_allowed(8080));
    }

    #[test]
    fn test_origin_allowed() {
        let mut c = Config::default();
        assert!(c.origin_allowed("https://any.com"), "空列表应允许");
        c.allow_origins = vec!["https://app.example.com".into()];
        assert!(c.origin_allowed("https://app.example.com"));
        assert!(!c.origin_allowed("https://evil.com"));
    }

    #[test]
    fn test_env_ports_invalid_fails() {
        let _lock = env_lock();
        std::env::set_var("ALLOW_PORTS", "80,not-a-port");
        let result = Config::from_env();
        std::env::remove_var("ALLOW_PORTS");
        assert!(result.is_err(), "非法端口应 fail-closed");
    }

    #[test]
    fn test_env_c1_defaults_empty() {
        let _lock = env_lock();
        for k in [
            "ALLOW_TARGETS",
            "ALLOW_ORIGINS",
            "AUTH_TOKEN",
            "ALLOW_PORTS",
            "PUBLIC_MODE",
        ] {
            std::env::remove_var(k);
        }
        let c = Config::from_env().unwrap();
        assert!(c.allow_targets.is_empty());
        assert!(c.allow_origins.is_empty());
        assert!(c.auth_token.is_none());
        assert!(c.allow_ports.is_empty());
        assert!(!c.public_mode, "默认零配置：所有安全带关闭，向后兼容");
    }
}
