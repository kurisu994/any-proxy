//! 逐 frame 空闲超时 Body wrapper
//!
//! 参见 DESIGN.md Section 6（Streaming, pooling and lifecycle）。
//!
//! 按相邻 frame 的空闲时间终止 Body 传输，不是总 Body 时长。
//! - `UPLOAD_IDLE_TIMEOUT`：上传请求体时相邻 frame 间的最大空闲时间
//! - `UPSTREAM_BODY_IDLE_TIMEOUT`：下载响应体时相邻 frame 间的最大空闲时间
//!
//! 超时后 Body 返回错误，Hyper/Axum 会中止流并关闭连接，
//! 调用方看到截断 Body，日志记录 `stream_aborted`。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Buf;
use http_body::Body as HttpBody;

/// 包装一个 `HttpBody`，在读取每个 frame 时应用 idle timeout
///
/// 使用 `tokio::time::Sleep`（Box::pin）注册定时器 waker，确保即使 inner body
/// 永久不产生数据，timeout 也能在指定时间后触发。
///
/// 同时累计透传的 data 字节数，用于遥测契约（DESIGN §9，N13）：
/// 流正常结束记 `stream complete`，中止时 `log_stream_aborted` 带真实
/// `request_id` 与字节数，替代过去硬编码的 `("unknown", 0)`。
pub struct IdleTimeoutBody<B: HttpBody> {
    /// 用 Option 包装以便在归还连接池前先释放底层 `Incoming`，让 Hyper 客户端状态机立刻回到 idle
    inner: Option<B>,
    timeout: Duration,
    /// 定时器：到期后唤醒 poll_frame 检查超时
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    /// 关联的 request_id（多个 body 共享 handler 的 id，用 Arc 避免克隆开销）
    request_id: Arc<str>,
    /// 方向标签："upload" / "download"
    direction: &'static str,
    /// 已透传的 data 字节累计
    bytes: u64,
    /// 是否已记过终态日志，避免重复
    finished: bool,
    /// 全局出口预算（配置后逐 frame 累加出口字节）
    budget: Option<std::sync::Arc<crate::budget::Budget>>,
    /// 流正常结束（`Poll::Ready(None)` 或 `inner.is_end_stream()` 为真）时触发的一次性回调
    ///
    /// 供上游连接池安全归还空闲连接：超时、frame 出错或客户端提前断开时永不触发，
    /// 连接直接随回调 Drop 关闭，避免未读完的脏流污染后续请求。
    on_complete: Option<Box<dyn FnOnce() + Send>>,
}

impl<B: HttpBody> IdleTimeoutBody<B> {
    /// 创建 idle timeout body（不带遥测上下文与预算，主要供测试使用）
    pub fn new(inner: B, timeout: Duration) -> Self {
        Self::with_context(inner, timeout, Arc::from("-"), "stream", None)
    }

    /// 创建带遥测上下文的 idle timeout body
    ///
    /// `request_id` 关联到 handler 的完成日志，`direction` 标记上传/下载方向，
    /// `budget` 配置后逐 frame 把出口字节计入全局预算。
    pub fn with_context(
        inner: B,
        timeout: Duration,
        request_id: Arc<str>,
        direction: &'static str,
        budget: Option<std::sync::Arc<crate::budget::Budget>>,
    ) -> Self {
        Self {
            inner: Some(inner),
            timeout,
            sleep: None,
            request_id,
            direction,
            bytes: 0,
            finished: false,
            budget,
            on_complete: None,
        }
    }

    /// 注册流正常结束时的回调
    pub fn with_on_complete<F>(mut self, f: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        self.on_complete = Some(Box::new(f));
        self
    }

    /// 启动或重置定时器
    fn reset_sleep(&mut self) {
        self.sleep = Some(Box::pin(tokio::time::sleep(self.timeout)));
    }

    /// 标记流正常完成：先释放底层 Body，再记日志并触发完成回调
    fn finish_complete(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // 先 drop 底层 `hyper::body::Incoming`，确保 Hyper HTTP/1.1 连接状态机
        // 在 `on_complete` 将 `SendRequest` 放回连接池前释放响应体占用。
        self.inner = None;
        tracing::debug!(
            request_id = %self.request_id,
            direction = self.direction,
            bytes = self.bytes,
            "stream complete"
        );
        if let Some(cb) = self.on_complete.take() {
            cb();
        }
    }

