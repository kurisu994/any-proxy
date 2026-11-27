//! 安全的 HTTP/1.1 Keep-Alive 上游连接池
//!
//! 仅复用已通过 [`crate::connector::Connector`] 完成「DNS 全量校验 → 固定 IP dial →
//! `peer_addr` 复核 → TLS 握手」的同源空闲连接，避免每次请求重复付出 DNS + TCP + TLS 握手延迟。
//!
//! # 安全边界
//!
//! - **按完整 origin 隔离**：PoolKey 为 `(is_tls, canonical_host, port)`，HTTP 与 HTTPS、
//!   不同域名或端口绝不共享连接，TLS SNI / 证书验证结论与 Key 严格绑定。
//! - **取出复核 `AddressPolicy`**：每次 `checkout` 与 `checkin` 都重新检查 `policy.is_allowed(&peer_addr.ip())`，
//!   宿主网卡地址刷新后不会继续复用落入新拒绝集合的旧连接。
//! - **最大存活时长 `max_lifetime`**：限制单条连接最长复用时间，强制周期性重新 DNS 解析。
//! - **仅在完整 EOF 后归还**：由 `IdleTimeoutBody` 的正常 EOF 回调触发 `checkin`；
//!   客户端中断、idle timeout、frame error、重定向中继或 WebSocket 升级连接绝不入池。
//! - **`idle_timeout` 只淘汰池内空闲连接**：绝不作用于正在传输数据的活跃流。
//! - **`max_idle_per_host = 0` 可一键关闭**：关闭时所有操作退化为 no-op，完全回退无池行为。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hyper::client::conn::http1::SendRequest;

use crate::config::Config;
use crate::resolver::AddressPolicy;
use crate::target::{canonical_host, Target};

/// 连接池查找键：严格按 `(is_tls, canonical_host, port)` 隔离
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub is_tls: bool,
    pub host: String,
    pub port: u16,
}

impl PoolKey {
    /// 从规范化目标构造连接池键
    pub fn from_target(target: &Target) -> Self {
        Self {
            is_tls: target.scheme.is_tls(),
            host: canonical_host(&target.host),
            port: target.port,
        }
    }
}

/// 连接池参数配置
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// 每个 host 最多保留的空闲连接数（0 = 关闭连接池）
    pub max_idle_per_host: usize,
    /// 全局最多保留的空闲连接数（0 = 关闭连接池）
    pub max_idle_total: usize,
    /// 池内空闲连接最大空闲时间
    pub idle_timeout: Duration,
    /// 单条连接最大存活时间（用于限制 DNS 变更陈旧窗口）
    pub max_lifetime: Duration,
}

impl PoolConfig {
    /// 从全局运行时配置提取连接池配置
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_idle_per_host: config.pool_max_idle_per_host,
            max_idle_total: config.pool_max_idle_total,
            idle_timeout: config.pool_idle_timeout,
            max_lifetime: config.pool_max_lifetime,
        }
    }
}

/// 池化的单条 HTTP/1.1 上游连接
pub struct PooledConnection {
    pub sender: SendRequest<axum::body::Body>,
    /// 建连时已验证的实际对端地址
    pub peer_addr: SocketAddr,
    /// 首次建连时间（用于 `max_lifetime` 控制）
    pub created_at: Instant,
    /// 最近一次归还入池的时间（用于 `idle_timeout` 淘汰）
    idle_since: Instant,
}

impl PooledConnection {
    /// 包装一条新建且已通过 Connector 安全校验的上游连接
    pub fn new(sender: SendRequest<axum::body::Body>, peer_addr: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            sender,
            peer_addr,
            created_at: now,
            idle_since: now,
        }
    }

    /// 供测试自定义时间戳构造池化连接
    #[cfg(test)]
    fn with_timestamps(
        sender: SendRequest<axum::body::Body>,
        peer_addr: SocketAddr,
        created_at: Instant,
        idle_since: Instant,
    ) -> Self {
        Self {
            sender,
            peer_addr,
            created_at,
            idle_since,
        }
    }
}

