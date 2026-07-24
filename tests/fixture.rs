//! 本地 HTTP/HTTPS 测试 fixture
//!
//! 提供本地 HTTP 和 HTTPS 服务器用于集成测试。
//! HTTPS 使用 rcgen 生成自签名证书。
//! DNS/Resolver/Dialer 均可注入，测试不依赖公网。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::CertificateParams;
use rcgen::KeyPair;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

/// 本地测试服务器
pub struct TestServer {
    pub addr: SocketAddr,
    pub hostname: String,
    /// HTTPS 模式下的证书 DER（用于客户端 TLS 配置）
    pub cert_der: Option<Vec<u8>>,
    /// 当前活跃连接数（chunked 服务器模式）
    pub active_connections: Option<Arc<std::sync::atomic::AtomicUsize>>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 启动本地 HTTP 服务器
    ///
    /// 服务器返回固定的 "hello" 响应。
    pub async fn start_http() -> Self {
        Self::start_http_with_status_and_body(200, "hello").await
    }

    /// 启动本地 HTTP 服务器，返回指定状态码
    pub async fn start_http_with_status(status: u16) -> Self {
        Self::start_http_with_status_and_body(status, "response").await
    }

    /// 启动本地 HTTP 服务器，返回指定状态码和 body
    pub async fn start_http_with_status_and_body(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();

        let join_handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        Self {
            addr,
            hostname: "localhost".to_string(),
            cert_der: None,
            active_connections: None,
            join_handle,
        }
    }

    /// 启动本地 HTTP 服务器，返回 chunked 流式大 body
    ///
    /// 总大小 = `chunk_size` * `chunk_count`，使用 chunked transfer encoding。
    /// 每个 chunk 之间有 `delay` 的延迟（用于测试 idle timeout 和取消传播）。
    /// 返回的第二个值是当前活跃连接数的共享计数器。
    pub async fn start_http_chunked(
        chunk_size: usize,
        chunk_count: usize,
        delay: std::time::Duration,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let active_clone = active.clone();
        let join_handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                active_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let active_clone2 = active_clone.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;

                    // chunked transfer encoding header
                    let header = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        active_clone2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }

                    let chunk_data = vec![b'A'; chunk_size];
                    for _ in 0..chunk_count {
                        let chunk_header = format!("{:X}\r\n", chunk_size);
                        if socket.write_all(chunk_header.as_bytes()).await.is_err() {
                            active_clone2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        if socket.write_all(&chunk_data).await.is_err() {
                            active_clone2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        if socket.write_all(b"\r\n").await.is_err() {
                            active_clone2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                    }
                    // 结束 chunk
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                    active_clone2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
        });

        Self {
            addr,
            hostname: "localhost".to_string(),
            cert_der: None,
            join_handle,
            active_connections: Some(active),
        }
    }

    /// 启动本地 HTTPS 服务器
    ///
    /// 使用自签名证书。`hostname` 用于 TLS SNI。
    /// 返回的 `TestServer` 的 `cert_der` 字段包含证书 DER，
    /// 客户端可用它构建信任该证书的 TLS 配置。
    pub async fn start_https(hostname: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let cert_params = CertificateParams::new(vec![hostname.to_string()]).unwrap();
        let key_pair = KeyPair::generate().unwrap();
        let cert = cert_params.self_signed(&key_pair).unwrap();
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialized_der().to_vec();

        let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
        );
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let join_handle = tokio::spawn(async move {
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let mut tls_stream = match acceptor.accept(socket).await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let mut buf = vec![0u8; 4096];
                    let _ = tls_stream.read(&mut buf).await;
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
                    let _ = tls_stream.write_all(response.as_bytes()).await;
                });
            }
        });

        Self {
            addr,
            hostname: hostname.to_string(),
            cert_der: Some(cert_der),
            active_connections: None,
            join_handle,
        }
    }

    /// 关闭服务器
    pub fn shutdown(self) {
        self.join_handle.abort();
    }
}

/// 一条被录制的上游请求
///
/// 与 `TestServer` 不同，`RecordingServer` 完整解析请求行、headers 和 body，
/// 供测试断言「代理实际发给上游的是什么」——这是 N1/N2/N3 长期缺失的能力（N10）。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// 按名取 header 值（大小写不敏感）
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 预设响应：按请求顺序返回
#[derive(Clone)]
pub struct CannedResponse {
    pub status: u16,
    /// 3xx 的 `Location`（相对或绝对）
    pub location: Option<String>,
    pub body: String,
}

