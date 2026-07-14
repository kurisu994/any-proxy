//! 重定向状态机与逐跳复查
//!
//! 参见 DESIGN.md Section 5（Redirect policy）。
//!
//! # 规则
//!
//! - 最多跟随 10 跳，维护 canonical URL 集合并拒绝重定向环
//! - 阻止 HTTPS 降级到 HTTP
//! - 只对 `GET` 与 `HEAD` 自动跟随
//! - 301/302/307/308 保持 GET 或 HEAD；303 对 GET 保持 GET、对 HEAD 保持 HEAD
//! - POST/PUT/PATCH/DELETE 收到任何 3xx 时不跟随，直接返回给调用方
//! - 相对 `Location` 按当前 URL 解析；缺失或非法 `Location` 的 3xx 原样返回
//! - 每一跳从 Target validation 第 1 步重新开始
//! - 跨 origin 时删除 `Authorization`；Cookie 本来就不转发

use std::collections::HashSet;

use crate::target::{Scheme, Target};

/// 重定向跟随决策
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectAction {
    /// 跟随重定向到新 URL
    Follow {
        /// 新的目标 URL
        target: Target,
        /// 是否删除 Authorization（跨 origin 时为 true）
        strip_authorization: bool,
    },
    /// 不跟随，原样返回 3xx 给调用方
    PassThrough,
}

/// 重定向状态机
#[derive(Debug, Clone)]
pub struct RedirectMachine {
    /// 已访问的 canonical URL 集合（用于环检测）
    visited: HashSet<String>,
    /// 当前跳数
    hop_count: usize,
    /// 最大跳数
    max_hops: usize,
}

impl RedirectMachine {
    /// 创建新的重定向状态机，最大 10 跳
    pub fn new() -> Self {
        Self {
            visited: HashSet::new(),
            hop_count: 0,
            max_hops: 10,
        }
    }

    /// 用指定的最大跳数创建
    pub fn with_max_hops(max_hops: usize) -> Self {
        Self {
            visited: HashSet::new(),
            hop_count: 0,
            max_hops,
        }
    }

    /// 记录当前 URL 并检查是否已访问（环检测）
    ///
    /// 返回 `true` 表示 URL 未访问过（可以继续），`false` 表示检测到环。
    pub fn visit(&mut self, target: &Target) -> bool {
        let key = canonical_url(target);
        if self.visited.contains(&key) {
            return false; // 环检测
        }
        self.visited.insert(key);
        true
    }

    /// 处理 3xx 响应，决定是否跟随
    ///
    /// `method` 为当前请求方法；`status` 为 3xx 状态码；`location` 为 Location header 值。
    /// `current_target` 为当前请求的 Target。
    pub fn handle_redirect(
        &mut self,
        method: Method,
        status: u16,
        location: Option<&str>,
        current_target: &Target,
    ) -> Result<RedirectAction, crate::ProxyError> {
        // 带 Body 的 method 不跟随
        if !matches!(method, Method::Get | Method::Head) {
            return Ok(RedirectAction::PassThrough);
        }

        // 状态码必须是 3xx
        if !(300..400).contains(&status) {
            return Ok(RedirectAction::PassThrough);
        }

        // Location 必须存在且非空
        let location_str = match location {
            Some(loc) if !loc.is_empty() => loc,
            _ => return Ok(RedirectAction::PassThrough), // 缺失或空 Location
        };

        // 解析 Location
        let new_target = parse_location(location_str, current_target)?;

        // HTTPS 降级检查
        if current_target.scheme == Scheme::Https && new_target.scheme == Scheme::Http {
            return Err(crate::ProxyError::TargetBlocked {
                message: "HTTPS 降级到 HTTP 不允许".into(),
            });
        }

        // 跳数检查
        self.hop_count += 1;
        if self.hop_count > self.max_hops {
            return Err(crate::ProxyError::TargetBlocked {
                message: format!("超过最大重定向跳数 {}", self.max_hops),
            });
        }

        // 环检测
        if !self.visit(&new_target) {
            return Err(crate::ProxyError::TargetBlocked {
                message: "检测到重定向环".into(),
            });
        }

        // 跨 origin 判定（scheme + host + port）
        let strip_auth = is_cross_origin(current_target, &new_target);

        Ok(RedirectAction::Follow {
            target: new_target,
            strip_authorization: strip_auth,
        })
    }

    /// 当前跳数
    pub fn hop_count(&self) -> usize {
        self.hop_count
    }
}

impl Default for RedirectMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP 方法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

/// 解析 Location header
///
/// 相对 Location 按当前 URL 解析。
/// 缺失或非法 Location 返回错误（调用方原样返回 3xx）。
fn parse_location(location: &str, current: &Target) -> Result<Target, crate::ProxyError> {
    // 如果是绝对 URL，直接解析
    if location.starts_with("http://") || location.starts_with("https://") {
        return crate::target::parse_target(&format!("/{location}"));
    }

    // 相对 URL：按当前 URL 的 scheme + host + port 拼接
    let absolute = format!("{}://{}{}", current.scheme, current.authority(), location);
    crate::target::parse_target(&format!("/{absolute}"))
}

/// 判断是否跨 origin
///
/// origin = scheme + host + port
fn is_cross_origin(a: &Target, b: &Target) -> bool {
    a.scheme != b.scheme || a.host != b.host || a.port != b.port
}

