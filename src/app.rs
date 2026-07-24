//! Axum Router 与共享状态
//!
//! 参见 DESIGN.md Section 2（HTTP interface）和 Section 6（Streaming, pooling and lifecycle）。
//!
//! 路由：
//! - 所有请求由 `proxy_handler` 统一处理
//! - `proxy_handler` 内部根据路径分发到 healthz、首页、预检或代理转发
//! - 并发上限由 `concurrency::limit_concurrency` 在进入 handler 前获取 permit，
//!   并把 permit 挂到响应 Body 上，直到流结束才释放（见 `crate::concurrency`）

use std::sync::Arc;

use axum::Router;
use tokio::sync::Semaphore;

use crate::concurrency::limit_concurrency;
use crate::config::Config;
use crate::connector::{Connector, Dialer};
use crate::proxy::{proxy_handler, ProxyState};
use crate::resolver::Resolver;

/// 创建 Axum 应用
///
/// 组装 Router、共享状态和 Tower 中间件。
/// 泛型参数允许测试注入自定义 Resolver 和 Dialer。
pub fn create_app<R, D>(connector: Arc<Connector<R, D>>, config: Arc<Config>) -> Router
where
    R: Resolver + Clone + Send + Sync + 'static,
    D: Dialer + Clone + Send + Sync + 'static,
{
    let budget = Arc::new(crate::budget::Budget::new(
        config.max_egress_bytes,
        config.rate_limit_rps,
    ));

    let state = ProxyState {
        connector,
        config: config.clone(),
        budget,
    };

    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_requests));

    Router::new()
        .fallback(proxy_handler)
        .layer(axum::middleware::from_fn_with_state(
            semaphore,
            limit_concurrency,
        ))
        .with_state(state)
}
