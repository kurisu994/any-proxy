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
    join_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 启动本地 HTTP 服务器
    ///
    /// 服务器返回固定的 "hello" 响应。
    pub async fn start_http() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let join_handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        Self {
            addr,
            hostname: "localhost".to_string(),
            cert_der: None,
            join_handle,
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
