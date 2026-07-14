//! 安全连接编排：resolve → validate → dial → TLS → 记录 peer_addr
//!
//! 参见 DESIGN.md Section 4（Target validation and connection pinning）。
//!
//! # 安全模型
//!
//! `Connector` 在一次调用中完成 DNS 解析、全量地址校验、选择 `SocketAddr` 与
//! `TcpStream::connect`，并记录 `peer_addr()`。对于 HTTPS 目标，在 TCP 连接
//! 建立后进行 TLS 握手，SNI 和证书校验使用原始规范化 hostname，TCP 使用已固定的 IP。
//!
//! 测试注入假 Resolver 与假 Dialer，断言策略允许集合与实际 dial 地址完全一致。
//!
//! M1 默认关闭上游连接池，每次请求都走一次完整的 resolve → validate → dial。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::resolver::{AddressPolicy, Resolver};
use crate::target::{canonical_host, Target};

/// 包装任意 AsyncRead + AsyncWrite stream 为可 trait-object 的类型
///
/// Rust 不允许 `Box<dyn AsyncRead + AsyncWrite>`（两个非 auto trait），
/// 用此 struct 统一 TCP 和 TLS stream。
pub struct BoxStream {
    inner: Box<dyn StreamInner + Send + Unpin>,
}

trait StreamInner: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {}
impl<T> StreamInner for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {}

impl BoxStream {
    /// 从任意 stream 创建 BoxStream
    pub fn new<S>(stream: S) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            inner: Box::new(stream),
        }
    }
}

impl tokio::io::AsyncRead for BoxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for BoxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

/// TCP 连接 trait（可注入测试替身）
///
/// 实现者连接到指定的 `SocketAddr`，返回连接记录（含 stream）。
#[trait_variant::make(Send)]
pub trait Dialer {
    /// 连接到指定地址，返回包含 stream 的连接记录
    fn dial(
        &self,
        addr: SocketAddr,
    ) -> impl std::future::Future<Output = Result<DialRecord, crate::ProxyError>> + Send;
}

/// dial 记录：记录实际连接的 peer 地址和 stream
pub struct DialRecord {
    /// 实际连接的 peer 地址
    pub peer_addr: SocketAddr,
    /// 底层 TCP stream
    pub stream: BoxStream,
}

impl std::fmt::Debug for DialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialRecord")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

