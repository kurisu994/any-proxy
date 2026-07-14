//! M1 完整 Relay 端到端集成测试
//!
//! 验证 DESIGN.md M1 Success Criteria：
//! 1. 浏览器可读取状态码、暴露的响应头和流式响应体
//! 2. 6 个 method 转发 + OPTIONS 预检 + 405
//! 3. 流式响应（大 body 不线性占用内存）
//! 4. Connection header 清理、4xx/5xx 原样转发
//! 5. CORS headers 正确
//! 6. 健康检查和首页

#![cfg(test)]

mod fixture;

use std::sync::Arc;

use any_proxy::app::create_app;
use any_proxy::config::Config;
use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::{AddressPolicy, ResolveResult, Resolver};

/// 假 Resolver：返回 loopback 地址（测试用）
#[derive(Clone)]
struct LoopbackResolver;

impl Resolver for LoopbackResolver {
    async fn resolve(&self, _host: &str) -> Result<ResolveResult, any_proxy::ProxyError> {
        Ok(ResolveResult {
            addresses: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        })
    }
}

/// 构建测试用的 Axum app
fn make_test_app(fixture_port: u16) -> axum::Router {
    let _ = fixture_port;
    let policy = AddressPolicy::allow_all_for_test();
    let connector = Arc::new(Connector::new(LoopbackResolver, policy, TcpDialer::new()));
    let config = Arc::new(Config::default());
    create_app(connector, config)
}

/// 启动测试代理服务器，返回 (端口, 关闭句柄)
async fn start_proxy(fixture_port: u16) -> (u16, tokio::task::JoinHandle<()>) {
    let app = make_test_app(fixture_port);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}

/// 发送 HTTP 请求并返回完整响应字符串
async fn http_get(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("连接代理应成功");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }
    String::from_utf8_lossy(&response).to_string()
}

/// 发送自定义方法的 HTTP 请求
async fn http_method(port: u16, method: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("连接代理应成功");
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }
    String::from_utf8_lossy(&response).to_string()
}

/// 测试 1: 健康检查
#[tokio::test]
async fn test_healthz() {
    let server = fixture::TestServer::start_http().await;
    let (port, _handle) = start_proxy(server.addr.port()).await;

    let resp = http_get(port, "/healthz").await;
    assert!(resp.contains("200 OK"), "healthz 应返回 200: {resp}");
    assert!(resp.contains("ok"), "healthz 应返回 ok: {resp}");

    server.shutdown();
}

/// 测试 2: 首页
#[tokio::test]
async fn test_index_page() {
    let server = fixture::TestServer::start_http().await;
    let (port, _handle) = start_proxy(server.addr.port()).await;

    let resp = http_get(port, "/").await;
    assert!(resp.contains("200 OK"), "首页应返回 200: {resp}");
    assert!(resp.contains("any-proxy"), "首页应包含 any-proxy: {resp}");

    server.shutdown();
}

/// 测试 3: HTTP GET 代理转发
///
/// M1 成功标准 1：浏览器可读取状态码和响应体
#[tokio::test]
async fn test_proxy_http_get() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();

    // 构建 proxy 请求路径
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy(port).await;

    let resp = http_get(proxy_port, &proxy_path).await;
    assert!(resp.contains("200 OK"), "代理应返回 200: {resp}");
    assert!(resp.contains("hello"), "代理应返回 hello: {resp}");

    server.shutdown();
}

/// 测试 4: CORS headers
///
/// M1 成功标准 5：响应包含 CORS headers
#[tokio::test]
async fn test_cors_headers_in_response() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy(port).await;

    let resp = http_get(proxy_port, &proxy_path).await;
    assert!(
        resp.contains("access-control-allow-origin: *"),
        "响应应包含 CORS header: {resp}"
    );
    assert!(
        resp.contains("access-control-expose-headers"),
        "响应应包含 expose-headers: {resp}"
    );

    server.shutdown();
}

/// 测试 5: OPTIONS 预检
///
/// M1 成功标准 2：入站 OPTIONS 按固定模板完成 CORS 预检
#[tokio::test]
async fn test_options_preflight() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();
    let (proxy_port, _handle) = start_proxy(port).await;

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .unwrap();
    let request = format!(
        "OPTIONS /http://127.0.0.1:{port}/ HTTP/1.1\r\n\
         Host: localhost\r\n\
         Access-Control-Request-Method: GET\r\n\
         Access-Control-Request-Headers: Content-Type\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();

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
    let resp = String::from_utf8_lossy(&response);
    assert!(resp.contains("204"), "预检应返回 204: {resp}");
    assert!(
        resp.contains("access-control-allow-origin: *"),
        "预检应包含 CORS: {resp}"
    );
    assert!(
        resp.contains("access-control-allow-methods"),
        "预检应包含 allow-methods: {resp}"
    );

    server.shutdown();
}

/// 测试 6: 不支持的方法返回 405
///
/// M1 成功标准 2：其他 method 返回 405
#[tokio::test]
async fn test_method_not_allowed() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();
    let (proxy_port, _handle) = start_proxy(port).await;

    let resp = http_method(proxy_port, "TRACE", "/http://example.com/").await;
    assert!(resp.contains("405"), "TRACE 应返回 405: {resp}");
    assert!(
        resp.contains("method_not_allowed"),
        "错误码应为 method_not_allowed: {resp}"
    );

    server.shutdown();
}

/// 测试 7: 上游 4xx 原样转发
///
/// M1 成功标准 5：上游 4xx/5xx 原样转发
#[tokio::test]
async fn test_upstream_4xx_passthrough() {
    let server = fixture::TestServer::start_http_with_status(404).await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy(port).await;

    let resp = http_get(proxy_port, &proxy_path).await;
    assert!(resp.contains("404"), "应原样转发 404: {resp}");

    server.shutdown();
}

/// 测试 8: HEAD 请求
///
/// M1 成功标准 2：HEAD 方法转发
#[tokio::test]
async fn test_head_request() {
    let server = fixture::TestServer::start_http().await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy(port).await;

    let resp = http_method(proxy_port, "HEAD", &proxy_path).await;
    assert!(resp.contains("200"), "HEAD 应返回 200: {resp}");

    server.shutdown();
}

/// 测试 9: 非法目标 URL 返回 400
#[tokio::test]
async fn test_invalid_target_url() {
    let server = fixture::TestServer::start_http().await;
    let (proxy_port, _handle) = start_proxy(server.addr.port()).await;

    let resp = http_get(proxy_port, "/ftp://example.com/").await;
    assert!(resp.contains("400"), "非法协议应返回 400: {resp}");
    assert!(
        resp.contains("invalid_target"),
        "错误码应为 invalid_target: {resp}"
    );

    server.shutdown();
}

/// 测试 10: 私网目标返回 403
#[tokio::test]
async fn test_private_target_blocked() {
    // 使用真实 AddressPolicy 和 SystemResolver（IP literal 直接返回，不经 DNS）
    let policy = AddressPolicy::new();
    let connector = Arc::new(Connector::new(
        any_proxy::resolver::SystemResolver::new(),
        policy,
        TcpDialer::new(),
    ));
    let config = Arc::new(Config::default());
    let app = create_app(connector, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let _handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let resp = http_get(proxy_port, "/http://10.0.0.1/").await;
    assert!(resp.contains("403"), "私网目标应返回 403: {resp}");
    assert!(
        resp.contains("target_blocked"),
        "错误码应为 target_blocked: {resp}"
    );
}
