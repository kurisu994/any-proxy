//! 本地 HTTP/HTTPS 测试 fixture
//!
//! 提供本地 HTTP 和 HTTPS 服务器用于集成测试。
//! HTTPS 使用 rcgen 生成自签名证书。
//! DNS/Resolver/Dialer 均可注入，测试不依赖公网。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::CertificateParams;
use rcgen::KeyPair;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
