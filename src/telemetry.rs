//! 无敏感信息的结构化日志与指标
//!
//! 参见 DESIGN.md Section 9（Observability and privacy）。
//!
//! # 隐私规则
//!
//! 结构化日志只记录 `request_id`、方法、目标 scheme、规范化 hostname、端口、
//! 最终状态、错误码、持续时间和流式字节计数。
//!
//! **不得记录**：URL query、userinfo、请求/响应 headers、Cookie、Authorization 或 Body。
//! 日志中的 hostname 也应支持关闭。

use std::sync::atomic::{AtomicU64, Ordering};

/// 全局请求 ID 计数器
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 进程启动时间（纳秒），用作 request_id 前缀
static PROCESS_START_NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// 生成唯一请求 ID
///
/// 格式：`{进程启动纳秒 hex}-{递增计数器 hex}`，如 `0192a3b4c5d6e7f8-0001`。
pub fn generate_request_id() -> String {
    let start_ns = *PROCESS_START_NS.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    });
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{start_ns:016x}-{counter:04x}")
}

/// 初始化 tracing-subscriber
///
/// 使用 JSON 格式输出，通过 `RUST_LOG` 环境变量控制日志级别。
/// 在测试环境中使用默认格式以便阅读。
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();
}

/// 流式中止日志
///
/// 响应 headers 已发出后 Body 传输中止时记录。
/// `request_id` 关联到 `proxy::log_complete` 的 headers 完成日志，
/// `bytes_sent` 为中止前已透传的字节数（N13）。
pub fn log_stream_aborted(request_id: &str, reason: &str, bytes_sent: u64) {
    tracing::warn!(
        request_id = %request_id,
        reason = %reason,
        bytes_sent = bytes_sent,
        "stream aborted"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_unique() {
        let id1 = generate_request_id();
        let id2 = generate_request_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_request_id_format() {
        let id = generate_request_id();
        // 格式应为 16 位 hex - 4 位 hex
        assert!(id.contains('-'));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 16);
        assert_eq!(parts[1].len(), 4);
    }
}