    /// 已透传的 data 字节数（测试用）
    #[cfg(test)]
    pub(crate) fn bytes_for_test(&self) -> u64 {
        self.bytes
    }
}

impl<B: HttpBody> Drop for IdleTimeoutBody<B> {
    fn drop(&mut self) {
        // Hyper HTTP/1.1 服务端写出带精确 `Content-Length` 或空 Body（如 HEAD / 204）
        // 的响应后，可能不再多调一次 `poll_frame()` 就直接 Drop Body。
        // 此时若底层 `inner.is_end_stream()` 已为 true，说明上游响应体确已完整消费，
        // 应正常触发完成日志与连接池归还；若尚未读完（客户端中途断开）则绝不触发。
        if !self.finished && self.inner.as_ref().is_some_and(HttpBody::is_end_stream) {
            self.finish_complete();
        }
    }
}

impl<B> HttpBody for IdleTimeoutBody<B>
where
    B: HttpBody + Unpin,
    B::Error: std::fmt::Debug + Send + 'static,
{
    type Data = B::Data;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        // 首次调用时启动定时器
        if this.sleep.is_none() {
            this.reset_sleep();
        }

        // 循环是为了丢弃 trailer frame 后继续读下一个 frame，
        // 而不是把 trailer 透传给下游。
        loop {
            // 1. 先检查定时器是否到期
            if let Some(sleep) = &mut this.sleep {
                if sleep.as_mut().poll(cx).is_ready() {
                    tracing::warn!(
                        timeout_ms = this.timeout.as_millis() as u64,
                        "body idle timeout，中止流"
                    );
                    if !this.finished {
                        this.finished = true;
                        crate::telemetry::log_stream_aborted(
                            &this.request_id,
                            "body_idle_timeout",
                            this.bytes,
                        );
                    }
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "body idle timeout",
                    ))));
                }
            }

            // 2. 轮询内部 body
            let Some(inner) = this.inner.as_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    // 有活动，重置定时器
                    this.reset_sleep();

                    if let Some(data) = frame.data_ref() {
                        let n = data.remaining() as u64;
                        this.bytes += n;
                        if let Some(b) = &this.budget {
                            b.add_egress(n);
                        }
                        // 定长响应读完最后一帧后 `is_end_stream()` 会立刻变为 true，
                        // 提前完成清理并归还连接池，避免依赖调用方额外再 poll 一次 None。
                        if this.inner.as_ref().is_some_and(HttpBody::is_end_stream) {
                            this.finish_complete();
                        }
                        return Poll::Ready(Some(Ok(frame)));
                    }

                    // trailer frame：直接丢弃，不透传。
                    // DESIGN §7 承诺不转发 trailers；更重要的是 trailer frame
                    // 不经过 headers::clean_request/response_headers，若透传会让
                    // Set-Cookie、Cookie、转发头、CORS 等借 trailer 绕过全部清理策略。
                    tracing::debug!("丢弃上游 trailer frame");
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::warn!(error = ?e, "body frame 错误");
                    if !this.finished {
                        this.finished = true;
                        crate::telemetry::log_stream_aborted(
                            &this.request_id,
                            "body_frame_error",
                            this.bytes,
                        );
                    }
                    return Poll::Ready(Some(Err(std::io::Error::other("body frame error"))));
                }
                Poll::Ready(None) => {
                    this.finish_complete();
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    // inner 还没数据，定时器已在上面注册了 waker
                    // 当定时器到期或 inner 有数据时都会被唤醒
                    return Poll::Pending;
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.as_ref().is_none_or(HttpBody::is_end_stream)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner
            .as_ref()
            .map_or_else(http_body::SizeHint::default, HttpBody::size_hint)
    }
}

/// 将 idle timeout 应用于上游响应 body，返回 Axum `Body`
///
/// 使用 `UPSTREAM_BODY_IDLE_TIMEOUT` 配置。`request_id` 用于流中止/完成日志。
pub fn wrap_response_body(
    body: hyper::body::Incoming,
    timeout: Duration,
    request_id: Arc<str>,
    budget: Arc<crate::budget::Budget>,
) -> axum::body::Body {
    wrap_response_body_with_on_complete(body, timeout, request_id, budget, None)
}

