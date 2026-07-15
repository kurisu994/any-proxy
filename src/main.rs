//! any-proxy 进程入口
//!
//! 参见 DESIGN.md Section 6（Streaming, pooling and lifecycle）和 Section 11（Container and process hardening）。
//!
//! 职责：
//! - 初始化 tracing
//! - 从环境变量加载配置
//! - 创建 AddressPolicy（含 DENY_CIDRS 和宿主接口刷新）
//! - 创建 TLS 配置
//! - 创建 Connector 和 Axum 应用
//! - 启动 HTTP 服务器，支持 SIGTERM 优雅关闭

use std::sync::Arc;

use any_proxy::app::create_app;
use any_proxy::config::Config;
use any_proxy::connector::{create_tls_config, ConnectTimeouts, Connector, TcpDialer};
use any_proxy::resolver::{AddressPolicy, SystemResolver};
use any_proxy::telemetry;

#[tokio::main]
async fn main() {
    // 0. health-check 子命令：容器健康检查用，避免依赖镜像内的 wget/curl
    //    （debian-slim 不含 wget，DESIGN §11 要求运行镜像只有二进制 + CA 证书）。
    //    连接本机 LISTEN_ADDR 端口发起 GET /healthz，200 则退出 0，否则退出 1。
    if std::env::args().nth(1).as_deref() == Some("health-check") {
        std::process::exit(run_health_check());
    }

    // 1. 初始化 tracing
    telemetry::init_tracing();

    // 2. 加载配置
    let config = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("配置错误: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        listen_addr = %config.listen_addr,
        max_concurrent = config.max_concurrent_requests,
        "启动 any-proxy"
    );

    // 3. 创建 AddressPolicy（spawn_host_refresh 内部会立即刷新一次）
    let policy = Arc::new(AddressPolicy::new().with_env_deny_cidrs());

    // 启动宿主接口定期刷新（立即刷新一次 + 每 60s 刷新）
    let _refresh_handle = policy.spawn_host_refresh(config.host_refresh_interval);

    // 4. 创建 TLS 配置
    let tls_config = create_tls_config();

    // 5. 创建 Connector（带连接各阶段超时）
    let timeouts = ConnectTimeouts {
        dns: config.dns_timeout,
        connect: config.connect_timeout,
        tls: config.tls_timeout,
    };
    let connector = Arc::new(
        Connector::with_tls(
            SystemResolver::new(),
            (*policy).clone(),
            TcpDialer::new(),
            tls_config,
        )
        .with_timeouts(timeouts),
    );

    // 6. 创建 Axum 应用
    let app = create_app(connector, config.clone());

    // 7. 启动服务器
    let listener = match tokio::net::TcpListener::bind(config.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("绑定地址 {} 失败: {e}", config.listen_addr);
            std::process::exit(1);
        }
    };

    tracing::info!("监听 {}", config.listen_addr);

    let shutdown = shutdown_signal();

    // Axum 的 with_graceful_shutdown 在 shutdown future 返回后
    // 停止接收新连接并等待活跃连接完成。
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;

    // grace 超时后强制退出
    let grace = config.shutdown_grace;
    let force_exit = tokio::time::timeout(grace, async {
        if let Err(e) = serve_result {
            tracing::error!("服务器错误: {e}");
            std::process::exit(1);
        }
    })
    .await;

    if force_exit.is_err() {
        tracing::warn!("优雅关闭超时（{grace:?}），强制退出");
        std::process::exit(0);
    }

    tracing::info!("any-proxy 已关闭");
}

/// 容器健康检查：连接本机监听端口发起一次 `GET /healthz`
///
/// 返回进程退出码：0 = 健康（响应行含 `200`），1 = 不健康。
/// 使用阻塞式 std::net，不引入额外依赖，也不需要镜像内有 wget/curl。
/// 监听地址通常是 `0.0.0.0:PORT`，客户端连接改用 `127.0.0.1:PORT`。
fn run_health_check() -> i32 {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    // 从 LISTEN_ADDR 取端口，默认 8080
    let listen = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let timeout = Duration::from_secs(3);
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("health-check 连接 {addr} 失败: {e}");
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let req = b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if let Err(e) = stream.write_all(req) {
        eprintln!("health-check 写入失败: {e}");
        return 1;
    }

    let mut buf = Vec::new();
    if let Err(e) = stream.read_to_end(&mut buf) {
        eprintln!("health-check 读取失败: {e}");
        return 1;
    }

    // 检查状态行是否为 200
    let head = String::from_utf8_lossy(&buf);
    let first_line = head.lines().next().unwrap_or("");
    if first_line.contains(" 200") {
        0
    } else {
        eprintln!("health-check 非 200 响应: {first_line}");
        1
    }
}

/// 优雅关闭信号处理
///
/// 等待 SIGTERM 或 SIGINT，然后返回，触发 Axum 的 graceful shutdown。
/// Axum 会停止接收新连接，等待活跃连接完成。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("收到关闭信号，开始优雅关闭...");
}
