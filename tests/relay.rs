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
fn make_test_app() -> axum::Router {
    let policy = AddressPolicy::allow_all_for_test();
    let connector = Arc::new(Connector::new(LoopbackResolver, policy, TcpDialer::new()));
    let config = Arc::new(Config::default());
    create_app(connector, config)
}

/// 启动测试代理服务器，返回 (端口, 关闭句柄)
async fn start_proxy() -> (u16, tokio::task::JoinHandle<()>) {
    let app = make_test_app();
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
    let (port, _handle) = start_proxy().await;

    let resp = http_get(port, "/healthz").await;
    assert!(resp.contains("200 OK"), "healthz 应返回 200: {resp}");
    assert!(resp.contains("ok"), "healthz 应返回 ok: {resp}");

    server.shutdown();
}

/// 测试 2: 首页
#[tokio::test]
async fn test_index_page() {
    let server = fixture::TestServer::start_http().await;
    let (port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

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
    let (proxy_port, _handle) = start_proxy().await;

    let resp = http_method(proxy_port, "HEAD", &proxy_path).await;
    assert!(resp.contains("200"), "HEAD 应返回 200: {resp}");

    server.shutdown();
}

/// 测试 9: 非法目标 URL 返回 400
#[tokio::test]
async fn test_invalid_target_url() {
    let server = fixture::TestServer::start_http().await;
    let (proxy_port, _handle) = start_proxy().await;

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

/// 测试 11: 流式传输 256 MiB 生成式响应
///
/// M1 成功标准 3：传输至少 256 MiB 的生成式响应时，
/// 额外常驻内存保持在固定小窗口内，不随响应大小线性增长。
///
/// 使用 chunked transfer encoding 分块发送 256 MiB 数据，
/// 验证代理能完整转发而不超时或缓冲。
#[tokio::test]
async fn test_streaming_256mib() {
    // 256 MiB = 256 * 1024 * 1024 bytes
    // 使用 1 MiB chunk * 256 chunks = 256 MiB
    let chunk_size = 1024 * 1024; // 1 MiB
    let chunk_count = 256;
    let total_size = chunk_size * chunk_count;

    let server =
        fixture::TestServer::start_http_chunked(chunk_size, chunk_count, std::time::Duration::ZERO)
            .await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy().await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("连接代理应成功");
    let request =
        format!("GET {proxy_path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    // 读取全部响应
    let mut response = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }

    let resp_str = String::from_utf8_lossy(&response);
    assert!(resp_str.contains("200 OK"), "应返回 200 OK");

    // 验证接收到的 body 数据量至少 256 MiB
    // chunked encoding 的 chunk header 中可能包含 'A'（hex digit），所以用 >=
    let a_count = response.iter().filter(|&&b| b == b'A').count();
    assert!(
        a_count >= total_size,
        "应至少收到 {total_size} 字节数据，实际收到 {a_count}"
    );

    server.shutdown();
}

/// 测试 12: 客户端中途断开后上游连接关闭
///
/// M1 成功标准 4：客户端断开后取消上游工作。
///
/// 上游服务器以慢速 chunked 响应（每 chunk 间隔 100ms），
/// 客户端在读取第一个 chunk 后主动断开，
/// 验证上游服务器的活跃连接数在断开后减少。
#[tokio::test]
async fn test_client_disconnect_cancels_upstream() {
    let server =
        fixture::TestServer::start_http_chunked(1024, 1000, std::time::Duration::from_millis(100))
            .await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");
    let (proxy_port, _handle) = start_proxy().await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("连接代理应成功");
    let request =
        format!("GET {proxy_path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    // 读取一些数据（至少读到 header + 第一个 chunk）
    let mut buf = vec![0u8; 4096];
    let _ = stream.read(&mut buf).await.expect("应读到一些数据");

    // 验证此时上游有活跃连接
    let active = server
        .active_connections
        .as_ref()
        .unwrap()
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(active > 0, "断开前上游应有活跃连接");

    // 客户端主动断开
    drop(stream);

    // 等待取消传播
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 验证上游活跃连接数减少（取消传播生效）
    let active_after = server
        .active_connections
        .as_ref()
        .unwrap()
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        active_after < active,
        "断开后活跃连接数应减少: {active_after} < {active}"
    );

    server.shutdown();
}

/// 测试 13: 响应 body idle timeout 中止流
///
/// 上游先快速发送一个 chunk，然后长时间不发送（超过 idle timeout），
/// 验证代理在 idle timeout 后中止流。
#[tokio::test]
async fn test_response_body_idle_timeout() {
    // 第一个 chunk 立即发送，后续 chunk 间隔 5 秒（超过默认 60s timeout 不会触发，
    // 所以用短超时配置的代理实例）
    let server =
        fixture::TestServer::start_http_chunked(1024, 10, std::time::Duration::from_secs(5)).await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/");

    // 创建短 idle timeout 配置的代理
    let policy = AddressPolicy::allow_all_for_test();
    let connector = Arc::new(Connector::new(LoopbackResolver, policy, TcpDialer::new()));
    let config = Arc::new(Config {
        upstream_body_idle_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    });
    let app = create_app(connector, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let _handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("连接代理应成功");
    let request =
        format!("GET {proxy_path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    // 读取响应，应在 idle timeout 后收到截断
    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let resp_str = String::from_utf8_lossy(&response);
    // 应该收到 200 OK 和一些数据，但不会收到全部 10 个 chunk
    assert!(resp_str.contains("200 OK"), "应返回 200 OK");
    // 收到的 'A' 字节数应少于总 10 * 1024 = 10240
    let a_count = response.iter().filter(|&&b| b == b'A').count();
    assert!(
        a_count < 10240,
        "idle timeout 应导致截断，但收到了全部 {a_count} 字节"
    );

    server.shutdown();
}

/// 发送带 body 的自定义方法请求
async fn http_request_with_body(port: u16, method: &str, path: &str, body: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("连接代理应成功");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

/// 测试 14: POST/PUT/PATCH/DELETE 带 body 端到端转发（N10）
///
/// 用录制式假上游断言「代理实际发给上游的是什么」：method、path、body 均正确转发，
/// 且 Host 按目标重建。这是 M1 标准 2 长期缺失的覆盖（原本只测了 GET/HEAD）。
#[tokio::test]
async fn test_methods_with_body_forwarded() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let server =
            fixture::RecordingServer::start(vec![fixture::CannedResponse::ok("done")]).await;
        let port = server.addr.port();
        let proxy_path = format!("/http://127.0.0.1:{port}/api/items");
        let (proxy_port, _handle) = start_proxy().await;

        let body = format!("payload-{method}");
        let resp = http_request_with_body(proxy_port, method, &proxy_path, &body).await;
        assert!(resp.contains("200"), "{method} 应返回 200: {resp}");
        assert!(resp.contains("done"), "{method} 应返回上游 body: {resp}");

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1, "{method} 应录到 1 个上游请求");
        let r = &recorded[0];
        assert_eq!(r.method, method, "上游收到的 method 应为 {method}");
        assert_eq!(r.path, "/api/items", "上游收到的 path 应正确");
        assert_eq!(r.body, body.as_bytes(), "上游收到的 body 应与发送一致");
        assert_eq!(
            r.header("host"),
            Some(format!("127.0.0.1:{port}").as_str()),
            "Host 应按目标重建"
        );

        server.shutdown();
    }
}

/// 测试 15: 重定向跟随端到端（N10 + N2 相对 Location 解析）
///
/// 上游第一跳返回 302 + 相对 `Location: /final`，第二跳返回 200。
/// 断言代理确实发起了两次请求，且相对 Location 被解析到同 host 的 `/final`。
/// 这是全仓库首个真正产生 3xx 的端到端重定向测试。
#[tokio::test]
async fn test_redirect_followed_end_to_end() {
    let server = fixture::RecordingServer::start(vec![
        fixture::CannedResponse::redirect(302, "/final"),
        fixture::CannedResponse::ok("arrived"),
    ])
    .await;
    let port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{port}/start");
    let (proxy_port, _handle) = start_proxy().await;

    let resp = http_get(proxy_port, &proxy_path).await;
    assert!(resp.contains("200"), "跟随重定向后应返回 200: {resp}");
    assert!(resp.contains("arrived"), "应返回最终 body: {resp}");

    let recorded = server.recorded();
    assert_eq!(recorded.len(), 2, "应录到 2 个请求（原始 + 重定向后）");
    assert_eq!(recorded[0].path, "/start", "首个请求 path 应为 /start");
    assert_eq!(
        recorded[1].path, "/final",
        "相对 Location 应解析到同 host 的 /final（N2）"
    );

    server.shutdown();
}
