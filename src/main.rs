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
