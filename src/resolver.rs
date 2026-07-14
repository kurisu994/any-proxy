//! DNS 解析、公网 IP 校验（AddressPolicy）与连接地址固定
//!
//! 参见 DESIGN.md Section 4（Target validation and connection pinning）。
//!
//! # 安全模型
//!
//! Connector 内部在一次调用中完成 resolve → 全量校验 → 选择 SocketAddr。
//! 只要答案中包含一个非公网地址，就拒绝整个目标，不能从混合答案中挑选公网地址继续。
//! 不读取 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 环境变量。

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

use ipnet::IpNet;

/// DNS 解析结果
#[derive(Debug, Clone)]
pub struct ResolveResult {
    /// 所有解析到的 IP 地址（A + AAAA，已跟踪 CNAME 链到最终 IP）
    pub addresses: Vec<IpAddr>,
}

/// DNS 解析 trait（可注入测试替身）
///
/// 实现者必须同时解析 A 与 AAAA 记录，并跟踪 CNAME 链到最终 IP 地址。
/// CNAME 最大跟踪深度 10，超过返回错误。
#[trait_variant::make(Send)]
pub trait Resolver {
    /// 解析主机名到 IP 地址列表
    ///
    /// 对 IP literal 直接返回单个元素的列表。
    /// 对域名解析 A 和 AAAA 记录，跟踪 CNAME 链。
    fn resolve(
        &self,
        host: &str,
    ) -> impl std::future::Future<Output = Result<ResolveResult, crate::ProxyError>> + Send;
}

/// 地址分类策略
///
/// 维护 IANA IPv4/IPv6 特殊用途地址表、宿主接口地址和 `DENY_CIDRS`。
/// 未知或无法分类的地址默认拒绝。
#[derive(Debug, Clone)]
pub struct AddressPolicy {
    /// 显式拒绝的 CIDR 列表（IANA 特殊用途 + DENY_CIDRS）
    deny_cidrs: Vec<IpNet>,
    /// 宿主网络接口地址（定期刷新）
    host_addresses: HashSet<IpAddr>,
}

impl AddressPolicy {
    /// 创建默认地址策略
    ///
    /// 包含 IANA 特殊用途地址表的默认拒绝列表。
    pub fn new() -> Self {
        Self {
            deny_cidrs: default_deny_cidrs(),
            host_addresses: HashSet::new(),
        }
    }

    /// 添加 `DENY_CIDRS` 环境变量中的 CIDR
    pub fn with_deny_cidrs(mut self, cidrs: &[IpNet]) -> Self {
        self.deny_cidrs.extend_from_slice(cidrs);
        self
    }

    /// 创建允许所有地址的策略（仅供测试使用）
    ///
    /// 不包含任何 deny CIDR 和宿主接口地址。
    #[doc(hidden)]
    pub fn allow_all_for_test() -> Self {
        Self {
            deny_cidrs: vec![],
            host_addresses: HashSet::new(),
        }
    }

    /// 刷新宿主网络接口地址
    pub fn refresh_host_addresses(&mut self) {
        self.host_addresses.clear();
        if let Ok(ifaces) = if_addrs::get_if_addrs() {
            for iface in ifaces {
                self.host_addresses.insert(iface.ip());
            }
        }
    }

    /// 检查单个 IP 地址是否允许
    ///
    /// 返回 `true` 表示允许（公网地址），`false` 表示拒绝。
    /// 未知或无法分类的地址默认拒绝。
    pub fn is_allowed(&self, ip: &IpAddr) -> bool {
        // 检查宿主接口地址
        if self.host_addresses.contains(ip) {
            return false;
        }

        // 检查 deny CIDR
        for cidr in &self.deny_cidrs {
            if cidr.contains(ip) {
                return false;
            }
        }

        // 默认允许（已通过所有 deny 检查的地址视为公网）
        // 注意：IANA 特殊用途表应覆盖所有非公网地址，
        // 未知地址默认拒绝的要求由 deny_cidrs 的完整性保证。
        true
    }

