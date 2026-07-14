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
use std::task::{Context, Poll};
use std::time::Duration;

use http_body::Body as HttpBody;

/// 包装一个 `HttpBody`，在读取每个 frame 时应用 idle timeout
///
/// 使用 `tokio::time::Sleep`（Box::pin）注册定时器 waker，确保即使 inner body
/// 永久不产生数据，timeout 也能在指定时间后触发。
pub struct IdleTimeoutBody<B> {
    inner: B,
    timeout: Duration,
    /// 定时器：到期后唤醒 poll_frame 检查超时
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<B> IdleTimeoutBody<B> {
    /// 创建 idle timeout body
    pub fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: None,
        }
    }

    /// 启动或重置定时器
    fn reset_sleep(&mut self) {
        self.sleep = Some(Box::pin(tokio::time::sleep(self.timeout)));
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

        // 1. 先检查定时器是否到期
        if let Some(sleep) = &mut this.sleep {
            if sleep.as_mut().poll(cx).is_ready() {
                tracing::warn!(
                    timeout_ms = this.timeout.as_millis() as u64,
                    "body idle timeout，中止流"
                );
                crate::telemetry::log_stream_aborted("unknown", "body_idle_timeout", 0);
                return Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "body idle timeout",
                ))));
            }
        }

        // 2. 轮询内部 body
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // 成功读取 frame，重置定时器
                this.reset_sleep();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::warn!(error = ?e, "body frame 错误");
                Poll::Ready(Some(Err(std::io::Error::other("body frame error"))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                // inner 还没数据，定时器已在上面注册了 waker
                // 当定时器到期或 inner 有数据时都会被唤醒
                Poll::Pending
            }
        }
    }
}

/// 将 idle timeout 应用于上游响应 body，返回 Axum `Body`
///
/// 使用 `UPSTREAM_BODY_IDLE_TIMEOUT` 配置。
pub fn wrap_response_body(body: hyper::body::Incoming, timeout: Duration) -> axum::body::Body {
    use http_body_util::BodyExt;
    let wrapped = IdleTimeoutBody::new(body, timeout);
    axum::body::Body::new(wrapped.map_err(axum::Error::new))
}

/// 将 idle timeout 应用于上传请求 body，返回 Axum `Body`
///
/// 使用 `UPLOAD_IDLE_TIMEOUT` 配置。
pub fn wrap_request_body(body: axum::body::Body, timeout: Duration) -> axum::body::Body {
    use http_body_util::BodyExt;
    let wrapped = IdleTimeoutBody::new(body, timeout);
    axum::body::Body::new(wrapped.map_err(axum::Error::new))
}