impl CannedResponse {
    /// 200 + 指定 body
    pub fn ok(body: &str) -> Self {
        Self {
            status: 200,
            location: None,
            body: body.to_string(),
        }
    }

    /// 3xx 重定向到 `location`
    pub fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            location: Some(location.to_string()),
            body: String::new(),
        }
    }
}

/// 录制式假上游服务器
///
/// 解析并记录每个请求（method / path / headers / body），按请求全局序号返回
/// 预设响应（序号超出时重复最后一条）。代理不复用连接，故重定向的每一跳都是新连接，
/// 用跨连接的原子计数器区分请求序号。
pub struct RecordingServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl RecordingServer {
    /// 启动录制服务器，`responses` 按请求顺序返回
    pub async fn start(responses: Vec<CannedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(responses);

        let req_store = requests.clone();
        let join_handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let req_store = req_store.clone();
                let counter = counter.clone();
                let responses = responses.clone();
                tokio::spawn(async move {
                    let recorded = match read_http_request(&mut socket).await {
                        Some(r) => r,
                        None => return,
                    };
                    req_store.lock().unwrap().push(recorded);

                    let idx = counter.fetch_add(1, Ordering::SeqCst);
                    let resp = responses
                        .get(idx)
                        .or_else(|| responses.last())
                        .cloned()
                        .unwrap_or_else(|| CannedResponse::ok("ok"));

                    let mut head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\n",
                        resp.status,
                        resp.body.len()
                    );
                    if let Some(loc) = &resp.location {
                        head.push_str(&format!("Location: {loc}\r\n"));
                    }
                    head.push_str("Connection: close\r\n\r\n");
                    let full = format!("{head}{}", resp.body);
                    let _ = socket.write_all(full.as_bytes()).await;
                });
            }
        });

        Self {
            addr,
            requests,
            join_handle,
        }
    }

    /// 返回已录制的所有请求（按到达顺序）
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 关闭服务器
    pub fn shutdown(self) {
        self.join_handle.abort();
    }
}

/// 在字节流中查找子序列位置
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 读取并解析一条完整 HTTP 请求（支持 Content-Length 与 chunked body）
async fn read_http_request(socket: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];

    // 1. 读到 headers 结束（\r\n\r\n）
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return None;
        }
    };

    // 2. 解析请求行与 headers
    let head_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    let mut is_chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            if k.eq_ignore_ascii_case("transfer-encoding") && v.to_lowercase().contains("chunked") {
                is_chunked = true;
            }
            headers.push((k, v));
        }
    }

    // 3. 读取 body
    let leftover = buf[header_end..].to_vec();
    let body = if is_chunked {
        read_chunked_body(socket, leftover).await
    } else {
        let mut body = leftover;
        while body.len() < content_length {
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(content_length);
        body
    };

    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// 读取 chunked 编码的 body，返回解码后的字节
async fn read_chunked_body(socket: &mut TcpStream, initial: Vec<u8>) -> Vec<u8> {
    let mut pending = initial;
    let mut body = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        // 读到一整行 chunk-size
        let line_end = loop {
            if let Some(p) = find_subsequence(&pending, b"\r\n") {
                break p;
            }
            match socket.read(&mut tmp).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&tmp[..n]),
                _ => return body,
            }
        };
        let size_str = String::from_utf8_lossy(&pending[..line_end]).to_string();
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        pending.drain(..line_end + 2);
        if size == 0 {
            break;
        }
        // 确保 pending 有 size + 2（数据 + CRLF）
        while pending.len() < size + 2 {
            match socket.read(&mut tmp).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&tmp[..n]),
                _ => break,
            }
        }
        let take = size.min(pending.len());
        body.extend_from_slice(&pending[..take]);
        pending.drain(..(size + 2).min(pending.len()));
    }
    body
}

/// 生成自签名测试证书，返回 DER 编码的证书和私钥
pub fn generate_test_cert(hostname: &str) -> (Vec<u8>, Vec<u8>) {
    let cert_params = CertificateParams::new(vec![hostname.to_string()]).unwrap();
    let key_pair = KeyPair::generate().unwrap();
    let cert = cert_params.self_signed(&key_pair).unwrap();
    (cert.der().to_vec(), key_pair.serialized_der().to_vec())
}

/// 构建 TLS 客户端配置，信任指定的自签名证书
pub fn test_tls_client_config(cert_der: Vec<u8>) -> Arc<rustls::ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(rustls::pki_types::CertificateDer::from(cert_der))
        .unwrap();

    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}