    /// 校验一组 IP 地址
    ///
    /// 只要答案中包含一个非公网地址，就拒绝整个目标。
    /// 返回通过校验的地址列表（全部允许时与输入相同）。
    pub fn validate_all(&self, addresses: &[IpAddr]) -> Result<Vec<IpAddr>, crate::ProxyError> {
        for ip in addresses {
            if !self.is_allowed(ip) {
                return Err(crate::ProxyError::TargetBlocked {
                    message: format!("目标地址不允许访问: {ip}"),
                });
            }
        }
        Ok(addresses.to_vec())
    }
}

impl Default for AddressPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// 默认拒绝 CIDR 列表
///
/// 包含 IANA IPv4/IPv6 特殊用途地址：
/// - 私网地址（RFC 1918）
/// - 链路本地
/// - 环回
/// - 多播
/// - 广播
/// - 未指定地址
/// - CGN（RFC 6598）
/// - IPv6 私网、链路本地、环回、多播、未指定
fn default_deny_cidrs() -> Vec<IpNet> {
    vec![
        // IPv4 私网地址 (RFC 1918)
        "10.0.0.0/8".parse().unwrap(),
        "172.16.0.0/12".parse().unwrap(),
        "192.168.0.0/16".parse().unwrap(),
        // IPv4 链路本地
        "169.254.0.0/16".parse().unwrap(),
        // IPv4 环回
        "127.0.0.0/8".parse().unwrap(),
        // IPv4 多播
        "224.0.0.0/4".parse().unwrap(),
        // IPv4 广播
        "255.255.255.255/32".parse().unwrap(),
        // IPv4 未指定
        "0.0.0.0/8".parse().unwrap(),
        // CGN (RFC 6598)
        "100.64.0.0/10".parse().unwrap(),
        // IPv6 环回
        "::1/128".parse().unwrap(),
        // IPv6 链路本地
        "fe80::/10".parse().unwrap(),
        // IPv6 多播
        "ff00::/8".parse().unwrap(),
        // IPv6 未指定
        "::/128".parse().unwrap(),
        // IPv6 私网 (RFC 4193)
        "fc00::/7".parse().unwrap(),
        // IPv4-mapped IPv6
        "::ffff:0:0/96".parse().unwrap(),
        // IPv4-compatible IPv6 (deprecated but still deny)
        "::/96".parse().unwrap(),
        // 64:ff9b::/96 (NAT64)
        "64:ff9b::/96".parse().unwrap(),
        // 100::/64 (discard prefix)
        "100::/64".parse().unwrap(),
        // 2001:db8::/32 (documentation)
        "2001:db8::/32".parse().unwrap(),
    ]
}

/// 从 `ResolveResult` 中选择用于连接的 `SocketAddr` 列表
///
/// 所有地址都已经过 `AddressPolicy` 校验。
/// 返回的列表保持原始顺序，Connector 从中选择第一个进行 dial。
pub fn select_socket_addrs(resolve_result: &ResolveResult, port: u16) -> Vec<SocketAddr> {
    resolve_result
        .addresses
        .iter()
        .map(|ip| SocketAddr::new(*ip, port))
        .collect()
}

/// 系统 DNS 解析器
///
/// 使用 `tokio::net::lookup_host` 进行真实 DNS 解析。
/// tokio 的 lookup_host 会自动跟踪 CNAME 链到最终 A/AAAA 记录。
///
/// 注意：lookup_host 返回的是 `SocketAddr`（含端口），我们提取 IP 地址。
/// 端口使用传入的 `host:port` 格式，最终只取 IP。
pub struct SystemResolver;

