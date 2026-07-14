//! TLS spike 集成测试
//!
//! 验证 M0 的关键集成路径（T1）：
//! - Connector 能建立 TCP 连接到本地 HTTPS fixture
//! - TLS SNI 使用原始 hostname（不是固定 IP）
//! - TLS 握手成功后能读取 HTTP 响应
//! - 证书校验失败时返回错误

#![cfg(test)]

mod fixture;

use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::AddressPolicy;
use any_proxy::target::{Host, Scheme, Target};

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tokio_rustls::TlsConnector;

/// 构建 Target，指向本地 fixture
fn local_target(hostname: &str, port: u16) -> Target {
    Target {
        scheme: Scheme::Https,
        host: Host::Domain(hostname.to_string()),
        port,
        query: String::new(),
    }
}

/// 测试 1: Connector 能建立 TCP 连接到本地 fixture
///
/// 验证 resolve → validate → dial 路径在本地 fixture 上可行。
/// 注意：127.0.0.1 被 AddressPolicy 拒绝（环回地址），
/// 所以测试使用自定义 policy 允许 loopback。
#[tokio::test]
async fn test_connector_tcp_to_local_fixture() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();

    // 构建允许 loopback 的 policy（仅用于测试）
    let test_policy = AddressPolicy::allow_all_for_test();

    // 构建注入假 Resolver（直接返回 127.0.0.1）
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };

    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, test_policy, dialer);

    let target = local_target("localhost", port);
    let conn = connector.connect(&target).await.expect("连接应成功");

    assert_eq!(
        conn.peer_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(conn.peer_addr.port(), port);

    server.shutdown();
}

/// 测试 2: TLS SNI 使用原始 hostname
///
/// 验证 TLS 握手时 SNI 使用 hostname（"localhost"）而不是 IP。
/// 本地 HTTPS fixture 的证书包含 hostname，如果 SNI 不匹配会握手失败。
#[tokio::test]
async fn test_tls_sni_uses_hostname() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();

    // 1. Connector 建立 TCP 连接
    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, test_policy, dialer);

    let target = local_target(hostname, port);
    let conn = connector.connect(&target).await.expect("TCP 连接应成功");

    // 2. 在 TCP 连接之上进行 TLS 握手
    //    重新建立 TCP 连接（TcpDialer 丢弃了 stream）
    let tcp_stream = tokio::net::TcpStream::connect(conn.peer_addr)
        .await
        .expect("TCP 重连应成功");

    // 3. 构建 TLS connector，使用原始 hostname 作为 SNI
    let cert_der = server
        .cert_der
        .clone()
        .expect("HTTPS server should have cert");
    let tls_config = fixture::test_tls_client_config(cert_der);
    let tls_connector = TlsConnector::from(tls_config);

    // 使用原始 hostname 作为 SNI（不是 IP）
    let domain =
        rustls::pki_types::ServerName::try_from(hostname).expect("hostname 应可解析为 ServerName");

    let mut tls_stream = tls_connector
        .connect(domain, tcp_stream)
        .await
        .expect("TLS 握手应成功");

    // 4. 发送 HTTP 请求并读取响应
    let request = format!("GET / HTTP/1.1\r\nHost: {hostname}\r\nConnection: close\r\n\r\n");
    tls_stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match tls_stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }
    let response_str = String::from_utf8_lossy(&response);
    assert!(response_str.contains("200 OK"), "应返回 200 OK");
    assert!(response_str.contains("hello"), "应包含 hello");

    server.shutdown();
}

/// 测试 3: Host header 使用原始 hostname
///
/// 验证 HTTP 请求中 Host header 使用原始 hostname。
#[tokio::test]
async fn test_host_header_uses_hostname() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();

    // Connector + TCP 连接
    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, test_policy, dialer);

    let target = local_target(hostname, port);
    let conn = connector.connect(&target).await.expect("连接应成功");

    // TLS 握手
    let tcp_stream = tokio::net::TcpStream::connect(conn.peer_addr)
        .await
        .expect("TCP 重连应成功");

    let cert_der = server
        .cert_der
        .clone()
        .expect("HTTPS server should have cert");
    let tls_config = fixture::test_tls_client_config(cert_der);
    let tls_connector = TlsConnector::from(tls_config);
    let domain = rustls::pki_types::ServerName::try_from(hostname).unwrap();
    let mut tls_stream = tls_connector
        .connect(domain, tcp_stream)
        .await
        .expect("TLS 握手应成功");

    // 验证 Host header 使用 hostname（不是 IP）
    let canonical_host = any_proxy::target::canonical_host(&target.host);
    assert_eq!(
        canonical_host, hostname,
        "canonical_host 应使用原始 hostname"
    );

    let request = format!("GET / HTTP/1.1\r\nHost: {canonical_host}\r\nConnection: close\r\n\r\n");
    tls_stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match tls_stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }
    let response_str = String::from_utf8_lossy(&response);
    assert!(response_str.contains("200 OK"));

    server.shutdown();
}

/// 测试 4: 证书校验失败
///
/// 验证当 TLS 证书不被信任时，握手失败。
#[tokio::test]
async fn test_tls_cert_validation_failure() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();

    // Connector + TCP 连接
    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, test_policy, dialer);

    let target = local_target(hostname, port);
    let conn = connector.connect(&target).await.expect("连接应成功");

    // 使用空的 root store 进行 TLS 握手，不信任自签名证书
    let tcp_stream = tokio::net::TcpStream::connect(conn.peer_addr)
        .await
        .expect("TCP 重连应成功");

    let default_config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let default_connector = TlsConnector::from(Arc::new(default_config));
    let domain = rustls::pki_types::ServerName::try_from(hostname).unwrap();

    let result = default_connector.connect(domain, tcp_stream).await;
    assert!(result.is_err(), "不信任的证书应导致 TLS 握手失败");

    // 清理：用 dangerous verifier 完成握手以正常关闭
    let cleanup_stream = tokio::net::TcpStream::connect(conn.peer_addr)
        .await
        .expect("TCP 清理连接应成功");
    let dangerous_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();
    let dangerous_connector = TlsConnector::from(Arc::new(dangerous_config));
    let cleanup_domain = rustls::pki_types::ServerName::try_from(hostname).unwrap();
    let _ = dangerous_connector
        .connect(cleanup_domain, cleanup_stream)
        .await;

    server.shutdown();
}

// === 测试辅助 ===

/// 假 Resolver：总是返回同一个 IP
struct FakeResolverOne {
    addr: std::net::IpAddr,
}

impl any_proxy::resolver::Resolver for FakeResolverOne {
    async fn resolve(
        &self,
        _host: &str,
    ) -> Result<any_proxy::resolver::ResolveResult, any_proxy::ProxyError> {
        Ok(any_proxy::resolver::ResolveResult {
            addresses: vec![self.addr],
        })
    }
}

/// 危险的证书验证器：接受所有证书（仅用于测试）
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}
