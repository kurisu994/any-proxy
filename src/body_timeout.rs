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
pub struct IdleTimeoutBody<B> {
    inner: B,
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
}

impl<B> IdleTimeoutBody<B> {
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
            inner,
            timeout,
            sleep: None,
            request_id,
            direction,
            bytes: 0,
            finished: false,
            budget,
        }
    }

    /// 启动或重置定时器
    fn reset_sleep(&mut self) {
        self.sleep = Some(Box::pin(tokio::time::sleep(self.timeout)));
    }

    /// 已透传的 data 字节数（测试用）
    #[cfg(test)]
    pub(crate) fn bytes_for_test(&self) -> u64 {
        self.bytes
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
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    // 有活动，重置定时器
                    this.reset_sleep();

                    if let Some(data) = frame.data_ref() {
                        let n = data.remaining() as u64;
                        this.bytes += n;
                        if let Some(b) = &this.budget {
                            b.add_egress(n);
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
                    // 流正常结束：记完成日志，补全 DESIGN §9 契约的字节计数（N13）
                    if !this.finished {
                        this.finished = true;
                        tracing::debug!(
                            request_id = %this.request_id,
                            direction = this.direction,
                            bytes = this.bytes,
                            "stream complete"
                        );
                    }
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
    use http_body_util::BodyExt;
    let wrapped =
        IdleTimeoutBody::with_context(body, timeout, request_id, "download", Some(budget));
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
}
