//! WebSocket 透明升级转发（基于 Hyper 原生 HTTP/1.1 Upgrade，零新增依赖）
//!
//! # 安全与资源边界
//!
//! - **仅白名单放行 `Upgrade: websocket`**：拒绝 `h2c` 等其他升级协议，防止协议混淆。
//! - **复用全部前置安全校验**：调用本模块前已完成 `AUTH_TOKEN`、`ALLOW_ORIGINS`、
//!   `ALLOW_TARGETS`、`ALLOW_PORTS` 与 `Budget::check_admission()` 检查；
//!   上游连接走 `Connector::connect()` 原子完成 DNS 全量校验 → 固定 IP dial → TLS 握手，
//!   危险目标零 dial，且升级连接绝不进入 HTTP/1.1 连接池。
//! - **并发 permit 覆盖整个 WebSocket 会话**：升级为 `101` 时通过 [`PermitSlot::take`]
//!   接管 permit 并移交给后台双向转发任务，直到 WebSocket 彻底关闭才归还。
//! - **双向空闲超时与预算记账**：任一方向有数据流动即重置 `WS_IDLE_TIMEOUT`，
//!   并实时将双向转发字节计入全局 `Budget::add_egress`。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use http::request::Builder as RequestBuilder;
use http::StatusCode;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;

use crate::body_timeout;
use crate::budget::Budget;
use crate::concurrency::PermitSlot;
use crate::connector::Dialer;
use crate::headers;
use crate::proxy::ProxyState;
use crate::resolver::Resolver;
use crate::target::Target;

/// 双向转发单次读缓冲大小（16 KiB）
const RELAY_BUF_BYTES: usize = 16 * 1024;
/// 双端 `OnUpgrade` 就绪等待上限（防止客户端收到 101 后永不切换协议挂死）
const UPGRADE_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 判断入站请求是否为合法的 RFC 6455 WebSocket 升级握手
///
/// 严格要求：
/// - 方法为 `GET`
/// - `Connection` 头包含 `upgrade` token（大小写不敏感）
/// - `Upgrade` 头精确等于 `websocket`（大小写不敏感，拒绝 `h2c` 等其他协议）
/// - 存在非空 `Sec-WebSocket-Key`
/// - `Sec-WebSocket-Version` 等于 `13`
pub fn is_websocket_upgrade_request(method: &http::Method, headers: &http::HeaderMap) -> bool {
    if *method != http::Method::GET {
        return false;
    }

    let has_connection_upgrade = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|s| {
            s.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        });
    if !has_connection_upgrade {
        return false;
    }

    let is_ws_upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"));
    if !is_ws_upgrade {
        return false;
    }

    let has_key = headers
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty());
    if !has_key {
        return false;
    }

    let version_ok = headers
        .get("sec-websocket-version")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "13");

    version_ok
}