/// 安全连接编排器
///
/// 持有 `Resolver`、`AddressPolicy`、`Dialer` 和可选的 `TlsConfig`。
/// 在一次调用中原子完成 resolve → validate → dial → (TLS)。
#[derive(Clone)]
pub struct Connector<R, D> {
    resolver: Arc<R>,
    policy: Arc<AddressPolicy>,
    dialer: Arc<D>,
    /// TLS 配置（HTTPS 目标使用），None 时不支持 HTTPS
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl<R, D> Connector<R, D>
where
    R: Resolver,
    D: Dialer,
{
    /// 创建新的 Connector（无 TLS 支持，仅供测试）
    pub fn new(resolver: R, policy: AddressPolicy, dialer: D) -> Self {
        Self {
            resolver: Arc::new(resolver),
            policy: Arc::new(policy),
            dialer: Arc::new(dialer),
            tls_config: None,
        }
    }

    /// 创建带 TLS 支持的 Connector
    pub fn with_tls(
        resolver: R,
        policy: AddressPolicy,
        dialer: D,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Self {
        Self {
            resolver: Arc::new(resolver),
            policy: Arc::new(policy),
            dialer: Arc::new(dialer),
            tls_config: Some(tls_config),
        }
    }

    /// 安全连接：resolve → validate → select → dial → (TLS) → 记录 peer_addr
    ///
    /// 任何策略失败都在建立上游连接前返回错误（零次 dial）。
    /// 允许目标的实际 dial peer 必须属于同一次 Resolver 返回且完成全量校验的地址集合。
    /// HTTPS 目标在 TCP 连接建立后进行 TLS 握手，SNI 使用原始规范化 hostname。
    pub async fn connect(&self, target: &Target) -> Result<Connection, crate::ProxyError> {
        // 1. 解析目标主机名
        let resolve_result = self.resolver.resolve(&target.host.as_str()).await?;

        // 2. 全量校验所有地址
        let validated = self.policy.validate_all(&resolve_result.addresses)?;

        // 3. 选择 SocketAddr（使用 target 端口）
        let addrs: Vec<SocketAddr> = validated
            .iter()
            .map(|ip| SocketAddr::new(*ip, target.port))
            .collect();

        if addrs.is_empty() {
            return Err(crate::ProxyError::DnsFailed {
                message: "DNS 返回空答案".into(),
            });
        }

        // 4. dial 到第一个验证过的地址
        let dial_record = self.dialer.dial(addrs[0]).await?;

        // 5. 验证 peer_addr 属于已验证集合
        let validated_set: HashSet<SocketAddr> = addrs.iter().copied().collect();
        if !validated_set.contains(&dial_record.peer_addr) {
            return Err(crate::ProxyError::ConnectFailed {
                message: format!("peer_addr {} 不属于已验证地址集合", dial_record.peer_addr),
            });
        }

        // 6. HTTPS 目标进行 TLS 握手
        let stream =
            if target.scheme.is_tls() {
                let tls_config =
                    self.tls_config
                        .as_ref()
                        .ok_or_else(|| crate::ProxyError::ConnectFailed {
                            message: "HTTPS 目标需要 TLS 配置".into(),
                        })?;

                let hostname = canonical_host(&target.host);
                let server_name = rustls::pki_types::ServerName::try_from(hostname.clone())
                    .map_err(|e| crate::ProxyError::ConnectFailed {
                        message: format!("SNI 解析失败: {e}"),
                    })?;

                let tls_connector = tokio_rustls::TlsConnector::from(tls_config.clone());
                let tls_stream = tls_connector
                    .connect(server_name, dial_record.stream)
                    .await
                    .map_err(|e| crate::ProxyError::ConnectFailed {
                        message: format!("TLS 握手失败: {e}"),
                    })?;

                BoxStream::new(tls_stream)
            } else {
                dial_record.stream
            };

        Ok(Connection {
            peer_addr: dial_record.peer_addr,
            target: target.clone(),
            stream,
        })
    }
}

/// 建立的连接
pub struct Connection {
    /// 实际连接的 peer 地址
    pub peer_addr: SocketAddr,
    /// 目标信息
    pub target: Target,
    /// 底层 stream（TCP 或 TLS）
    pub stream: BoxStream,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("peer_addr", &self.peer_addr)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// 真实 TCP Dialer
///
/// 使用 `tokio::net::TcpStream::connect` 连接到固定 `SocketAddr`，
/// 返回 TCP stream 和 peer_addr 记录。
#[derive(Clone)]
pub struct TcpDialer;

impl TcpDialer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TcpDialer {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialer for TcpDialer {
    async fn dial(&self, addr: SocketAddr) -> Result<DialRecord, crate::ProxyError> {
        let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
            crate::ProxyError::ConnectFailed {
                message: format!("TCP 连接失败: {e}"),
            }
        })?;

        let peer_addr = stream
            .peer_addr()
            .map_err(|e| crate::ProxyError::ConnectFailed {
                message: format!("获取 peer_addr 失败: {e}"),
            })?;

        Ok(DialRecord {
            peer_addr,
            stream: BoxStream::new(stream),
        })
    }
}

/// 创建生产用 TLS 配置
///
/// 加载系统根证书，使用 ring 加密后端，不跳过证书校验。
pub fn create_tls_config() -> Arc<rustls::ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();

    let certs = rustls_native_certs::load_native_certs().unwrap_or_default();
    for cert in certs {
        root_store.add(cert).ok();
    }
    if root_store.is_empty() {
        tracing::warn!("系统根证书加载为空，HTTPS 证书校验可能失败");
    }

    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolveResult;
    use std::net::IpAddr;

    /// 假 Resolver：返回预设的 IP 地址
    struct FakeResolver {
        addresses: Vec<IpAddr>,
        fail: bool,
    }

    impl FakeResolver {
        fn new(addresses: Vec<IpAddr>) -> Self {
            Self {
                addresses,
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                addresses: vec![],
                fail: true,
            }
        }
    }

    impl Resolver for FakeResolver {
        async fn resolve(&self, _host: &str) -> Result<ResolveResult, crate::ProxyError> {
            if self.fail {
                return Err(crate::ProxyError::DnsFailed {
                    message: "fake resolver failed".into(),
                });
            }
            Ok(ResolveResult {
                addresses: self.addresses.clone(),
            })
        }
    }

    /// 假 Dialer：记录 dial 地址，返回 duplex stream
    #[derive(Clone)]
    struct FakeDialer {
        records: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    }

    impl FakeDialer {
        fn new() -> Self {
            Self {
                records: Arc::new(std::sync::Mutex::new(vec![])),
            }
        }

        fn dial_records(&self) -> Vec<SocketAddr> {
            self.records.lock().unwrap().clone()
        }
    }

    impl Dialer for FakeDialer {
        async fn dial(&self, addr: SocketAddr) -> Result<DialRecord, crate::ProxyError> {
            self.records.lock().unwrap().push(addr);
            // 使用 duplex 创建假 stream（测试不读写 stream 内容）
            let (client, _server) = tokio::io::duplex(64);
            Ok(DialRecord {
                peer_addr: addr,
                stream: BoxStream::new(client),
            })
        }
    }

    fn make_target(port: u16) -> Target {
        Target {
            scheme: crate::target::Scheme::Https,
            host: crate::target::Host::Domain("example.com".into()),
            port,
            path: "/".into(),
            query: String::new(),
        }
    }

    fn make_http_target(port: u16) -> Target {
        Target {
            scheme: crate::target::Scheme::Http,
            host: crate::target::Host::Domain("example.com".into()),
            port,
            path: "/".into(),
            query: String::new(),
        }
    }

    #[tokio::test]
    async fn test_normal_connect() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let conn = connector.connect(&make_http_target(80)).await.unwrap();
        assert_eq!(conn.peer_addr, "1.2.3.4:80".parse().unwrap());
        assert_eq!(dialer.dial_records(), vec!["1.2.3.4:80".parse().unwrap()]);
    }

