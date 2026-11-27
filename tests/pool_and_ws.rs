//! 上游 HTTP/1.1 连接池与 WebSocket 透明升级的端到端回归测试
//!
//! 全部使用本地确定性 `TcpListener` 与可注入 `Resolver`/`Dialer`，不依赖公网。
//! 覆盖：
//! - 连接池 Keep-Alive 复用（同目标连续请求仅 1 次 dial，含 POST）
//! - `POOL_MAX_IDLE_PER_HOST = 0` 关闭连接池
//! - 池化连接半关闭后对 POST 请求零副作用透明重建
//! - 客户端中途断开时绝不归还脏连接到池中
//! - 启用连接池时危险目标（私网）零 dial
//! - `ws://` 与 `wss://` 透明升级端到端双向收发与敏感头清洗
//! - 上游返回非 101（如 401）按普通响应透传并带 CORS
//! - 上游畸形 101（缺少 Sec-WebSocket-Accept）被拦截返回 502
//! - WebSocket 并发 permit 覆盖整个会话生命周期直至断连
//! - WebSocket 空闲超时自动断连并归还并发 permit
//! - 危险 WebSocket 目标（私网）零 dial

#![cfg(test)]

mod fixture;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use any_proxy::app::create_app;
use any_proxy::config::Config;
use any_proxy::connector::{Connector, DialRecord, Dialer, TcpDialer};
use any_proxy::resolver::{AddressPolicy, ResolveResult, Resolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

#[derive(Clone)]
struct FixedResolver(Vec<IpAddr>);

impl Resolver for FixedResolver {
    async fn resolve(&self, _host: &str) -> Result<ResolveResult, any_proxy::ProxyError> {
        Ok(ResolveResult {
            addresses: self.0.clone(),
        })
    }
}

/// 包装真实 `TcpDialer`，统计实际发生 TCP dial 的次数与地址
#[derive(Clone)]
struct CountingTcpDialer {
    inner: TcpDialer,
    dials: Arc<AtomicUsize>,
    addrs: Arc<Mutex<Vec<SocketAddr>>>,
}

impl CountingTcpDialer {
    fn new() -> Self {
        Self {
            inner: TcpDialer::new(),
            dials: Arc::new(AtomicUsize::new(0)),
            addrs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn count(&self) -> usize {
        self.dials.load(Ordering::SeqCst)
    }

    fn dialed_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.lock().unwrap().clone()
    }
}

impl Dialer for CountingTcpDialer {
    async fn dial(&self, addr: SocketAddr) -> Result<DialRecord, any_proxy::ProxyError> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        self.addrs.lock().unwrap().push(addr);
        self.inner.dial(addr).await
    }
}

/// 启动可自定义 policy / resolver / dialer / tls 的测试代理
async fn start_proxy<R, D>(connector: Arc<Connector<R, D>>, config: Config) -> u16
where
    R: Resolver + Clone + Send + Sync + 'static,
    D: Dialer + Clone + Send + Sync + 'static,
{
    let app = create_app(connector, Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// 在字节流中查找子序列
fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 从任意异步流读取单条 HTTP 请求（支持 Content-Length）
async fn read_one_http_request<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find_subseq(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    Some((head, body))
}

/// 启动支持真实 HTTP/1.1 Keep-Alive 的本地上游服务器，统计 TCP accept 次数与收到的总请求数
async fn start_keep_alive_upstream() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let req_count = Arc::new(AtomicUsize::new(0));

    let accepts_c = accepts.clone();
    let req_count_c = req_count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            accepts_c.fetch_add(1, Ordering::SeqCst);
            let req_count_c = req_count_c.clone();
            tokio::spawn(async move {
                // 在同一条 TCP 连接上循环处理多条 HTTP/1.1 Keep-Alive 请求
                while let Some((_head, body)) = read_one_http_request(&mut sock).await {
                    let seq = req_count_c.fetch_add(1, Ordering::SeqCst) + 1;
                    let reply = format!("seq={seq}:{}", String::from_utf8_lossy(&body));
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        reply.len(),
                        reply
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    (port, accepts, req_count)
}

/// 通过代理发送一次 HTTP 请求并读取完整响应
async fn send_http(proxy_port: u16, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("连接代理应成功");
    let req = if body.is_empty() {
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    } else {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp).await;
    String::from_utf8_lossy(&resp).to_string()
}

/// 读取 HTTP 响应头（保留底层流供 WebSocket 升级后续读写）
async fn read_http_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(pos) = find_subseq(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos + 4]).to_string();
            let rest = buf[pos + 4..].to_vec();
            return (head, rest);
        }
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "读到响应头前连接意外关闭");
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// 构造一条标准 WebSocket 升级 HTTP 请求报文
fn build_ws_handshake_request(path: &str, extra_headers: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {extra_headers}\
         \r\n"
    )
}

/// 启动一个本地透明 WS 回显上游（明文 TCP），记录收到的握手头并把升级后的字节加上 `echo:` 回写
async fn start_ws_echo_upstream(response_head: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_c = seen.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seen_c = seen_c.clone();
            tokio::spawn(async move {
                let Some((head, _)) = read_one_http_request(&mut sock).await else {
                    return;
                };
                seen_c.lock().unwrap().push(head);
                if sock.write_all(response_head.as_bytes()).await.is_err() {
                    return;
                }
                if !response_head.starts_with("HTTP/1.1 101") {
                    return;
                }
                // 升级后持续将收到的每段字节加上 `echo:` 回写给客户端
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut out = b"echo:".to_vec();
                            out.extend_from_slice(&buf[..n]);
                            if sock.write_all(&out).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    (port, seen)
}

/// 启动本地 TLS (WSS) 回显上游服务器，返回 `(port, cert_der)`
async fn start_wss_echo_upstream(hostname: &str) -> (u16, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (cert_der, key_der) = fixture::generate_test_cert(hostname);
    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(sock).await else {
                    return;
                };
                if read_one_http_request(&mut tls).await.is_none() {
                    return;
                }
                let head = "HTTP/1.1 101 Switching Protocols\r\n\
                            Upgrade: websocket\r\n\
                            Connection: Upgrade\r\n\
                            Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
                if tls.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let mut buf = [0u8; 4096];
                while let Ok(n) = tls.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut out = b"wss:".to_vec();
                    out.extend_from_slice(&buf[..n]);
                    if tls.write_all(&out).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    (port, cert_der)
}

// =========================================================================
// 连接池端到端回归测试
// =========================================================================

/// 同一目标连续发送多次请求（含带 JSON body 的 POST），应复用同一条 Keep-Alive 连接（仅 1 次 dial）
#[tokio::test]
async fn test_pool_reuses_keep_alive_connection() {
    let (upstream_port, accepts, req_count) = start_keep_alive_upstream().await;
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;
    let path = format!("/http://127.0.0.1:{upstream_port}/v1/chat/completions");

    let r1 = send_http(proxy_port, "POST", &path, r#"{"msg":"one"}"#).await;
    let r2 = send_http(proxy_port, "POST", &path, r#"{"msg":"two"}"#).await;
    let r3 = send_http(proxy_port, "GET", &path, "").await;

    assert!(
        r1.contains("200") && r1.contains(r#"seq=1:{"msg":"one"}"#),
        "{r1}"
    );
    assert!(
        r2.contains("200") && r2.contains(r#"seq=2:{"msg":"two"}"#),
        "{r2}"
    );
    assert!(r3.contains("200") && r3.contains("seq=3:"), "{r3}");
    assert_eq!(req_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        dialer.count(),
        1,
        "连续 3 次同源 Keep-Alive 请求应只建立 1 次 TCP 连接"
    );
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
}

/// 将 `POOL_MAX_IDLE_PER_HOST` 设为 0 时，完全关闭连接池，每次请求重新建连
#[tokio::test]
async fn test_pool_disabled_dials_every_request() {
    let (upstream_port, accepts, _) = start_keep_alive_upstream().await;
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
    ));
    let config = Config {
        pool_max_idle_per_host: 0,
        ..Default::default()
    };
    let proxy_port = start_proxy(connector, config).await;
    let path = format!("/http://127.0.0.1:{upstream_port}/");

    for _ in 0..3 {
        let r = send_http(proxy_port, "GET", &path, "").await;
        assert!(r.contains("200"), "{r}");
    }
    assert_eq!(dialer.count(), 3, "关闭连接池后每次请求都应重新 dial");
    assert_eq!(accepts.load(Ordering::SeqCst), 3);
}

/// 池化连接被上游空闲关闭后，下一次 POST 请求应在发送字节前丢弃死连接并透明重建，零副作用成功
#[tokio::test]
async fn test_pool_rebuilds_transparently_for_post_after_upstream_close() {
    // 上游处理完 1 条请求且不发 Connection: close 就主动关掉 TCP（模拟服务端空闲断开）
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let seq = Arc::new(AtomicUsize::new(0));
    let seq_c = seq.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seq_c = seq_c.clone();
            tokio::spawn(async move {
                if let Some((_head, body)) = read_one_http_request(&mut sock).await {
                    let n = seq_c.fetch_add(1, Ordering::SeqCst) + 1;
                    let payload = format!("ok-{n}:{}", String::from_utf8_lossy(&body));
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
        }
    });

    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;
    let path = format!("/http://127.0.0.1:{upstream_port}/v1/messages");

    let r1 = send_http(proxy_port, "POST", &path, r#"{"step":1}"#).await;
    assert!(
        r1.contains("200") && r1.contains(r#"ok-1:{"step":1}"#),
        "{r1}"
    );

    // 稍等上游 TCP FIN 被 hyper 连接任务感知
    tokio::time::sleep(Duration::from_millis(50)).await;

    let r2 = send_http(proxy_port, "POST", &path, r#"{"step":2}"#).await;
    assert!(
        r2.contains("200") && r2.contains(r#"ok-2:{"step":2}"#),
        "池化连接已关后 POST 应透明重建成功，实际：{r2}"
    );
    assert_eq!(dialer.count(), 2, "死连接丢弃后应触发第二次 dial");
}

/// 客户端在流式响应未结束前提前断开时，脏连接绝不得归还连接池
#[tokio::test]
async fn test_pool_does_not_return_connection_on_client_abort() {
    let server =
        fixture::TestServer::start_http_chunked(1024, 50, Duration::from_millis(100)).await;
    let upstream_port = server.addr.port();

    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;
    let path = format!("/http://127.0.0.1:{upstream_port}/");

    // 读完响应头立刻断开（不读完 chunked body）
    {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let (head, _) = read_http_head(&mut s).await;
        assert!(head.contains("200"), "{head}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 第二次请求必须重新建连，说明前一条未读完的连接没有进入池中
    {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let (head, _) = read_http_head(&mut s).await;
        assert!(head.contains("200"), "{head}");
    }
    assert_eq!(
        dialer.count(),
        2,
        "客户端提前断开的连接不得入池，第二次请求必须重新 dial"
    );
    server.shutdown();
}

/// 启用连接池后，访问私网地址仍必须 403 且零 dial
#[tokio::test]
async fn test_pool_dangerous_target_zero_dial() {
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec!["10.0.0.1".parse().unwrap()]),
        AddressPolicy::new(), // 真实生产安全策略
        dialer.clone(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;

    let resp = send_http(proxy_port, "GET", "/http://internal.example/secret", "").await;
    assert!(
        resp.contains("403") && resp.contains("target_blocked"),
        "{resp}"
    );
    assert_eq!(dialer.count(), 0, "私网目标必须零次 dial");
    assert!(dialer.dialed_addrs().is_empty());
}

// =========================================================================
// WebSocket 透明升级端到端回归测试
// =========================================================================

const VALID_WS_101: &str = "HTTP/1.1 101 Switching Protocols\r\n\
    Upgrade: websocket\r\n\
    Connection: Upgrade\r\n\
    Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
    Sec-WebSocket-Protocol: realtime\r\n\
    Set-Cookie: upstream_leak=1\r\n\
    Server: hidden-backend\r\n\r\n";

/// `ws://` 端到端升级、敏感头剥离、业务头透传与全双工多轮双向收发
#[tokio::test]
async fn test_ws_end_to_end_bidirectional_relay() {
    let (upstream_port, seen_heads) = start_ws_echo_upstream(VALID_WS_101).await;
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
    ));
    let config = Config {
        auth_token: Some("proxy-secret".into()),
        ..Default::default()
    };
    let proxy_port = start_proxy(connector, config).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req = build_ws_handshake_request(
        &format!("/ws://127.0.0.1:{upstream_port}/v1/realtime?model=gpt-4o"),
        "Authorization: Bearer sk-llm-test\r\n\
         Cookie: browser_cookie=forbidden\r\n\
         X-Proxy-Token: proxy-secret\r\n\
         Sec-WebSocket-Protocol: realtime\r\n",
    );
    client.write_all(req.as_bytes()).await.unwrap();

    let (head, leftover) = read_http_head(&mut client).await;
    let lower_head = head.to_ascii_lowercase();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "应返回 101 升级响应：{head}"
    );
    assert!(lower_head.contains("upgrade: websocket"), "{head}");
    assert!(
        lower_head.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "{head}"
    );
    assert!(
        !lower_head.contains("set-cookie"),
        "101 响应不得泄露 Set-Cookie：{head}"
    );
    assert!(
        !lower_head.contains("server:"),
        "101 响应应剥离 Server：{head}"
    );
    assert!(leftover.is_empty());

    // 验证上游收到的握手请求清洗结果
    let seen = seen_heads.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let up_lower = seen[0].to_ascii_lowercase();
    assert!(seen[0].starts_with("GET /v1/realtime?model=gpt-4o HTTP/1.1"));
    assert!(up_lower.contains("authorization: bearer sk-llm-test"));
    assert!(!up_lower.contains("cookie:"), "Cookie 不得转发给上游");
    assert!(
        !up_lower.contains("x-proxy-token"),
        "X-Proxy-Token 不得转发给上游"
    );

    // 升级后执行两轮双向通信
    for msg in [&b"hello-realtime-1"[..], &b"audio-chunk-2"[..]] {
        client.write_all(msg).await.unwrap();
        let mut expected = b"echo:".to_vec();
        expected.extend_from_slice(msg);
        let mut got = vec![0u8; expected.len()];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(got, expected);
    }

    assert_eq!(dialer.count(), 1);
    // 验证实际连接地址在已验证集合中（127.0.0.1:upstream_port）
    assert_eq!(
        dialer.dialed_addrs(),
        vec![SocketAddr::from(([127, 0, 0, 1], upstream_port))]
    );
}

/// `wss://` 端到端 TLS 握手 + WebSocket 升级与双向收发
#[tokio::test]
async fn test_wss_end_to_end_tls_upgrade_and_relay() {
    let (upstream_port, cert_der) = start_wss_echo_upstream("realtime.local").await;
    let tls_config = fixture::test_tls_client_config(cert_der);
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::with_tls(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        dialer.clone(),
        tls_config,
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req = build_ws_handshake_request(
        &format!("/wss://realtime.local:{upstream_port}/v1/live"),
        "",
    );
    client.write_all(req.as_bytes()).await.unwrap();

    let (head, _) = read_http_head(&mut client).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "WSS 应成功升级 101：{head}"
    );

    client.write_all(b"ping-tls").await.unwrap();
    let mut got = [0u8; 12];
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"wss:ping-tls");
    assert_eq!(dialer.count(), 1);
}

/// 上游拒绝 WebSocket 升级（返回 401）时，原样透传状态码、响应体与 CORS 头
#[tokio::test]
async fn test_ws_upstream_non_101_passthrough() {
    let (upstream_port, _) = start_ws_echo_upstream(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 11\r\n\r\ninvalid key",
    )
    .await;
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        TcpDialer::new(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req = build_ws_handshake_request(
        &format!("/ws://127.0.0.1:{upstream_port}/v1/realtime"),
        "Connection: close, Upgrade\r\n",
    );
    // 用带单独连接读取全响应（仅测状态与 body）
    client.write_all(req.as_bytes()).await.unwrap();
    let (head, rest) = read_http_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("access-control-allow-origin: *"),
        "非 101 响应应带 CORS：{head}"
    );
    let mut body = rest;
    if body.len() < 11 {
        let mut tail = vec![0u8; 11 - body.len()];
        client.read_exact(&mut tail).await.unwrap();
        body.extend_from_slice(&tail);
    }
    assert_eq!(&body[..11], b"invalid key");
}

/// 上游返回畸形 101（缺少 Sec-WebSocket-Accept）时必须拒绝升级并返回 502
#[tokio::test]
async fn test_ws_invalid_101_missing_accept_rejected() {
    let (upstream_port, _) = start_ws_echo_upstream(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    )
    .await;
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        TcpDialer::new(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req =
        build_ws_handshake_request(&format!("/ws://127.0.0.1:{upstream_port}/v1/realtime"), "");
    client.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_http_head(&mut client).await;
    assert!(
        head.starts_with("HTTP/1.1 502"),
        "畸形 101 应返回 502：{head}"
    );
}

/// 并发 permit 必须覆盖整个 WebSocket 会话直到连接关闭才归还
#[tokio::test]
async fn test_ws_permit_held_until_session_closed() {
    let (upstream_port, _) = start_ws_echo_upstream(VALID_WS_101).await;
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        TcpDialer::new(),
    ));
    let config = Config {
        max_concurrent_requests: 1,
        ..Default::default()
    };
    let proxy_port = start_proxy(connector, config).await;

    // 建立第一条活跃 WS 连接（占满唯一 permit）
    let mut ws_client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req =
        build_ws_handshake_request(&format!("/ws://127.0.0.1:{upstream_port}/v1/realtime"), "");
    ws_client.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_http_head(&mut ws_client).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");

    // 确认 WS 确实活跃
    ws_client.write_all(b"hi").await.unwrap();
    let mut buf = [0u8; 7];
    ws_client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"echo:hi");

    // 此时第二个请求必须被并发上限立即拒绝（503）
    let overloaded = send_http(proxy_port, "GET", "/", "").await;
    assert!(
        overloaded.contains("503") && overloaded.contains("service_overloaded"),
        "WS 会话活跃期间并发槽位应被占用，实际：{overloaded}"
    );

    // 关闭 WS 连接后，permit 应自动释放
    drop(ws_client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recovered = send_http(proxy_port, "GET", "/", "").await;
    assert!(
        recovered.contains("200"),
        "WS 关闭后应归还 permit，后续请求恢复 200，实际：{recovered}"
    );
}

/// WebSocket 双向空闲超过 `WS_IDLE_TIMEOUT` 后自动断开并归还并发 permit
#[tokio::test]
async fn test_ws_idle_timeout_disconnects_and_releases_permit() {
    let (upstream_port, _) = start_ws_echo_upstream(VALID_WS_101).await;
    let connector = Arc::new(Connector::new(
        FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        AddressPolicy::allow_all_for_test(),
        TcpDialer::new(),
    ));
    let config = Config {
        max_concurrent_requests: 1,
        ws_idle_timeout: Duration::from_secs(1),
        ..Default::default()
    };
    let proxy_port = start_proxy(connector, config).await;

    let mut ws_client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req =
        build_ws_handshake_request(&format!("/ws://127.0.0.1:{upstream_port}/v1/realtime"), "");
    ws_client.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_http_head(&mut ws_client).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");

    // 静默等待超过 ws_idle_timeout（1s），代理应主动关闭连接并归还 permit
    let mut tmp = [0u8; 16];
    let read_res = tokio::time::timeout(Duration::from_secs(4), ws_client.read(&mut tmp)).await;
    assert!(
        matches!(read_res, Ok(Ok(0)) | Ok(Err(_))),
        "WS 空闲超时后应断开连接，实际：{read_res:?}"
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    let recovered = send_http(proxy_port, "GET", "/", "").await;
    assert!(
        recovered.contains("200"),
        "WS 空闲超时释放后 permit 应归还，实际：{recovered}"
    );
}

/// 指向私网地址的 WebSocket 升级请求必须返回 403 且零次 dial
#[tokio::test]
async fn test_ws_dangerous_target_zero_dial() {
    let dialer = CountingTcpDialer::new();
    let connector = Arc::new(Connector::new(
        FixedResolver(vec!["10.0.0.1".parse().unwrap()]),
        AddressPolicy::new(), // 真实生产安全策略
        dialer.clone(),
    ));
    let proxy_port = start_proxy(connector, Config::default()).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let req = build_ws_handshake_request("/wss://internal.example/v1/realtime", "");
    client.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_http_head(&mut client).await;

    assert!(
        head.starts_with("HTTP/1.1 403"),
        "私网 WS 应返回 403：{head}"
    );
    assert_eq!(dialer.count(), 0, "私网 WS 目标必须零次 dial");
    assert!(dialer.dialed_addrs().is_empty());
}