/// 校验上游 `101` 响应是否为合法的 WebSocket 协议升级
fn is_valid_upstream_ws_101(headers: &http::HeaderMap) -> bool {
    let upgrade_ws = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"));
    let has_accept = headers
        .get("sec-websocket-accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty());
    upgrade_ws && has_accept
}

/// 执行 WebSocket 透明升级转发
///
/// - 上游返回合法 `101 Switching Protocols` 时，接管 `permit_slot` 并启动双向转发任务，
///   向调用方返回清洗后的 `101` 响应；
/// - 上游返回非 `101`（如 `401`/`403`/`400`/`500`）时，不接管 permit，
///   按普通 HTTP 响应清洗 headers、补充 CORS 并流式返回。
pub async fn handle_websocket_upgrade<R, D>(
    state: &ProxyState<R, D>,
    target: &Target,
    mut req_headers: http::HeaderMap,
    inbound_upgrade: hyper::upgrade::OnUpgrade,
    permit_slot: Option<PermitSlot>,
    request_id: &str,
    cors_origin: &Option<String>,
) -> Result<Response, crate::ProxyError>
where
    R: Resolver + 'static,
    D: Dialer + 'static,
{
    // 1. 清洗请求头并重建目标 Host（保留 websocket 握手头，绝不透传 Cookie / X-Proxy-Token 等）
    headers::clean_ws_request_headers(&mut req_headers);
    req_headers.insert(http::header::HOST, headers::build_host_header(target));

    tracing::debug!(
        request_id = %request_id,
        target = %target.display_safe(),
        "建立 WebSocket 上游连接"
    );

    // 2. 始终走 Connector 建立新鲜连接（绝不从 HTTP/1.1 连接池取用）
    let conn = state.connector.connect(target).await?;
    let io = hyper_util::rt::TokioIo::new(conn.stream);

    // 3. HTTP/1.1 握手，并启用 `.with_upgrades()` 以便 101 后取回原始底层流
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| crate::ProxyError::UpstreamFailed {
                message: format!("WebSocket HTTP 握手失败: {e}"),
            })?;

    tokio::spawn(async move {
        if let Err(e) = connection.with_upgrades().await {
            tracing::debug!(error = %e, "WebSocket 上游连接任务结束");
        }
    });

    // 4. 构造并发送上游升级握手请求
    let request_target = target.request_target();
    let mut upstream_req = RequestBuilder::new()
        .method(http::Method::GET)
        .uri(&request_target)
        .version(http::Version::HTTP_11)
        .body(Body::empty())
        .map_err(|e| crate::ProxyError::UpstreamFailed {
            message: format!("构建 WebSocket 上游请求失败: {e}"),
        })?;
    *upstream_req.headers_mut() = req_headers;

    let mut upstream_resp = tokio::time::timeout(
        state.config.upstream_headers_timeout,
        sender.send_request(upstream_req),
    )
    .await
    .map_err(|_| crate::ProxyError::UpstreamTimeout)?
    .map_err(|e| crate::ProxyError::UpstreamFailed {
        message: format!("WebSocket 上游握手请求失败: {e}"),
    })?;

    // 5. 若上游拒绝升级（非 101），原样按普通响应返回（不拿走并发 permit）
    if upstream_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (mut parts, body) = upstream_resp.into_parts();
        headers::clean_response_headers(&mut parts.headers);
        headers::add_cors_headers(&mut parts.headers);
        let axum_body = body_timeout::wrap_response_body(
            body,
            state.config.upstream_body_idle_timeout,
            Arc::from(request_id),
            state.budget.clone(),
        );
        let mut resp = Response::from_parts(parts, axum_body);
        if let Some(o) = cors_origin {
            if let Ok(v) = http::HeaderValue::from_str(o) {
                resp.headers_mut().insert("access-control-allow-origin", v);
                resp.headers_mut()
                    .append("vary", http::HeaderValue::from_static("Origin"));
            }
        }
        return Ok(resp);
    }

    // 6. 校验上游 101 确为合法 WebSocket 协议升级
    if !is_valid_upstream_ws_101(upstream_resp.headers()) {
        return Err(crate::ProxyError::UpstreamFailed {
            message: "上游 101 响应缺少合法的 WebSocket 升级头".into(),
        });
    }

    // 在拆解 upstream_resp 前先取得上游升级 future
    let upstream_upgrade = hyper::upgrade::on(&mut upstream_resp);
    let (mut parts, _empty_body) = upstream_resp.into_parts();
    headers::clean_ws_response_headers(&mut parts.headers);

    // 7. 接管并发 permit，将其生命周期绑定到后台双向转发任务直到连接关闭
    let held_permit: Option<OwnedSemaphorePermit> = permit_slot.and_then(|slot| slot.take());
    let idle_timeout = state.config.ws_idle_timeout;
    let budget = state.budget.clone();
    let req_id: Arc<str> = Arc::from(request_id);

    tokio::spawn(async move {
        // 持有 permit 直到本任务退出
        let _permit = held_permit;

        let pair = tokio::time::timeout(UPGRADE_READY_TIMEOUT, async {
            tokio::try_join!(inbound_upgrade, upstream_upgrade)
        })
        .await;

        let (inbound_io, upstream_io) = match pair {
            Ok(Ok((in_up, up_up))) => (
                hyper_util::rt::TokioIo::new(in_up),
                hyper_util::rt::TokioIo::new(up_up),
            ),
            Ok(Err(e)) => {
                tracing::warn!(request_id = %req_id, error = %e, "WebSocket 协议升级失败");
                return;
            }
            Err(_) => {
                tracing::warn!(request_id = %req_id, "WebSocket 协议升级就绪超时");
                return;
            }
        };

        let stats = relay_bidirectional(inbound_io, upstream_io, idle_timeout, Some(&budget)).await;
        tracing::debug!(
            request_id = %req_id,
            upload_bytes = stats.upload_bytes,
            download_bytes = stats.download_bytes,
            reason = stats.close_reason,
            "websocket closed"
        );
    });

    Ok(Response::from_parts(parts, Body::empty()))
}

/// WebSocket 双向转发统计结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayStats {
    pub upload_bytes: u64,
    pub download_bytes: u64,
    pub close_reason: &'static str,
}