/// 进程级安全上游 HTTP/1.1 Keep-Alive 连接池
pub struct UpstreamPool {
    config: PoolConfig,
    /// 每个 origin 对应一个 LIFO 栈（尾部最新，优先复用最热连接）
    idle: Mutex<HashMap<PoolKey, Vec<PooledConnection>>>,
}

impl UpstreamPool {
    /// 按指定参数构造连接池
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            idle: Mutex::new(HashMap::new()),
        }
    }

    /// 从运行时 `Config` 构造连接池
    pub fn from_config(config: &Config) -> Self {
        Self::new(PoolConfig::from_config(config))
    }

    /// 连接池是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.max_idle_per_host > 0 && self.config.max_idle_total > 0
    }

    /// 尝试取出一条针对 `key` 的健康且通过策略复核的空闲连接（LIFO）
    ///
    /// 自动淘汰已关闭、超 `idle_timeout`、超 `max_lifetime` 或不再被 `policy` 允许的条目。
    pub fn checkout(&self, key: &PoolKey, policy: &AddressPolicy) -> Option<PooledConnection> {
        if !self.is_enabled() {
            return None;
        }
        let now = Instant::now();
        let mut map = self.idle.lock().ok()?;
        let bucket = map.get_mut(key)?;

        let mut chosen = None;
        while let Some(conn) = bucket.pop() {
            if self.is_usable(&conn, now, policy) {
                chosen = Some(conn);
                break;
            }
            // 失效条目直接随 drop 关闭底层连接
        }
        if bucket.is_empty() {
            map.remove(key);
        }
        chosen
    }

    /// 归还一条已完成完整请求-响应周期的连接
    ///
    /// 若池关闭、连接已关闭、超最大存活时间或策略已拒绝该 `peer_addr`，则直接丢弃。
    pub fn checkin(&self, key: PoolKey, mut conn: PooledConnection, policy: &AddressPolicy) {
        if !self.is_enabled() {
            return;
        }
        let now = Instant::now();
        if conn.sender.is_closed()
            || now.saturating_duration_since(conn.created_at) > self.config.max_lifetime
            || !policy.is_allowed(&conn.peer_addr.ip())
        {
            return;
        }

        let Ok(mut map) = self.idle.lock() else {
            return;
        };

        // 惰性清理全局已过期/已关闭连接，避免无后台 reaper 时堆积僵尸条目
        self.purge_stale_locked(&mut map, now);

        // 先检查该 host 是否已达单 host 上限：达上限则淘汰最老的一条为最新连接腾位
        if let Some(bucket) = map.get_mut(&key) {
            if bucket.len() >= self.config.max_idle_per_host {
                bucket.remove(0);
            }
        }

        // 检查全局总空闲上限：满则淘汰全局最久未用的一条
        let total: usize = map.values().map(Vec::len).sum();
        if total >= self.config.max_idle_total {
            self.evict_oldest_locked(&mut map);
        }

        let total_after: usize = map.values().map(Vec::len).sum();
        if total_after >= self.config.max_idle_total {
            return;
        }

        conn.idle_since = now;
        map.entry(key).or_default().push(conn);
    }

    /// 当前池内空闲连接总数（监控与测试用）
    pub fn idle_count(&self) -> usize {
        self.idle
            .lock()
            .map(|m| m.values().map(Vec::len).sum())
            .unwrap_or(0)
    }

    /// 判断池内条目是否仍然有效且合规
    fn is_usable(&self, conn: &PooledConnection, now: Instant, policy: &AddressPolicy) -> bool {
        !conn.sender.is_closed()
            && now.saturating_duration_since(conn.idle_since) <= self.config.idle_timeout
            && now.saturating_duration_since(conn.created_at) <= self.config.max_lifetime
            && policy.is_allowed(&conn.peer_addr.ip())
    }

    /// 清理所有桶中已关闭或超时的条目
    fn purge_stale_locked(&self, map: &mut HashMap<PoolKey, Vec<PooledConnection>>, now: Instant) {
        map.retain(|_, bucket| {
            bucket.retain(|c| {
                !c.sender.is_closed()
                    && now.saturating_duration_since(c.idle_since) <= self.config.idle_timeout
                    && now.saturating_duration_since(c.created_at) <= self.config.max_lifetime
            });
            !bucket.is_empty()
        });
    }

    /// 淘汰全局 `idle_since` 最早的一条连接
    fn evict_oldest_locked(&self, map: &mut HashMap<PoolKey, Vec<PooledConnection>>) {
        let mut oldest: Option<(PoolKey, usize, Instant)> = None;
        for (k, bucket) in map.iter() {
            for (idx, conn) in bucket.iter().enumerate() {
                if oldest.as_ref().is_none_or(|(_, _, t)| conn.idle_since < *t) {
                    oldest = Some((k.clone(), idx, conn.idle_since));
                }
            }
        }
        if let Some((key, idx, _)) = oldest {
            if let Some(bucket) = map.get_mut(&key) {
                bucket.remove(idx);
                if bucket.is_empty() {
                    map.remove(&key);
                }
            }
        }
    }
}

