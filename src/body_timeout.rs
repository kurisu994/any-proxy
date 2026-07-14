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

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body::Body as HttpBody;

/// 包装一个 `HttpBody`，在读取每个 frame 时应用 idle timeout
///
/// 每次 `poll_data` 成功后重置计时器；如果两次 frame 间空闲超过 `timeout`，
/// 返回 `std::io::ErrorKind::TimedOut`。
pub struct IdleTimeoutBody<B> {
    inner: B,
    timeout: Duration,
    /// 上一次成功读取 frame 的时间
    last_frame: Option<std::time::Instant>,
}

impl<B> IdleTimeoutBody<B> {
    /// 创建 idle timeout body
    pub fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            last_frame: None,
        }
    }
}

impl<B> HttpBody for IdleTimeoutBody<B>
where
    B: HttpBody + Unpin,
    B::Data: Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
{
    type Data = B::Data;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        // 检查 idle timeout
        if let Some(last) = this.last_frame {
            let elapsed = std::time::Instant::now().duration_since(last);
            if elapsed >= this.timeout {
                tracing::warn!(
                    timeout_ms = this.timeout.as_millis() as u64,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "body idle timeout，中止流"
                );
                return Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "body idle timeout",
                ))));
            }
        }

        // 轮询内部 body
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // 成功读取 frame，重置计时器
                this.last_frame = Some(std::time::Instant::now());
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::warn!(error = ?e, "body frame 错误");
                Poll::Ready(Some(Err(std::io::Error::other("body frame error"))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
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
