//! 进程级并发上限：permit 生命周期绑定到响应流结束
//!
//! 参见 DESIGN.md Section 6（Streaming, pooling and lifecycle）。
//!
//! # 为什么不用 `tower::limit::ConcurrencyLimitLayer`
//!
//! Tower 的 `ConcurrencyLimit` 在 handler 返回 `Response` 时就释放 permit，
//! 而流式 Body 的生命周期在那之后才真正开始。结果是 `MAX_CONCURRENT_REQUESTS`
//! 只限制「正在构建响应」的请求数，完全不限制并发活动下载：调用方可以堆积
//! 远超上限的活跃流、socket 与上游连接任务，进程级资源上限形同虚设。
//!
//! 本模块改为：
//! - permit 在请求进入时获取，挂在响应 Body 上，Body 读完或被丢弃时才释放；
//! - 饱和时立即返回 503 `service_overloaded`，而不是靠背压无限排队
//!   （背压会让调用方看到「服务卡死」而不是「服务过载」）。
//!
//! `/healthz` 不受上限约束：满载是「忙」不是「不健康」，
//! 让健康检查在满载时失败会触发编排系统重启，反而杀掉在途请求。

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http_body::Body as HttpBody;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 持有并发 permit 的 Body wrapper
///
/// permit 随 Body 一起 `Drop`，因此并发计数覆盖整个流式响应的生命周期，
/// 而不是只覆盖到 handler 返回。
pub struct GuardedBody<B> {
    inner: B,
    /// 只为在 Drop 时归还 permit 而持有，不会被主动读取
    _permit: OwnedSemaphorePermit,
}

impl<B> GuardedBody<B> {
    /// 用 permit 包裹一个 Body
    pub fn new(inner: B, permit: OwnedSemaphorePermit) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }
}

impl<B> HttpBody for GuardedBody<B>
where
    B: HttpBody + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// 并发上限中间件
///
/// 获取 permit → 执行 handler → 把 permit 挂到响应 Body 上。
/// 无可用 permit 时立即返回 503，不排队。
pub async fn limit_concurrency(
    State(semaphore): State<Arc<Semaphore>>,
    req: Request,
    next: Next,
) -> Response {
    // /healthz 不占用 permit，也不会被上限拒绝
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let permit = match semaphore.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let request_id = crate::telemetry::generate_request_id();
            tracing::warn!(
                request_id = %request_id,
                "并发达到上限，拒绝请求"
            );
            return crate::error::build_error_response(
                &crate::ProxyError::ServiceOverloaded,
                &request_id,
            );
        }
    };

    let response = next.run(req).await;

    // 把 permit 移交给响应 Body：流没结束前不归还
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(GuardedBody::new(body, permit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn test_permit_released_only_after_body_drop() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let body = GuardedBody::new(axum::body::Body::from("hello"), permit);

        // permit 仍被 Body 持有
        assert_eq!(sem.available_permits(), 0);

        drop(body);
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn test_permit_held_across_body_read() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let body = GuardedBody::new(axum::body::Body::from("hello"), permit);

        assert_eq!(sem.available_permits(), 0);
        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(&collected[..], b"hello");
        // collect 消耗了 Body，permit 随之归还
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn test_size_hint_delegated() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let inner = axum::body::Body::from("hello");
        let expected = inner.size_hint().exact();
        let body = GuardedBody::new(inner, permit);
        assert_eq!(body.size_hint().exact(), expected);
    }
}