/// 生成 canonical URL 字符串（用于环检测）
fn canonical_url(target: &Target) -> String {
    format!("{}://{}", target.scheme, target.authority())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Host;

    fn https_target(host: &str, port: u16) -> Target {
        Target {
            scheme: Scheme::Https,
            host: Host::Domain(host.into()),
            port,
            query: String::new(),
        }
    }

    #[allow(dead_code)]
    fn http_target(host: &str, port: u16) -> Target {
        Target {
            scheme: Scheme::Http,
            host: Host::Domain(host.into()),
            port,
            query: String::new(),
        }
    }

    #[test]
    fn test_get_follow_301() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Get, 301, Some("https://other.com/"), &current)
            .unwrap();
        assert!(matches!(action, RedirectAction::Follow { .. }));
    }

    #[test]
    fn test_head_follow_308() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Head, 308, Some("https://other.com/"), &current)
            .unwrap();
        assert!(matches!(action, RedirectAction::Follow { .. }));
    }

    #[test]
    fn test_post_not_follow() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Post, 301, Some("https://other.com/"), &current)
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    #[test]
    fn test_put_not_follow() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Put, 302, Some("https://other.com/"), &current)
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    #[test]
    fn test_https_downgrade_blocked() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let result = rm.handle_redirect(Method::Get, 301, Some("http://other.com/"), &current);
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_location_passthrough() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Get, 302, None, &current)
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    #[test]
    fn test_empty_location_passthrough() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Get, 302, Some(""), &current)
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    #[test]
    fn test_redirect_loop_detected() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        rm.visit(&current); // 记录初始 URL

        // 第一跳：example.com -> other.com
        let action1 = rm
            .handle_redirect(Method::Get, 301, Some("https://other.com/"), &current)
            .unwrap();
        let target1 = match action1 {
            RedirectAction::Follow { target, .. } => target,
            _ => panic!("expected Follow"),
        };

        // 第二跳：other.com -> example.com（环）
        let result = rm.handle_redirect(Method::Get, 301, Some("https://example.com/"), &target1);
        assert!(result.is_err());
    }

    #[test]
    fn test_max_hops_exceeded() {
        let mut rm = RedirectMachine::with_max_hops(2);
        let current = https_target("a.com", 443);

        // 第 1 跳
        let action1 = rm
            .handle_redirect(Method::Get, 301, Some("https://b.com/"), &current)
            .unwrap();
        let target1 = match action1 {
            RedirectAction::Follow { target, .. } => target,
            _ => panic!("expected Follow"),
        };

        // 第 2 跳
        let action2 = rm
            .handle_redirect(Method::Get, 301, Some("https://c.com/"), &target1)
            .unwrap();
        let target2 = match action2 {
            RedirectAction::Follow { target, .. } => target,
            _ => panic!("expected Follow"),
        };

        // 第 3 跳：超过 max_hops=2
        let result = rm.handle_redirect(Method::Get, 301, Some("https://d.com/"), &target2);
        assert!(result.is_err());
    }

    #[test]
    fn test_exactly_10_hops_allowed() {
        let mut rm = RedirectMachine::new(); // max_hops=10
        let mut current = https_target("a0.com", 443);

        for i in 0..10 {
            let next = format!("https://a{i}.com/");
            let action = rm
                .handle_redirect(Method::Get, 301, Some(&next), &current)
                .unwrap();
            current = match action {
                RedirectAction::Follow { target, .. } => target,
                _ => panic!("expected Follow at hop {}", i + 1),
            };
        }
        assert_eq!(rm.hop_count(), 10);
    }

    #[test]
    fn test_11th_hop_rejected() {
        let mut rm = RedirectMachine::new();
        let mut current = https_target("a0.com", 443);

        for i in 0..10 {
            let next = format!("https://a{i}.com/");
            let action = rm
                .handle_redirect(Method::Get, 301, Some(&next), &current)
                .unwrap();
            current = match action {
                RedirectAction::Follow { target, .. } => target,
                _ => panic!("expected Follow"),
            };
        }

        // 第 11 跳
        let result = rm.handle_redirect(Method::Get, 301, Some("https://a10.com/"), &current);
        assert!(result.is_err());
    }

    #[test]
    fn test_cross_origin_strips_authorization() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Get, 301, Some("https://other.com/"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow {
                strip_authorization,
                ..
            } => assert!(strip_authorization),
            _ => panic!("expected Follow"),
        }
    }

    #[test]
    fn test_same_origin_keeps_authorization() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(
                Method::Get,
                301,
                Some("https://example.com/new-path"),
                &current,
            )
            .unwrap();
        match action {
            RedirectAction::Follow {
                strip_authorization,
                ..
            } => assert!(!strip_authorization),
            _ => panic!("expected Follow"),
        }
    }

    #[test]
    fn test_relative_location() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 443);
        let action = rm
            .handle_redirect(Method::Get, 302, Some("/redirected"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.host, Host::Domain("example.com".into()));
                assert_eq!(target.scheme, Scheme::Https);
            }
            _ => panic!("expected Follow"),
        }
    }

    #[test]
    fn test_relative_location_with_port() {
        let mut rm = RedirectMachine::new();
        let current = https_target("example.com", 8443);
        let action = rm
            .handle_redirect(Method::Get, 302, Some("/redirected"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.port, 8443);
            }
            _ => panic!("expected Follow"),
        }
    }
}