/// 带共享空闲超时与出口预算记账的全双工透明流转发
///
/// - 任一方向读到有效字节即重置空闲计时器；
/// - 每次读写均将字节计入 `budget`（与 HTTP Body 一致统计双向出口）；
/// - 支持半关闭（一端 EOF 时只关闭对端写半边，等待另一端优雅发完 Close 帧）；
/// - 写操作同样受 `idle_timeout` 约束，避免对端反压死锁导致任务永不退出。
pub async fn relay_bidirectional<C, U>(
    client: C,
    upstream: U,
    idle_timeout: Duration,
    budget: Option<&Budget>,
) -> RelayStats
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut c_read, mut c_write) = tokio::io::split(client);
    let (mut u_read, mut u_write) = tokio::io::split(upstream);

    let mut c2u_buf = vec![0u8; RELAY_BUF_BYTES];
    let mut u2c_buf = vec![0u8; RELAY_BUF_BYTES];
    let mut c2u_done = false;
    let mut u2c_done = false;
    let mut upload_bytes = 0u64;
    let mut download_bytes = 0u64;

    let idle_sleep = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_sleep);

    let close_reason = loop {
        if c2u_done && u2c_done {
            break "eof";
        }

        tokio::select! {
            biased;
            _ = &mut idle_sleep => {
                let _ = c_write.shutdown().await;
                let _ = u_write.shutdown().await;
                break "ws_idle_timeout";
            }
            res = c_read.read(&mut c2u_buf), if !c2u_done => {
                match res {
                    Ok(0) => {
                        c2u_done = true;
                        let _ = u_write.shutdown().await;
                    }
                    Ok(n) => {
                        idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                        upload_bytes += n as u64;
                        if let Some(b) = budget {
                            b.add_egress(n as u64);
                        }
                        match tokio::time::timeout(idle_timeout, u_write.write_all(&c2u_buf[..n])).await {
                            Ok(Ok(())) => {
                                let _ = u_write.flush().await;
                            }
                            Ok(Err(_)) => break "upstream_write_error",
                            Err(_) => break "upstream_write_timeout",
                        }
                    }
                    Err(_) => break "client_read_error",
                }
            }
            res = u_read.read(&mut u2c_buf), if !u2c_done => {
                match res {
                    Ok(0) => {
                        u2c_done = true;
                        let _ = c_write.shutdown().await;
                    }
                    Ok(n) => {
                        idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                        download_bytes += n as u64;
                        if let Some(b) = budget {
                            b.add_egress(n as u64);
                        }
                        match tokio::time::timeout(idle_timeout, c_write.write_all(&u2c_buf[..n])).await {
                            Ok(Ok(())) => {
                                let _ = c_write.flush().await;
                            }
                            Ok(Err(_)) => break "client_write_error",
                            Err(_) => break "client_write_timeout",
                        }
                    }
                    Err(_) => break "upstream_read_error",
                }
            }
        }
    };

    RelayStats {
        upload_bytes,
        download_bytes,
        close_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_headers() -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        h.insert("upgrade", "websocket".parse().unwrap());
        h.insert(
            "sec-websocket-key",
            "dGhlIHNhbXBsZSBub25jZQ==".parse().unwrap(),
        );
        h.insert("sec-websocket-version", "13".parse().unwrap());
        h
    }

    #[test]
    fn test_is_websocket_upgrade_request_valid() {
        assert!(is_websocket_upgrade_request(
            &http::Method::GET,
            &ws_headers()
        ));
    }

    #[test]
    fn test_is_websocket_upgrade_rejects_non_websocket_and_non_get() {
        let mut h2c = ws_headers();
        h2c.insert("upgrade", "h2c".parse().unwrap());
        assert!(!is_websocket_upgrade_request(&http::Method::GET, &h2c));

        assert!(!is_websocket_upgrade_request(
            &http::Method::POST,
            &ws_headers()
        ));

        let mut bad_ver = ws_headers();
        bad_ver.insert("sec-websocket-version", "8".parse().unwrap());
        assert!(!is_websocket_upgrade_request(&http::Method::GET, &bad_ver));

        let mut no_key = ws_headers();
        no_key.remove("sec-websocket-key");
        assert!(!is_websocket_upgrade_request(&http::Method::GET, &no_key));
    }

    #[tokio::test]
    async fn test_relay_bidirectional_echo_and_budget() {
        let (mut client_peer, client_side) = tokio::io::duplex(4096);
        let (upstream_side, mut upstream_peer) = tokio::io::duplex(4096);
        let budget = Budget::new(Some(1024 * 1024), None);

        let relay_task = tokio::spawn({
            let budget_ref = Arc::new(budget);
            let b = budget_ref.clone();
            async move {
                let stats = relay_bidirectional(
                    client_side,
                    upstream_side,
                    Duration::from_secs(5),
                    Some(&b),
                )
                .await;
                (stats, b.egress_used())
            }
        });

        // 客户端发 ping，上游收后回 pong，随后双端有序关闭
        client_peer.write_all(b"ping-from-client").await.unwrap();
        let mut buf = [0u8; 64];
        let n = upstream_peer.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping-from-client");

        upstream_peer
            .write_all(b"pong-from-upstream")
            .await
            .unwrap();
        let n2 = client_peer.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n2], b"pong-from-upstream");

        client_peer.shutdown().await.unwrap();
        upstream_peer.shutdown().await.unwrap();

        let (stats, used) = relay_task.await.unwrap();
        assert_eq!(stats.close_reason, "eof");
        assert_eq!(stats.upload_bytes, 16);
        assert_eq!(stats.download_bytes, 18);
        assert_eq!(used, 34);
    }

    #[tokio::test]
    async fn test_relay_bidirectional_idle_timeout() {
        let (_client_peer, client_side) = tokio::io::duplex(4096);
        let (upstream_side, _upstream_peer) = tokio::io::duplex(4096);

        let stats =
            relay_bidirectional(client_side, upstream_side, Duration::from_millis(80), None).await;
        assert_eq!(stats.close_reason, "ws_idle_timeout");
    }
}
