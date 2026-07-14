//! TLS spike 集成测试
//!
//! 验证 M1 的关键集成路径：
//! - Connector 能建立 TCP 连接到本地 fixture
//! - TLS SNI 使用原始 hostname（不是固定 IP），握手成功后能读取 HTTP 响应
//! - Host header 使用原始 hostname
//! - 证书校验失败时返回错误

#![cfg(test)]

mod fixture;

use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::AddressPolicy;
use any_proxy::target::{Host, Scheme, Target};

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;

/// 构建 HTTPS Target，指向本地 fixture
fn local_https_target(hostname: &str, port: u16) -> Target {
    Target {
        scheme: Scheme::Https,
        host: Host::Domain(hostname.to_string()),
        port,
        path: "/".into(),
        query: String::new(),
    }
}

/// 构建 HTTP Target，指向本地 fixture
fn local_http_target(hostname: &str, port: u16) -> Target {
    Target {
        scheme: Scheme::Http,
        host: Host::Domain(hostname.to_string()),
        port,
        path: "/".into(),
        query: String::new(),
    }
}

/// 测试 1: Connector 能建立 TCP 连接到本地 fixture
///
/// 验证 resolve -> validate -> dial 路径在本地 fixture 上可行。
/// 使用 HTTP fixture 和 HTTP target，只测试 TCP 连接。
#[tokio::test]
async fn test_connector_tcp_to_local_fixture() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();

    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, test_policy, dialer);

    let target = local_http_target("localhost", port);
    let conn = connector.connect(&target).await.expect("连接应成功");

    assert_eq!(
        conn.peer_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(conn.peer_addr.port(), port);

    server.shutdown();
}

/// 测试 2: TLS SNI 使用原始 hostname，握手成功后能读取 HTTP 响应
///
/// Connector 内部完成 TCP + TLS 握手。
/// 本地 HTTPS fixture 的证书包含 hostname，SNI 不匹配会握手失败。
#[tokio::test]
async fn test_tls_sni_uses_hostname() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();
    let cert_der = server
        .cert_der
        .clone()
        .expect("HTTPS server should have cert");

    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let tls_config = fixture::test_tls_client_config(cert_der);
    let connector = Connector::with_tls(resolver, test_policy, dialer, tls_config);

    let target = local_https_target(hostname, port);
    let conn = connector.connect(&target).await.expect("TLS 连接应成功");

    // 通过已建立的 TLS stream 发送 HTTP 请求
    let mut stream = conn.stream;
    let request = format!("GET / HTTP/1.1\r\nHost: {hostname}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
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
/// 验证 canonical_host 使用原始 hostname（不是固定 IP）。
#[tokio::test]
async fn test_host_header_uses_hostname() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();
    let cert_der = server
        .cert_der
        .clone()
        .expect("HTTPS server should have cert");

    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let tls_config = fixture::test_tls_client_config(cert_der);
    let connector = Connector::with_tls(resolver, test_policy, dialer, tls_config);

    let target = local_https_target(hostname, port);
    let conn = connector.connect(&target).await.expect("连接应成功");

    // 验证 canonical_host 使用 hostname（不是 IP）
    let canonical_host = any_proxy::target::canonical_host(&target.host);
    assert_eq!(
        canonical_host, hostname,
        "canonical_host 应使用原始 hostname"
    );

    let mut stream = conn.stream;
    let request = format!("GET / HTTP/1.1\r\nHost: {canonical_host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
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
/// 验证当 TLS 证书不被信任时，Connector 的 TLS 握手失败。
#[tokio::test]
async fn test_tls_cert_validation_failure() {
    let hostname = "localhost";
    let server = fixture::TestServer::start_https(hostname).await;
    let port = server.addr.port();

    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();

    // 使用空的 root store，不信任任何证书
    let empty_config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
    );
    let connector = Connector::with_tls(resolver, test_policy, dialer, empty_config);

    let target = local_https_target(hostname, port);
    let result = connector.connect(&target).await;
    assert!(result.is_err(), "不信任的证书应导致连接失败");

    server.shutdown();
}

/// 测试 5: SNI 不匹配导致 TLS 握手失败
///
/// 服务器证书是 "localhost"，但客户端使用 "other.example.com" 作为 SNI。
/// 即使信任该证书，SNI 不匹配也应导致握手失败。
#[tokio::test]
async fn test_tls_sni_mismatch_failure() {
    let server_hostname = "localhost";
    let server = fixture::TestServer::start_https(server_hostname).await;
    let port = server.addr.port();
    let cert_der = server
        .cert_der
        .clone()
        .expect("HTTPS server should have cert");

    let test_policy = AddressPolicy::allow_all_for_test();
    let resolver = FakeResolverOne {
        addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    let dialer = TcpDialer::new();
    let tls_config = fixture::test_tls_client_config(cert_der);
    let connector = Connector::with_tls(resolver, test_policy, dialer, tls_config);

    // 使用不匹配的 hostname 作为 SNI
    let target = local_https_target("other.example.com", port);
    let result = connector.connect(&target).await;
    assert!(result.is_err(), "SNI 不匹配应导致 TLS 握手失败");

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
