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
use std::sync::{Arc, RwLock};

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
/// 采用 **fail-closed** 策略：IPv6 仅允许 `2000::/3`（全局单播），
/// IPv4 通过穷举特殊用途 deny 列表覆盖所有非公网范围。
/// 未知或无法分类的地址默认拒绝。
#[derive(Debug, Clone)]
pub struct AddressPolicy {
    /// 显式拒绝的 CIDR 列表（IANA 特殊用途 + DENY_CIDRS）
    deny_cidrs: Vec<IpNet>,
    /// 宿主网络接口地址（定期刷新，使用 RwLock 支持后台刷新）
    host_addresses: Arc<RwLock<HashSet<IpAddr>>>,
}

impl AddressPolicy {
    /// 创建默认地址策略
    ///
    /// 包含 IANA 特殊用途地址表的默认拒绝列表。
    pub fn new() -> Self {
        Self {
            deny_cidrs: default_deny_cidrs(),
            host_addresses: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// 添加额外的拒绝 CIDR
    pub fn with_deny_cidrs(mut self, cidrs: &[IpNet]) -> Self {
        self.deny_cidrs.extend_from_slice(cidrs);
        self
    }

    /// 从环境变量 `DENY_CIDRS` 加载额外拒绝 CIDR
    ///
    /// 格式：逗号分隔的 CIDR 列表，例如 `10.0.0.0/8,172.16.0.0/12`。
    ///
    /// **fail-closed**：任一条目非法直接返回 `Err`，由调用方在启动时终止进程。
    /// 静默跳过会让运维笔误导致整条防护失效，与 fail-closed 产品定位冲突（N12）。
    pub fn with_env_deny_cidrs(mut self) -> Result<Self, String> {
        if let Ok(val) = std::env::var("DENY_CIDRS") {
            for cidr_str in val.split(',') {
                let trimmed = cidr_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match trimmed.parse::<IpNet>() {
                    Ok(cidr) => self.deny_cidrs.push(cidr),
                    Err(e) => {
                        return Err(format!("DENY_CIDRS 条目非法 `{trimmed}`: {e}"));
                    }
                }
            }
        }
        Ok(self)
    }

    /// 创建允许所有地址的策略（仅供测试使用）
    ///
    /// 不包含任何 deny CIDR 和宿主接口地址。
    #[doc(hidden)]
    pub fn allow_all_for_test() -> Self {
        Self {
            deny_cidrs: vec![],
            host_addresses: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// 刷新宿主网络接口地址
    ///
    /// 使用 `if_addrs` 枚举所有网络接口，替换当前 host_addresses。
    /// 通过 `RwLock` 实现并发安全，后台任务可定期调用此方法。
    pub fn refresh_host_addresses(&self) {
        // fail-closed：只有成功枚举接口才替换快照。
        // 枚举失败时保留上一次快照，避免用空集合覆盖后放行宿主公网接口地址（E7）。
        match if_addrs::get_if_addrs() {
            Ok(ifaces) => {
                let new_addrs: HashSet<IpAddr> = ifaces.into_iter().map(|i| i.ip()).collect();
                if let Ok(mut guard) = self.host_addresses.write() {
                    *guard = new_addrs;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "枚举宿主网络接口失败，保留上一次快照");
            }
        }
    }

    /// 启动宿主接口地址定期刷新的后台任务
    ///
    /// 立即刷新一次，然后每隔 `interval` 刷新一次。
    /// 返回 `JoinHandle` 供调用方管理任务生命周期。
    pub fn spawn_host_refresh(
        self: &Arc<Self>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let policy = Arc::clone(self);
        tokio::spawn(async move {
            // 立即刷新一次
            policy.refresh_host_addresses();
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // 跳过第一次（已经手动刷新）
            loop {
                ticker.tick().await;
                policy.refresh_host_addresses();
            }
        })
    }

    /// 检查单个 IP 地址是否允许
    ///
    /// 返回 `true` 表示允许（公网地址），`false` 表示拒绝。
    ///
    /// 实现策略为 **fail-closed**（默认拒绝）：
    /// - 先检查宿主接口地址和 deny CIDR 列表
    /// - 再检查地址是否属于已知公网范围（IPv4 全局单播排除特殊用途、IPv6 仅 `2000::/3`）
    /// - 未命中公网范围的地址一律拒绝
    ///
    /// 这满足 DESIGN.md 4.4 "默认拒绝未知或无法分类的地址" 的要求。
    pub fn is_allowed(&self, ip: &IpAddr) -> bool {
        // 检查宿主接口地址（读锁，不阻塞其他读取）
        if let Ok(host_addrs) = self.host_addresses.read() {
            if host_addrs.contains(ip) {
                return false;
            }
        }

        // 检查 deny CIDR
        for cidr in &self.deny_cidrs {
            if cidr.contains(ip) {
                return false;
            }
        }

        // fail-closed：只允许已知公网范围
        // IPv6：仅 2000::/3（全局单播）为公网
        // IPv4：不在 deny 列表中的剩余地址视为公网（deny 列表已覆盖所有特殊用途范围）
        match ip {
            IpAddr::V6(v6) => global_unicast_v6().contains(v6),
            IpAddr::V4(_) => true, // deny 列表已穷举 IPv4 特殊用途范围
        }
    }

    /// 校验一组 IP 地址
    ///
    /// 只要答案中包含一个非公网地址，就拒绝整个目标。
    /// 返回通过校验的地址列表（全部允许时与输入相同）。
    pub fn validate_all(&self, addresses: &[IpAddr]) -> Result<Vec<IpAddr>, crate::ProxyError> {
        for ip in addresses {
            if !self.is_allowed(ip) {
                // 被拒 IP 只进内部日志，不回显给调用方：错误消息会明文序列化，
                // 泄露解析结果等于对外提供 DNS 解析预言机（DESIGN §8，N5）。
                tracing::debug!(blocked_ip = %ip, "目标地址被地址策略拒绝");
                return Err(crate::ProxyError::TargetBlocked {
                    message: "目标解析到非公网地址，已拒绝".into(),
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

/// IPv6 全局单播地址范围（RFC 4291）
///
/// 只有 `2000::/3` 是公网可路由地址，其余一律拒绝。
/// 这是 fail-closed 策略的核心：不在该范围的 IPv6 地址默认拒绝。
static GLOBAL_UNICAST_V6: std::sync::OnceLock<ipnet::Ipv6Net> = std::sync::OnceLock::new();

/// 获取 IPv6 全局单播范围（首次调用时初始化）
fn global_unicast_v6() -> &'static ipnet::Ipv6Net {
    GLOBAL_UNICAST_V6.get_or_init(|| {
        ipnet::Ipv6Net::new(std::net::Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3)
            .expect("2000::/3 是合法的 IPv6 CIDR")
    })
}

/// 默认拒绝 CIDR 列表
///
/// 包含 IANA IPv4/IPv6 特殊用途地址（fail-closed 策略的显式拒绝部分）：
/// - IPv4：私网、链路本地、环回、多播、广播、未指定、CGN、保留、文档、基准测试、IETF 协议分配
/// - IPv6：环回、链路本地、多播、未指定、私网、IPv4-mapped、IPv4-compatible、NAT64、discard、文档
///
/// IPv6 的 fail-closed 由 `GLOBAL_UNICAST_V6` 正向白名单实现，不在此列表中穷举。
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
        // IPv4 保留（RFC 1112, Class E）
        "240.0.0.0/4".parse().unwrap(),
        // IPv4 文档/测试 (RFC 5737)
        "192.0.2.0/24".parse().unwrap(),
        "198.51.100.0/24".parse().unwrap(),
        "203.0.113.0/24".parse().unwrap(),
        // IPv4 基准测试 (RFC 2544)
        "198.18.0.0/15".parse().unwrap(),
        // IPv4 IETF 协议分配 (RFC 7534)
        "192.0.0.0/24".parse().unwrap(),
        // IPv4 6to4 中继任播（已废弃, RFC 7526）
        "192.88.99.0/24".parse().unwrap(),
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
        // 以下三段虽落在 2000::/3 全局单播白名单内，但会嵌入/转换到 IPv4，
        // 宿主配置对应路由时可形成条件式 SSRF，必须显式拒绝（E8）：
        // 6to4 (RFC 3056)：地址内嵌 IPv4，可指向私网
        "2002::/16".parse().unwrap(),
        // Teredo (RFC 4380)：隧道封装 IPv4
        "2001::/32".parse().unwrap(),
        // ORCHID (RFC 4843，已废弃)：非路由标识符
        "2001:10::/28".parse().unwrap(),
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
#[derive(Clone)]
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

    impl AddressPolicy {
        #[doc(hidden)]
        fn host_addresses_count(&self) -> usize {
            self.host_addresses.read().map(|g| g.len()).unwrap_or(0)
        }
    }

    #[test]
    fn test_refresh_host_addresses() {
        let p = AddressPolicy::new();
        assert_eq!(p.host_addresses_count(), 0);
        p.refresh_host_addresses();
        // 测试环境通常至少有一个网络接口
        assert!(p.host_addresses_count() > 0);
    }

    // 合并为单个测试：DENY_CIDRS 是进程级全局环境变量，
    // 拆成两个测试并行执行会相互污染，故顺序断言合法与非法两种情形。
    #[test]
    fn test_with_env_deny_cidrs() {
        // 合法条目：正常加载
        std::env::set_var("DENY_CIDRS", "1.2.3.0/24, 2001:db8::/32");
        let p = AddressPolicy::new().with_env_deny_cidrs().unwrap();
        assert!(!p.is_allowed(&"1.2.3.4".parse().unwrap()));
        assert!(p.is_allowed(&"1.2.4.4".parse().unwrap()));
        assert!(!p.is_allowed(&"2001:db8::1".parse().unwrap()));

        // 非法条目：fail-closed 返回 Err，而非静默跳过（N12）
        std::env::set_var("DENY_CIDRS", "not-a-cidr");
        let result = AddressPolicy::new().with_env_deny_cidrs();
        std::env::remove_var("DENY_CIDRS");
        assert!(result.is_err());
    }

    #[test]
    fn test_deny_6to4_teredo_orchid() {
        let p = policy();
        // 三段虽在 2000::/3 内，但因内嵌/转换 IPv4 被显式拒绝（E8）
        assert!(!p.is_allowed(&"2002:c0a8:0101::1".parse().unwrap())); // 6to4 → 192.168.1.1
        assert!(!p.is_allowed(&"2001:0:4137:9e76::1".parse().unwrap())); // Teredo
        assert!(!p.is_allowed(&"2001:10::1".parse().unwrap())); // ORCHID
    }

    #[test]
    fn test_fail_closed_ipv6_outside_2000() {
        let p = policy();
        // ::abcd 不在 2000::/3 范围内，应被拒绝
        assert!(!p.is_allowed(&"::abcd".parse().unwrap()));
        // ::2 不在 2000::/3 范围内
        assert!(!p.is_allowed(&"::2".parse().unwrap()));
        // 3fff::1 不在 2000::/3 范围内（3fff > 3fff... actually 2000::/3 covers 2000:: to 3fff:ffff:...）
        // 4000::1 不在 2000::/3 范围内
        assert!(!p.is_allowed(&"4000::1".parse().unwrap()));
    }

    #[test]
    fn test_fail_closed_ipv6_2000_range_allowed() {
        let p = policy();
        // 2000::/3 范围内的公网 IPv6 应允许
        assert!(p.is_allowed(&"2001:4860:4860::8888".parse().unwrap()));
        assert!(p.is_allowed(&"2606:4700::1".parse().unwrap()));
        assert!(p.is_allowed(&"3fff::1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_reserved() {
        let p = policy();
        // 240.0.0.0/4 保留地址
        assert!(!p.is_allowed(&"240.0.0.1".parse().unwrap()));
        assert!(!p.is_allowed(&"255.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_documentation() {
        let p = policy();
        assert!(!p.is_allowed(&"192.0.2.1".parse().unwrap()));
        assert!(!p.is_allowed(&"198.51.100.1".parse().unwrap()));
        assert!(!p.is_allowed(&"203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_benchmarking() {
        let p = policy();
        assert!(!p.is_allowed(&"198.18.0.1".parse().unwrap()));
        assert!(!p.is_allowed(&"198.19.255.255".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_ietf_protocol() {
        let p = policy();
        assert!(!p.is_allowed(&"192.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_deny_ipv4_6to4_deprecated() {
        let p = policy();
        assert!(!p.is_allowed(&"192.88.99.1".parse().unwrap()));
    }
}