    #[tokio::test]
    async fn test_dangerous_target_zero_dial() {
        let resolver = FakeResolver::new(vec!["10.0.0.1".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_http_target(80)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty()); // 零次 dial
    }

    #[tokio::test]
    async fn test_mixed_a_aaaa_rejects_all() {
        let resolver = FakeResolver::new(vec![
            "1.2.3.4".parse().unwrap(),  // 公网
            "10.0.0.1".parse().unwrap(), // 私网
        ]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_http_target(80)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty()); // 零次 dial
    }

    #[tokio::test]
    async fn test_dns_failure() {
        let resolver = FakeResolver::failing();
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_http_target(80)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty());
    }

    #[tokio::test]
    async fn test_dns_empty_answer() {
        let resolver = FakeResolver::new(vec![]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_http_target(80)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty());
    }

    #[tokio::test]
    async fn test_concurrent_connects_no_cross_mapping() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Arc::new(Connector::new(resolver, policy, dialer.clone()));

        let t1 = make_http_target(80);
        let t2 = make_http_target(8080);
        let t3 = make_http_target(3000);
        let (r1, r2, r3) = tokio::join!(
            connector.connect(&t1),
            connector.connect(&t2),
            connector.connect(&t3),
        );

        assert_eq!(r1.unwrap().peer_addr.port(), 80);
        assert_eq!(r2.unwrap().peer_addr.port(), 8080);
        assert_eq!(r3.unwrap().peer_addr.port(), 3000);

        let records = dialer.dial_records();
        assert_eq!(records.len(), 3);
        for port in [80, 8080, 3000] {
            let expected: SocketAddr = format!("1.2.3.4:{}", port).parse().unwrap();
            assert!(records.contains(&expected));
        }
    }

    #[tokio::test]
    async fn test_peer_addr_in_validated_set() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer);

        let conn = connector.connect(&make_http_target(8080)).await.unwrap();
        assert_eq!(conn.peer_addr.port(), 8080);
    }

    #[tokio::test]
    async fn test_https_without_tls_config_fails() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        // Connector::new 不设置 tls_config
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_target(443)).await;
        assert!(result.is_err());
        // dial 已发生（TCP 连接成功），但 TLS 步骤失败
        assert!(!dialer.dial_records().is_empty());
    }
}