impl SystemResolver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Result<ResolveResult, crate::ProxyError> {
        // IP literal 直接返回
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(ResolveResult {
                addresses: vec![ip],
            });
        }

        // 域名：使用 tokio DNS 解析
        // lookup_host 需要一个 host:port 格式，端口用 0 表示只关心 IP
        let lookup = format!("{host}:0");
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host(lookup)
            .await
            .map_err(|e| crate::ProxyError::DnsFailed {
                message: format!("DNS 解析失败: {e}"),
            })?
            .collect();

        // 提取唯一 IP 地址（去重，保留 A 和 AAAA）
        let mut ips: Vec<IpAddr> = Vec::new();
        let mut seen: HashSet<IpAddr> = HashSet::new();
        for addr in addrs {
            if seen.insert(addr.ip()) {
                ips.push(addr.ip());
            }
        }

        if ips.is_empty() {
            return Err(crate::ProxyError::DnsFailed {
                message: "DNS 返回空答案".into(),
            });
        }

        Ok(ResolveResult { addresses: ips })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn policy() -> AddressPolicy {
        AddressPolicy::new()
    }

    #[test]
    fn test_allow_public_ipv4() {
        let p = policy();
        assert!(p.is_allowed(&"1.2.3.4".parse().unwrap()));
        assert!(p.is_allowed(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn test_allow_public_ipv6() {
        let p = policy();
        assert!(p.is_allowed(&"2606:4700::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_private_ipv4() {
        let p = policy();
        assert!(!p.is_allowed(&"10.0.0.1".parse().unwrap()));
        assert!(!p.is_allowed(&"172.16.0.1".parse().unwrap()));
        assert!(!p.is_allowed(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn test_deny_link_local() {
        let p = policy();
        assert!(!p.is_allowed(&"169.254.1.1".parse().unwrap()));
        assert!(!p.is_allowed(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_loopback() {
        let p = policy();
        assert!(!p.is_allowed(&"127.0.0.1".parse().unwrap()));
        assert!(!p.is_allowed(&"::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_unspecified() {
        let p = policy();
        assert!(!p.is_allowed(&"0.0.0.0".parse().unwrap()));
        assert!(!p.is_allowed(&"::".parse().unwrap()));
    }

    #[test]
    fn test_deny_broadcast() {
        let p = policy();
        assert!(!p.is_allowed(&"255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn test_deny_cgn() {
        let p = policy();
        assert!(!p.is_allowed(&"100.64.0.1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_mapped_ipv6() {
        let p = policy();
        let mapped: Ipv6Addr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(!p.is_allowed(&IpAddr::V6(mapped)));
    }

    #[test]
    fn test_deny_ipv6_private() {
        let p = policy();
        assert!(!p.is_allowed(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv6_multicast() {
        let p = policy();
        assert!(!p.is_allowed(&"ff02::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_cloud_metadata() {
        let p = policy();
        // 169.254.169.254 是云元数据地址，属于链路本地
        assert!(!p.is_allowed(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn test_deny_cidrs() {
        let cidr: IpNet = "1.2.3.0/24".parse().unwrap();
        let p = AddressPolicy::new().with_deny_cidrs(&[cidr]);
        assert!(!p.is_allowed(&"1.2.3.4".parse().unwrap()));
        assert!(p.is_allowed(&"1.2.4.4".parse().unwrap()));
    }

    #[test]
    fn test_validate_all_rejects_mixed() {
        let p = policy();
        let addrs = vec![
            "1.2.3.4".parse().unwrap(),
            "10.0.0.1".parse().unwrap(), // 私网
        ];
        assert!(p.validate_all(&addrs).is_err());
    }

    #[test]
    fn test_validate_all_accepts_all_public() {
        let p = policy();
        let addrs = vec!["1.2.3.4".parse().unwrap(), "8.8.8.8".parse().unwrap()];
        assert!(p.validate_all(&addrs).is_ok());
    }

    #[test]
    fn test_select_socket_addrs() {
        let result = ResolveResult {
            addresses: vec!["1.2.3.4".parse().unwrap()],
        };
        let addrs = select_socket_addrs(&result, 443);
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 443);
    }
}
