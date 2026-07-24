//! 全局出口预算与限速（C1 批次 2）
//!
//! 参见 DESIGN.md Section 7。
//! - **出口字节预算**：全局累计代理出口字节（请求体 + 响应体），达到上限后拒绝新请求。
//! - **限速**：令牌桶，限制全局请求速率 rps。
//!
//! 进程级共享，通过 `Arc` 注入。预算是进程累计，重启即重置（本版不做每日窗口）。
//! 预算检查在请求准入时进行（此时 body 尚未传输），因此是软上限：单个正在进行的
//! 请求可能让总量略微超过 `MAX_EGRESS_BYTES`，但后续请求会被拒绝——足以为带宽账单兜底。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// 简单令牌桶：容量与每秒补充速率均为 rps
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rps: u32) -> Self {
        let cap = rps.max(1) as f64;
        Self {
            capacity: cap,
            tokens: cap,
            refill_per_sec: cap,
            last: Instant::now(),
        }
    }

    /// 尝试取一个令牌，成功返回 true
    fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// 全局出口预算与限速
pub struct Budget {
    egress_used: AtomicU64,
    max_egress_bytes: Option<u64>,
    rate_limiter: Option<Mutex<TokenBucket>>,
}

impl Budget {
    /// 从配置构造
    pub fn new(max_egress_bytes: Option<u64>, rate_limit_rps: Option<u32>) -> Self {
        Self {
            egress_used: AtomicU64::new(0),
            max_egress_bytes,
            rate_limiter: rate_limit_rps.map(|rps| Mutex::new(TokenBucket::new(rps))),
        }
    }

    /// 请求准入检查：先限速（429），再查出口预算（503）
    pub fn check_admission(&self) -> Result<(), crate::ProxyError> {
        if let Some(rl) = &self.rate_limiter {
            // 锁中毒时放行（不因内部错误拒绝服务）
            let ok = rl.lock().map(|mut b| b.try_acquire()).unwrap_or(true);
            if !ok {
                return Err(crate::ProxyError::RateLimited);
            }
        }
        if let Some(max) = self.max_egress_bytes {
            if self.egress_used.load(Ordering::Relaxed) >= max {
                return Err(crate::ProxyError::BudgetExceeded);
            }
        }
        Ok(())
    }

    /// 累加出口字节（body 流式传输时逐 frame 调用）；未配置预算时为 no-op
    pub fn add_egress(&self, bytes: u64) {
        if self.max_egress_bytes.is_some() {
            self.egress_used.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// 是否需要统计出口字节（决定是否给 body 注入计数器）
    pub fn tracks_egress(&self) -> bool {
        self.max_egress_bytes.is_some()
    }

    /// 已用出口字节（测试 / 遥测用）
    pub fn egress_used(&self) -> u64 {
        self.egress_used.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_limits_admits_all() {
        let b = Budget::new(None, None);
        assert!(b.check_admission().is_ok());
        b.add_egress(1000); // 无预算时不计
        assert_eq!(b.egress_used(), 0);
        assert!(!b.tracks_egress());
    }

    #[test]
    fn test_egress_budget_blocks_when_exhausted() {
        let b = Budget::new(Some(1000), None);
        assert!(b.check_admission().is_ok());
        b.add_egress(1000);
        assert_eq!(b.egress_used(), 1000);
        assert!(
            matches!(b.check_admission(), Err(crate::ProxyError::BudgetExceeded)),
            "达到上限后应拒绝"
        );
    }

    #[test]
    fn test_rate_limit_bucket() {
        // rps=2：桶初始 2 个令牌，连续三次准入前两次通过、第三次被限
        let b = Budget::new(None, Some(2));
        assert!(b.check_admission().is_ok());
        assert!(b.check_admission().is_ok());
        assert!(
            matches!(b.check_admission(), Err(crate::ProxyError::RateLimited)),
            "令牌耗尽应限速"
        );
    }
}