/// 根据上游响应版本与 `Connection` 头判断连接是否支持 Keep-Alive 复用
///
/// **必须在 `headers::clean_response_headers` 删除 `Connection` 头之前调用。**
pub fn is_response_keep_alive<B>(resp: &hyper::Response<B>) -> bool {
    let has_token = |needle: &str| {
        resp.headers()
            .get_all(http::header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case(needle)))
    };

    if has_token("close") {
        return false;
    }

    match resp.version() {
        http::Version::HTTP_11 => true,
        http::Version::HTTP_10 => has_token("keep-alive"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::IpNet;

    async fn make_sender() -> (
        SendRequest<axum::body::Body>,
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<()>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let io = hyper_util::rt::TokioIo::new(client);
        let (sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        let handle = tokio::spawn(async move {
            let _ = conn.await;
        });
        (sender, server, handle)
    }

    fn key(host: &str, port: u16, is_tls: bool) -> PoolKey {
        PoolKey {
            is_tls,
            host: host.to_string(),
            port,
        }
    }

    #[tokio::test]
    async fn test_pool_checkin_and_lifo_checkout() {
        let pool = UpstreamPool::new(PoolConfig {
            max_idle_per_host: 4,
            max_idle_total: 16,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(300),
        });
        let policy = AddressPolicy::new();
        let k = key("api.example.com", 443, true);

        let (s1, _srv1, _h1) = make_sender().await;
        let (s2, _srv2, _h2) = make_sender().await;
        let addr1: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let addr2: SocketAddr = "8.8.8.8:443".parse().unwrap();

        pool.checkin(k.clone(), PooledConnection::new(s1, addr1), &policy);
        pool.checkin(k.clone(), PooledConnection::new(s2, addr2), &policy);
        assert_eq!(pool.idle_count(), 2);

        // LIFO：最后放入的 addr2 先被取出
        let got1 = pool.checkout(&k, &policy).expect("应取出最新连接");
        assert_eq!(got1.peer_addr, addr2);
        let got2 = pool.checkout(&k, &policy).expect("应取出次新连接");
        assert_eq!(got2.peer_addr, addr1);
        assert!(pool.checkout(&k, &policy).is_none());
    }

    #[tokio::test]
    async fn test_pool_disabled_when_max_idle_zero() {
        let pool = UpstreamPool::new(PoolConfig {
            max_idle_per_host: 0,
            max_idle_total: 64,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(300),
        });
        assert!(!pool.is_enabled());
        let policy = AddressPolicy::new();
        let k = key("api.example.com", 443, true);
        let (s, _srv, _h) = make_sender().await;
        pool.checkin(
            k.clone(),
            PooledConnection::new(s, "1.1.1.1:443".parse().unwrap()),
            &policy,
        );
        assert_eq!(pool.idle_count(), 0);
        assert!(pool.checkout(&k, &policy).is_none());
    }

    #[tokio::test]
    async fn test_pool_evicts_expired_idle_and_max_lifetime() {
        let pool = UpstreamPool::new(PoolConfig {
            max_idle_per_host: 4,
            max_idle_total: 16,
            idle_timeout: Duration::from_secs(10),
            max_lifetime: Duration::from_secs(30),
        });
        let policy = AddressPolicy::new();
        let k = key("api.example.com", 443, true);
        let addr: SocketAddr = "1.1.1.1:443".parse().unwrap();

        let (s_idle_old, _srv1, _h1) = make_sender().await;
        let (s_life_old, _srv2, _h2) = make_sender().await;
        let now = Instant::now();
        {
            let mut map = pool.idle.lock().unwrap();
            map.entry(k.clone())
                .or_default()
                .push(PooledConnection::with_timestamps(
                    s_idle_old,
                    addr,
                    now - Duration::from_secs(15),
                    now - Duration::from_secs(15), // 超过 idle_timeout 10s
                ));
            map.entry(k.clone())
                .or_default()
                .push(PooledConnection::with_timestamps(
                    s_life_old,
                    addr,
                    now - Duration::from_secs(40), // 超过 max_lifetime 30s
                    now,
                ));
        }

        assert!(
            pool.checkout(&k, &policy).is_none(),
            "超过 idle_timeout 与 max_lifetime 的条目都必须被淘汰"
        );
        assert_eq!(pool.idle_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_checkout_revalidates_address_policy() {
        let pool = UpstreamPool::new(PoolConfig {
            max_idle_per_host: 4,
            max_idle_total: 16,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(300),
        });
        let open_policy = AddressPolicy::new();
        let k = key("api.example.com", 443, true);
        let addr: SocketAddr = "1.2.3.4:443".parse().unwrap();

        let (s, _srv, _h) = make_sender().await;
        pool.checkin(k.clone(), PooledConnection::new(s, addr), &open_policy);
        assert_eq!(pool.idle_count(), 1);

        // 策略收紧后（模拟宿主网卡刷新或 DENY_CIDRS 覆盖到该地址），取出时必须拒绝
        let deny_cidr: IpNet = "1.2.3.4/32".parse().unwrap();
        let strict_policy = AddressPolicy::new().with_deny_cidrs(&[deny_cidr]);
        assert!(
            pool.checkout(&k, &strict_policy).is_none(),
            "不再被 AddressPolicy 允许的池化连接必须拒绝复用"
        );
        assert_eq!(pool.idle_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_enforces_per_host_and_total_caps() {
        let pool = UpstreamPool::new(PoolConfig {
            max_idle_per_host: 2,
            max_idle_total: 3,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(300),
        });
        let policy = AddressPolicy::new();
        let k1 = key("a.example.com", 443, true);
        let k2 = key("b.example.com", 443, true);

        let mut keepers = Vec::new();
        for i in 1..=3u8 {
            let (s, srv, h) = make_sender().await;
            keepers.push((srv, h));
            let addr: SocketAddr = format!("1.1.1.{i}:443").parse().unwrap();
            pool.checkin(k1.clone(), PooledConnection::new(s, addr), &policy);
        }
        // 单 host 上限 2，淘汰最老的 1.1.1.1
        assert_eq!(pool.idle_count(), 2);

        for i in 1..=2u8 {
            let (s, srv, h) = make_sender().await;
            keepers.push((srv, h));
            let addr: SocketAddr = format!("8.8.8.{i}:443").parse().unwrap();
            pool.checkin(k2.clone(), PooledConnection::new(s, addr), &policy);
        }
        // 全局总上限 3
        assert_eq!(pool.idle_count(), 3);
    }

    #[test]
    fn test_is_response_keep_alive() {
        let r_h11 = hyper::Response::builder()
            .version(http::Version::HTTP_11)
            .body(())
            .unwrap();
        assert!(is_response_keep_alive(&r_h11));

        let r_h11_close = hyper::Response::builder()
            .version(http::Version::HTTP_11)
            .header("connection", "keep-alive, Close")
            .body(())
            .unwrap();
        assert!(!is_response_keep_alive(&r_h11_close));

        let r_h10 = hyper::Response::builder()
            .version(http::Version::HTTP_10)
            .body(())
            .unwrap();
        assert!(!is_response_keep_alive(&r_h10));

        let r_h10_ka = hyper::Response::builder()
            .version(http::Version::HTTP_10)
            .header("connection", "Keep-Alive")
            .body(())
            .unwrap();
        assert!(is_response_keep_alive(&r_h10_ka));
    }
}
