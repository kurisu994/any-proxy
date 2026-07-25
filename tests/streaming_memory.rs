//! 流式转发的内存上界回归测试（N10）
//!
//! `relay.rs::test_streaming_256mib` 只统计收到的字节数，而字节数对
//! 「真流式」和「先把 256 MiB 全缓冲进内存再吐出去」是**完全一样的**——
//! 它无法证明 DESIGN M1 成功标准 3「大 body 不线性占用内存」。
//!
//! 本测试用自定义全局分配器记录进程峰值堆占用，把这条属性变成可断言的事实。
//!
//! 单独作为一个 test binary：`#[global_allocator]` 是进程级的，若与其他测试
//! 并行跑，别的测试的分配会污染计数。

#![cfg(test)]

mod fixture;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use any_proxy::app::create_app;
use any_proxy::config::Config;
use any_proxy::connector::{Connector, TcpDialer};
use any_proxy::resolver::{AddressPolicy, ResolveResult, Resolver};

/// 统计当前与峰值堆占用的分配器
///
/// 不覆盖 `realloc`/`alloc_zeroed`：`GlobalAlloc` 的默认实现会转调本类型的
/// `alloc` 与 `dealloc`，计数因此仍然准确（代价是 realloc 少了一次原地扩展的
/// 优化，对测试无影响）。
struct TrackingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let cur = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(cur, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static ALLOC: TrackingAllocator = TrackingAllocator;

#[derive(Clone)]
struct LoopbackResolver;

impl Resolver for LoopbackResolver {
    async fn resolve(&self, _host: &str) -> Result<ResolveResult, any_proxy::ProxyError> {
        Ok(ResolveResult {
            addresses: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        })
    }
}

async fn start_proxy() -> u16 {
    let policy = AddressPolicy::allow_all_for_test();
    let connector = Arc::new(Connector::new(LoopbackResolver, policy, TcpDialer::new()));
    let app = create_app(connector, Arc::new(Config::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// 传输 256 MiB 时，峰值堆占用必须远低于传输体量
///
/// 阈值 32 MiB：真流式实现的峰值只有若干读缓冲 + fixture 的 1 MiB chunk 源数据，
/// 实测在个位数 MiB；而任何「整体缓冲」实现都会冲到 256 MiB 以上。
/// 阈值取在两者之间且留足余量，既能抓住回归，又不会因分配器抖动而 flaky。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_streaming_256mib_does_not_buffer_in_memory() {
    const CHUNK: usize = 1024 * 1024;
    const COUNT: usize = 256;
    const TOTAL: usize = CHUNK * COUNT;
    const PEAK_LIMIT: usize = 32 * 1024 * 1024;

    let server =
        fixture::TestServer::start_http_chunked(CHUNK, COUNT, std::time::Duration::ZERO).await;
    let upstream_port = server.addr.port();
    let proxy_port = start_proxy().await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("连接代理应成功");

    // 基线：连接已建立、传输尚未开始。之后的增量才是「传输本身」的内存代价。
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);

    let request = format!(
        "GET /http://127.0.0.1:{upstream_port}/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("发送应成功");

    // 关键：边读边丢，只累加计数。客户端自己缓冲 256 MiB 会让本测试失去意义。
    let mut buf = vec![0u8; 64 * 1024];
    let mut total_read = 0usize;
    // payload 字节数单独计数：total_read 含 chunked 编码开销，不足以证明数据完整
    let mut payload_bytes = 0usize;
    let mut head = Vec::new();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if head.len() < 64 {
                    head.extend_from_slice(&buf[..n.min(64)]);
                }
                payload_bytes += buf[..n].iter().filter(|&&b| b == b'A').count();
                total_read += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("读取失败: {e}"),
        }
    }

    let peak = PEAK.load(Ordering::Relaxed);
    let delta = peak.saturating_sub(baseline);

    assert!(
        String::from_utf8_lossy(&head).contains("200 OK"),
        "应返回 200 OK，实际响应头: {}",
        String::from_utf8_lossy(&head)
    );
    assert!(
        total_read >= TOTAL,
        "应至少收到 {TOTAL} 字节，实际 {total_read}"
    );
    // chunk header 里的 hex digit 也可能是 'A'，故用 >=
    assert!(
        payload_bytes >= TOTAL,
        "payload 应完整送达 {TOTAL} 字节，实际 {payload_bytes}"
    );
    assert!(
        delta < PEAK_LIMIT,
        "传输 {} MiB 期间峰值堆增量为 {} MiB，超过上限 {} MiB——响应体疑似被整体缓冲而非流式转发",
        TOTAL / 1024 / 1024,
        delta / 1024 / 1024,
        PEAK_LIMIT / 1024 / 1024,
    );

    server.shutdown();
}