/// 将 idle timeout 应用于上游响应 body，并附带正常 EOF 完成回调（用于连接池归还）
pub fn wrap_response_body_with_on_complete(
    body: hyper::body::Incoming,
    timeout: Duration,
    request_id: Arc<str>,
    budget: Arc<crate::budget::Budget>,
    on_complete: Option<Box<dyn FnOnce() + Send>>,
) -> axum::body::Body {
    use http_body_util::BodyExt;
    let mut wrapped =
        IdleTimeoutBody::with_context(body, timeout, request_id, "download", Some(budget));
    wrapped.on_complete = on_complete;
    axum::body::Body::new(wrapped.map_err(axum::Error::new))
}

/// 将 idle timeout 应用于上传请求 body，返回 Axum `Body`
///
/// 使用 `UPLOAD_IDLE_TIMEOUT` 配置。`request_id` 用于流中止/完成日志，
/// `budget` 把上传出口字节计入全局预算。
pub fn wrap_request_body(
    body: axum::body::Body,
    timeout: Duration,
    request_id: Arc<str>,
    budget: Arc<crate::budget::Budget>,
) -> axum::body::Body {
    use http_body_util::BodyExt;
    let wrapped = IdleTimeoutBody::with_context(body, timeout, request_id, "upload", Some(budget));
    axum::body::Body::new(wrapped.map_err(axum::Error::new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use http::HeaderMap;
    use http_body_util::BodyExt;
    use std::convert::Infallible;

    /// 先发一个 data frame，再发一个带 Set-Cookie 的 trailer frame
    struct BodyWithTrailer {
        data_sent: bool,
        trailer_sent: bool,
    }

    impl HttpBody for BodyWithTrailer {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            let this = self.get_mut();
            if !this.data_sent {
                this.data_sent = true;
                return Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(
                    b"hello",
                )))));
            }
            if !this.trailer_sent {
                this.trailer_sent = true;
                let mut tm = HeaderMap::new();
                tm.insert("set-cookie", "evil=1".parse().unwrap());
                return Poll::Ready(Some(Ok(http_body::Frame::trailers(tm))));
            }
            Poll::Ready(None)
        }
    }

    /// M2 回归：trailer frame 必须被丢弃，不能透传给下游
    ///
    /// 否则 Set-Cookie / Cookie / CORS 等可借 trailer 绕过 header 清理。
    #[tokio::test]
    async fn test_trailer_frame_dropped() {
        let body = IdleTimeoutBody::new(
            BodyWithTrailer {
                data_sent: false,
                trailer_sent: false,
            },
            Duration::from_secs(30),
        );
        let collected = body.collect().await.unwrap();
        assert!(
            collected.trailers().is_none(),
            "trailer 必须被丢弃，不能透传"
        );
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"hello"));
    }

    /// data frame 正常透传，不受 trailer 丢弃逻辑影响
    #[tokio::test]
    async fn test_data_frames_pass_through() {
        let body = IdleTimeoutBody::new(
            axum::body::Body::from("hello world"),
            Duration::from_secs(30),
        );
        let collected = body.collect().await.unwrap();
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"hello world"));
    }

    /// N13：透传 data frame 时按字节累加，供遥测契约使用
    #[tokio::test]
    async fn test_byte_count_accumulates() {
        let mut body = IdleTimeoutBody::new(
            axum::body::Body::from("hello world"), // 11 字节
            Duration::from_secs(30),
        );
        // 逐帧读完，不消费 body 本身
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
        assert_eq!(body.bytes_for_test(), 11);
    }

    /// 正常读到 EOF 触发 on_complete；中途 drop 不触发，防止脏连接归还连接池
    #[tokio::test]
    async fn test_on_complete_only_on_eof() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let body = IdleTimeoutBody::new(axum::body::Body::from("ok"), Duration::from_secs(30))
            .with_on_complete(move || {
                fired_clone.store(true, Ordering::SeqCst);
            });
        let _ = body.collect().await.unwrap();
        assert!(fired.load(Ordering::SeqCst), "读到 EOF 后必须触发回调");

        // 未读完直接 Drop，绝不能触发 on_complete
        let fired2 = Arc::new(AtomicBool::new(false));
        let fired2_clone = fired2.clone();
        let body2 = IdleTimeoutBody::new(axum::body::Body::from("unread"), Duration::from_secs(30))
            .with_on_complete(move || {
                fired2_clone.store(true, Ordering::SeqCst);
            });
        drop(body2);
        assert!(
            !fired2.load(Ordering::SeqCst),
            "流未读完即被 Drop 时不得触发完成回调"
        );
    }
}
