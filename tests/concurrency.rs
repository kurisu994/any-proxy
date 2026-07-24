//! 并发上限与流式传输生命周期的回归测试
//!
//! 覆盖 2026-07-24 全量审查发现的两个缺陷：
//!
//! - **M1**：Tower `ConcurrencyLimitLayer` 在 handler 返回 Response 时就释放 permit，
//!   流式 Body 的生命周期完全不受 `MAX_CONCURRENT_REQUESTS` 约束，
//!   调用方可堆积远超上限的活跃流与上游连接任务。
//! - **N1**：`POOL_IDLE_TIMEOUT` 被用作整个连接 future 的总时长上限，
//!   持续有数据流动的长传输也会在到期时被静默截断。

#![cfg(test)]

mod fixture;

use std::sync::Arc;

use any_proxy::app::create_app;
use any_proxy::config::Config;
use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::{AddressPolicy, ResolveResult, Resolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
struct LoopbackResolver;

impl Resolver for LoopbackResolver {
    async fn resolve(&self, _host: &str) -> Result<ResolveResult, any_proxy::ProxyError> {
        Ok(ResolveResult {
            addresses: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        })
    }
}

/// 启动带指定配置的测试代理，返回监听端口
async fn start_proxy_with(config: Config) -> u16 {
    let policy = AddressPolicy::allow_all_for_test();
    let connector = Arc::new(Connector::new(LoopbackResolver, policy, TcpDialer::new()));
    let app = create_app(connector, Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// 打开一个请求并只读到响应头，保持连接不关闭
///
/// 返回仍然持有的 stream；drop 它等于客户端断开。
async fn open_and_read_headers(port: u16, path: &str) -> (tokio::net::TcpStream, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("连接代理应成功");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    // 读到响应头结束为止
    let mut acc = Vec::new();
    let mut buf = vec![0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&buf[..n]);
        if acc.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    (stream, String::from_utf8_lossy(&acc).to_string())
}

/// 完整发一个请求并读到连接关闭
async fn full_request(port: u16, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("连接代理应成功");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp).await;
    String::from_utf8_lossy(&resp).to_string()
}

/// 统计响应 **body** 中的 payload 字节数
///
/// 必须跳过响应头再数：CORS 的 `Access-Control-*` 与 `Accept-Ranges`
/// 本身含有大写 `A`，直接在整个响应上计数会多算 3 个。
fn count_payload_bytes(resp: &str, byte: char) -> usize {
    match resp.split_once("\r\n\r\n") {
        Some((_headers, body)) => body.chars().filter(|&c| c == byte).count(),
        None => 0,
    }
}

/// M1 回归：permit 必须覆盖响应流的生命周期
///
/// 上限设为 1，先开一个「响应头已到、body 还在流」的请求并挂住不读完，
/// 此时第二个请求必须被拒绝。
///
/// 修复前：第一个请求的 permit 在 handler 返回时就还回去了，
/// 第二个请求会正常拿到 200 —— 并发上限对活跃下载完全不起作用。
#[tokio::test]
async fn test_permit_covers_streaming_body() {
    // 慢速 chunked 上游：每 200ms 一个 chunk，共 50 个，足够长时间保持流打开
    let server =
        fixture::TestServer::start_http_chunked(1024, 50, std::time::Duration::from_millis(200))
            .await;
    let upstream_port = server.addr.port();
    let proxy_path = format!("/http://127.0.0.1:{upstream_port}/");

    let proxy_port = start_proxy_with(Config {
        max_concurrent_requests: 1,
        ..Default::default()
    })
    .await;

    // 第一个请求：读到响应头后挂住，body 仍在流动
    let (held_stream, first_head) = open_and_read_headers(proxy_port, &proxy_path).await;
    assert!(
        first_head.contains("200"),
        "第一个请求应拿到 200：{first_head}"
    );

    // 第二个请求：此刻 permit 仍被第一个流持有，必须被拒绝
    let second = full_request(proxy_port, &proxy_path).await;
    assert!(
        second.contains("503") && second.contains("service_overloaded"),
        "并发上限=1 且第一个流仍活跃时，第二个请求必须返回 503 service_overloaded，实际：{second}"
    );

    // 释放第一个流后，permit 应归还，新请求可以通过
    drop(held_stream);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let third = full_request(proxy_port, &proxy_path).await;
    assert!(
        third.contains("200"),
        "第一个流结束后 permit 应归还，第三个请求应成功：{third}"
    );

    server.shutdown();
}

/// M1 回归：503 响应必须带 CORS headers
///
/// 代理生成的所有错误响应都要能被浏览器读取，过载响应不能例外。
#[tokio::test]
async fn test_overload_response_has_cors() {
    let server =
        fixture::TestServer::start_http_chunked(1024, 50, std::time::Duration::from_millis(200))
            .await;
    let proxy_path = format!("/http://127.0.0.1:{}/", server.addr.port());

    let proxy_port = start_proxy_with(Config {
        max_concurrent_requests: 1,
        ..Default::default()
    })
    .await;

    let (_held, _) = open_and_read_headers(proxy_port, &proxy_path).await;
    let rejected = full_request(proxy_port, &proxy_path).await;

    assert!(rejected.contains("503"), "应为 503：{rejected}");
    assert!(
        rejected.contains("access-control-allow-origin: *"),
        "503 响应必须带 CORS header：{rejected}"
    );

    server.shutdown();
}

/// M1 回归：/healthz 不受并发上限约束
///
/// 满载是「忙」不是「不健康」。让健康检查在满载时失败会触发编排系统重启，
/// 反而杀掉全部在途请求。
#[tokio::test]
async fn test_healthz_exempt_from_limit() {
    let server =
        fixture::TestServer::start_http_chunked(1024, 50, std::time::Duration::from_millis(200))
            .await;
    let proxy_path = format!("/http://127.0.0.1:{}/", server.addr.port());

    let proxy_port = start_proxy_with(Config {
        max_concurrent_requests: 1,
        ..Default::default()
    })
    .await;

    // 占满唯一的 permit
    let (_held, _) = open_and_read_headers(proxy_port, &proxy_path).await;

    // 代理请求被拒
    let rejected = full_request(proxy_port, &proxy_path).await;
    assert!(rejected.contains("503"), "代理请求应被拒：{rejected}");

    // 健康检查仍然通过
    let health = full_request(proxy_port, "/healthz").await;
    assert!(
        health.contains("200") && health.contains("ok"),
        "满载时 /healthz 仍应返回 200：{health}"
    );

    server.shutdown();
}

/// 启动一个按固定节奏持续发 chunk 的上游，返回端口
///
/// 每个 chunk 固定 16 字节 `A`，chunk 之间间隔 `gap`。
async fn start_paced_upstream(chunks: usize, gap: std::time::Duration) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await;
        for _ in 0..chunks {
            tokio::time::sleep(gap).await;
            if sock.write_all(b"10\r\nAAAAAAAAAAAAAAAA\r\n").await.is_err() {
                return;
            }
        }
        let _ = sock.write_all(b"0\r\n\r\n").await;
    });
    port
}

