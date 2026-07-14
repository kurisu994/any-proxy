//! 本地 HTTP/HTTPS fixture 端到端集成测试
//!
//! 使用本地 fixture 验证 Connector 与真实 TCP 连接的端到端路径。

#![cfg(test)]

mod fixture;

use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::{AddressPolicy, ResolveResult, Resolver};
use any_proxy::target::{Host, Scheme, Target};

use fixture::test_tls_client_config;

/// 假 Resolver：返回预设的 loopback 地址
struct LoopbackResolver;

impl Resolver for LoopbackResolver {
    async fn resolve(&self, _host: &str) -> Result<ResolveResult, any_proxy::ProxyError> {
        Ok(ResolveResult {
            addresses: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        })
    }
}

/// 允许 loopback 的测试 policy
fn test_policy() -> AddressPolicy {
    AddressPolicy::allow_all_for_test()
}

/// 测试 1: HTTP GET — 本地 HTTP fixture
///
/// Connector 建立 TCP 连接到本地 HTTP 服务器，
/// 验证 peer_addr 正确且连接可达。
#[tokio::test]
async fn test_http_get_local_fixture() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();

    let resolver = LoopbackResolver;
    let policy = test_policy();
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, policy, dialer);

    let target = Target {
        scheme: Scheme::Http,
        host: Host::Domain("localhost".to_string()),
        port,
        path: "/".into(),
        query: String::new(),
    };

    let conn = connector.connect(&target).await.expect("连接应成功");
    assert_eq!(conn.peer_addr, server.addr);

    server.shutdown();
}

/// 测试 2: HTTPS GET — 本地 HTTPS fixture
///
/// Connector 建立 TCP + TLS 连接到本地 HTTPS 服务器，
/// 验证 peer_addr 正确且 TLS 握手成功。
#[tokio::test]
async fn test_https_get_local_fixture() {
    let server = fixture::TestServer::start_https("localhost").await;
    let port = server.addr.port();
    let cert_der = server.cert_der.clone().expect("HTTPS fixture 应有证书");

    let resolver = LoopbackResolver;
    let policy = test_policy();
    let dialer = TcpDialer::new();
    let tls_config = test_tls_client_config(cert_der);
    let connector = Connector::with_tls(resolver, policy, dialer, tls_config);

    let target = Target {
        scheme: Scheme::Https,
        host: Host::Domain("localhost".to_string()),
        port,
        path: "/".into(),
        query: String::new(),
    };

    let conn = connector.connect(&target).await.expect("连接应成功");
    assert_eq!(conn.peer_addr, server.addr);

    server.shutdown();
}

/// 测试 3: 重定向链 — Connector 可用于多跳连接
///
/// 验证 Connector 可以连续建立多个连接（模拟重定向场景）。
#[tokio::test]
async fn test_redirect_chain_multiple_connects() {
    let server1 = fixture::TestServer::start_http().await;
    let server2 = fixture::TestServer::start_http().await;

    let resolver = LoopbackResolver;
    let policy = test_policy();
    let dialer = TcpDialer::new();
    let connector = Connector::new(resolver, policy, dialer);

    // 第一跳
    let target1 = Target {
        scheme: Scheme::Http,
        host: Host::Domain("first.example.com".to_string()),
        port: server1.addr.port(),
        path: "/".into(),
        query: String::new(),
    };
    let conn1 = connector.connect(&target1).await.expect("第一跳应成功");
    assert_eq!(conn1.peer_addr, server1.addr);

    // 第二跳
    let target2 = Target {
        scheme: Scheme::Http,
        host: Host::Domain("second.example.com".to_string()),
        port: server2.addr.port(),
        path: "/".into(),
        query: String::new(),
    };
    let conn2 = connector.connect(&target2).await.expect("第二跳应成功");
    assert_eq!(conn2.peer_addr, server2.addr);

    // 验证两次连接的 peer_addr 不同
    assert_ne!(conn1.peer_addr, conn2.peer_addr);

    server1.shutdown();
    server2.shutdown();
}
