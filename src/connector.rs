//! 安全连接编排：resolve → validate → dial → 记录 peer_addr
//!
//! 参见 DESIGN.md Section 4（Target validation and connection pinning）。
//!
//! # 安全模型
//!
//! `Connector` 在一次调用中完成 DNS 解析、全量地址校验、选择 `SocketAddr` 与
//! `TcpStream::connect`，并记录 `peer_addr()`。测试注入假 Resolver 与假 Dialer，
//! 断言策略允许集合与实际 dial 地址完全一致。
//!
//! M0 默认关闭上游连接池，每次请求都走一次完整的 resolve → validate → dial。

use std::net::SocketAddr;
use std::sync::Arc;

use crate::resolver::{AddressPolicy, Resolver};
use crate::target::Target;

/// TCP 连接 trait（可注入测试替身）
///
/// 实现者连接到指定的 `SocketAddr`，返回连接记录。
#[trait_variant::make(Send)]
pub trait Dialer {
    /// 连接到指定地址
    fn dial(
        &self,
        addr: SocketAddr,
    ) -> impl std::future::Future<Output = Result<DialRecord, crate::ProxyError>> + Send;
}

/// dial 记录：记录实际连接的 peer 地址
#[derive(Debug, Clone)]
pub struct DialRecord {
    /// 实际连接的 peer 地址
    pub peer_addr: SocketAddr,
}

/// 安全连接编排器
///
/// 持有 `Resolver`、`AddressPolicy` 和 `Dialer` 三个可注入依赖。
/// 在一次调用中原子完成 resolve → validate → dial。
#[derive(Debug, Clone)]
pub struct Connector<R, D> {
    resolver: Arc<R>,
    policy: Arc<AddressPolicy>,
    dialer: Arc<D>,
}

impl<R, D> Connector<R, D>
where
    R: Resolver,
    D: Dialer,
{
    /// 创建新的 Connector
    pub fn new(resolver: R, policy: AddressPolicy, dialer: D) -> Self {
        Self {
            resolver: Arc::new(resolver),
            policy: Arc::new(policy),
            dialer: Arc::new(dialer),
        }
    }

    /// 安全连接：resolve → validate → select → dial → 记录 peer_addr
    ///
    /// 任何策略失败都在建立上游连接前返回错误（零次 dial）。
    /// 允许目标的实际 dial peer 必须属于同一次 Resolver 返回且完成全量校验的地址集合。
    pub async fn connect(&self, target: &Target) -> Result<Connection, crate::ProxyError> {
        // 1. 解析目标主机名
        let resolve_result = self.resolver.resolve(&target.host.as_str()).await?;

        // 2. 全量校验所有地址
        let validated = self.policy.validate_all(&resolve_result.addresses)?;

        // 3. 选择 SocketAddr（使用 target 端口）
        let addrs: Vec<SocketAddr> = validated
            .iter()
            .map(|ip| SocketAddr::new(*ip, target.port))
            .collect();

        if addrs.is_empty() {
            return Err(crate::ProxyError::DnsFailed {
                message: "DNS 返回空答案".into(),
            });
        }

        // 4. dial 到第一个验证过的地址
        let dial_record = self.dialer.dial(addrs[0]).await?;

        // 5. 验证 peer_addr 属于已验证集合
        let validated_set: HashSet<SocketAddr> = addrs.iter().copied().collect();
        if !validated_set.contains(&dial_record.peer_addr) {
            return Err(crate::ProxyError::ConnectFailed {
                message: format!("peer_addr {} 不属于已验证地址集合", dial_record.peer_addr),
            });
        }

        Ok(Connection {
            peer_addr: dial_record.peer_addr,
            target: target.clone(),
        })
    }
}

use std::collections::HashSet;

/// 建立的连接
#[derive(Debug, Clone)]
pub struct Connection {
    /// 实际连接的 peer 地址
    pub peer_addr: SocketAddr,
    /// 目标信息
    pub target: Target,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolveResult;
    use std::net::IpAddr;

    /// 假 Resolver：返回预设的 IP 地址
    struct FakeResolver {
        addresses: Vec<IpAddr>,
        fail: bool,
    }

    impl FakeResolver {
        fn new(addresses: Vec<IpAddr>) -> Self {
            Self {
                addresses,
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                addresses: vec![],
                fail: true,
            }
        }
    }

    impl Resolver for FakeResolver {
        async fn resolve(&self, _host: &str) -> Result<ResolveResult, crate::ProxyError> {
            if self.fail {
                return Err(crate::ProxyError::DnsFailed {
                    message: "fake resolver failed".into(),
                });
            }
            Ok(ResolveResult {
                addresses: self.addresses.clone(),
            })
        }
    }

    /// 假 Dialer：记录 dial 地址，不实际连接
    #[derive(Clone)]
    struct FakeDialer {
        records: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    }

    impl FakeDialer {
        fn new() -> Self {
            Self {
                records: Arc::new(std::sync::Mutex::new(vec![])),
            }
        }

        fn dial_records(&self) -> Vec<SocketAddr> {
            self.records.lock().unwrap().clone()
        }
    }

    impl Dialer for FakeDialer {
        async fn dial(&self, addr: SocketAddr) -> Result<DialRecord, crate::ProxyError> {
            self.records.lock().unwrap().push(addr);
            Ok(DialRecord { peer_addr: addr })
        }
    }

    fn make_target(port: u16) -> Target {
        Target {
            scheme: crate::target::Scheme::Https,
            host: crate::target::Host::Domain("example.com".into()),
            port,
            query: String::new(),
        }
    }

    #[tokio::test]
    async fn test_normal_connect() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let conn = connector.connect(&make_target(443)).await.unwrap();
        assert_eq!(conn.peer_addr, "1.2.3.4:443".parse().unwrap());
        assert_eq!(dialer.dial_records(), vec!["1.2.3.4:443".parse().unwrap()]);
    }

    #[tokio::test]
    async fn test_dangerous_target_zero_dial() {
        let resolver = FakeResolver::new(vec!["10.0.0.1".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_target(443)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty()); // 零次 dial
    }

    #[tokio::test]
    async fn test_mixed_a_aaaa_rejects_all() {
        let resolver = FakeResolver::new(vec![
            "1.2.3.4".parse().unwrap(),  // 公网
            "10.0.0.1".parse().unwrap(), // 私网
        ]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_target(443)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty()); // 零次 dial
    }

    #[tokio::test]
    async fn test_dns_failure() {
        let resolver = FakeResolver::failing();
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_target(443)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty());
    }

    #[tokio::test]
    async fn test_dns_empty_answer() {
        let resolver = FakeResolver::new(vec![]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer.clone());

        let result = connector.connect(&make_target(443)).await;
        assert!(result.is_err());
        assert!(dialer.dial_records().is_empty());
    }

    #[tokio::test]
    async fn test_peer_addr_in_validated_set() {
        let resolver = FakeResolver::new(vec!["1.2.3.4".parse().unwrap()]);
        let policy = AddressPolicy::new();
        let dialer = FakeDialer::new();
        let connector = Connector::new(resolver, policy, dialer);

        let conn = connector.connect(&make_target(8443)).await.unwrap();
        assert_eq!(conn.peer_addr.port(), 8443);
    }
}