/// N1 不变量：持续有数据流动的传输不得因「总时长」被截断
///
/// 上游以 200ms 间隔发 25 个 chunk（约 5 秒），全程无任何空闲超过 200ms，
/// 而 body idle timeout 只有 1 秒 —— 总时长远超单次空闲上限也必须完整送达。
///
/// 注意：这条**不足以**复现原始缺陷。原始 `POOL_IDLE_TIMEOUT` 默认 30 秒，
/// 5 秒的传输在旧代码下同样不会被截断。真正能钉住原 bug 的是下面
/// `test_long_transfer_beyond_old_pool_deadline`（35 秒，默认 ignore）。
/// 这条留在默认测试集里，作用是快速守住「不设总时长上限」这个不变量。
#[tokio::test]
async fn test_long_transfer_not_truncated() {
    let upstream_port = start_paced_upstream(25, std::time::Duration::from_millis(200)).await;

    let proxy_port = start_proxy_with(Config {
        upstream_body_idle_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    })
    .await;

    let started = std::time::Instant::now();
    let resp = full_request(proxy_port, &format!("/http://127.0.0.1:{upstream_port}/")).await;
    let elapsed = started.elapsed();

    assert!(resp.contains("200"), "应返回 200：{resp}");
    assert_eq!(
        count_payload_bytes(&resp, 'A'),
        400,
        "持续流动的传输不得被截断，应收到全部 400 字节（耗时 {elapsed:?}）"
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(4),
        "传输本身约 5 秒，提前结束说明被截断：{elapsed:?}"
    );
}

/// N1 真回归：传输时长超过旧 `POOL_IDLE_TIMEOUT` 默认值（30s）仍不得截断
///
/// 这条是唯一能真正复现原始缺陷的测试：旧代码把 `POOL_IDLE_TIMEOUT`（默认 30 秒）
/// 当作整个连接 future 的总时长上限，因此这个 35 秒、全程持续有数据的传输
/// 会在第 30 秒被静默切断，客户端拿到 200 + 半截 body 且没有 chunked 终止符。
///
/// 因为要跑满 35 秒，默认 ignore。改动 `proxy.rs` 的连接驱动逻辑时请手动跑：
/// `cargo test --test concurrency -- --ignored`
#[tokio::test]
#[ignore = "耗时约 35 秒，改动连接驱动逻辑时手动运行"]
async fn test_long_transfer_beyond_old_pool_deadline() {
    // 70 个 chunk * 500ms = 35 秒，跨过旧的 30 秒硬上限
    let upstream_port = start_paced_upstream(70, std::time::Duration::from_millis(500)).await;

    let proxy_port = start_proxy_with(Config {
        upstream_body_idle_timeout: std::time::Duration::from_secs(5),
        ..Default::default()
    })
    .await;

    let started = std::time::Instant::now();
    let resp = full_request(proxy_port, &format!("/http://127.0.0.1:{upstream_port}/")).await;
    let elapsed = started.elapsed();

    assert!(resp.contains("200"), "应返回 200：{resp}");
    assert_eq!(
        count_payload_bytes(&resp, 'A'),
        70 * 16,
        "超过 30 秒的持续传输不得被截断（耗时 {elapsed:?}）"
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(34),
        "传输本身约 35 秒，提前结束说明被截断：{elapsed:?}"
    );
}

/// N1 回归：真正的空闲仍然必须触发超时
///
/// 上面放宽了总时长限制，这里确认「卡死」场景没有一起被放过：
/// 上游发完第一个 chunk 后长时间不发，必须在 idle timeout 后中止。
#[tokio::test]
async fn test_idle_stall_still_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await;
        let _ = sock.write_all(b"10\r\nAAAAAAAAAAAAAAAA\r\n").await;
        // 之后一直不发，模拟上游卡死
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });

    let proxy_port = start_proxy_with(Config {
        upstream_body_idle_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    })
    .await;

    let started = std::time::Instant::now();
    let resp = full_request(proxy_port, &format!("/http://127.0.0.1:{upstream_port}/")).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "上游卡死应在 idle timeout 后中止，实际耗时 {elapsed:?}"
    );
    assert_eq!(
        count_payload_bytes(&resp, 'A'),
        16,
        "应只收到第一个 chunk 的 16 字节"
    );
}
