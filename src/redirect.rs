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

        // N3：只对明确「换个 URL 重发」语义的状态码跟随。
        // 304 Not Modified、300 Multiple Choices、305/306 都不是重定向：
        // 尤其 304 带 Location 时若跟随，会吞掉条件请求（If-None-Match /
        // If-Modified-Since）并改成重新抓取，彻底破坏缓存语义。
        if !is_followable_redirect(status) {
            return Ok(RedirectAction::PassThrough);
        }

        // Location 必须存在且非空
        let location_str = match location {
            Some(loc) if !loc.is_empty() => loc,
            _ => return Ok(RedirectAction::PassThrough), // 缺失或空 Location
        };

        // N2：按 RFC 3986 解析 Location。无法解析成 http/https 绝对目标的
        // （畸形、协议相对到非法 host、非 http(s) scheme 等）按承诺原样返回 3xx，
        // 交给调用方处理，而不是抛出代理错误。
        let new_target = match parse_location(location_str, current_target) {
            Some(t) => t,
            None => return Ok(RedirectAction::PassThrough),
        };

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

/// 是否为可自动跟随的重定向状态码
///
/// 只有 `301/302/303/307/308` 表达「换个 URL 重发请求」。其余 3xx
/// （尤其 304）不跟随，原样返回给调用方。
fn is_followable_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// 按 RFC 3986 解析 Location header，得到绝对的 http/https 目标
///
/// 用 `url::Url::join` 处理绝对、协议相对（`//host/path`）、绝对路径
/// （`/path`）、相对引用（`next`、`../x`、`?q`）和大小写 scheme。
///
/// 返回 `None` 表示无法解析成 http/https 绝对目标（畸形、非 http(s) scheme、
/// 协议相对到非法 host 等），调用方据此原样返回 3xx。
///
/// 之所以能可靠工作，前提是 `current.full_url()` 是合法 URL——这依赖
/// IPv6 authority 的方括号处理（见 `Target::authority`）。
fn parse_location(location: &str, current: &Target) -> Option<Target> {
    let base = url::Url::parse(&current.full_url()).ok()?;
    let joined = base.join(location).ok()?;

    // 只接受 http/https；mailto、data、javascript 等一律不跟随
    match joined.scheme() {
        "http" | "https" => {}
        _ => return None,
    }

    // 复用 parse_target 做协议/端口/host 的完整校验与规范化。
    // parse_target 期望带一个前导 `/`。
    crate::target::parse_target(&format!("/{joined}")).ok()
}

/// 判断是否跨 origin
///
/// origin = scheme + host + port
fn is_cross_origin(a: &Target, b: &Target) -> bool {
    a.scheme != b.scheme || a.host != b.host || a.port != b.port
}

/// 生成 canonical URL 字符串（用于环检测）
///
/// 必须包含 path 与 query：环检测的语义是"同一个完整 URL 被访问两次"，
/// 而不是"同一个 origin 被访问两次"。只用 scheme://authority 会把同源换路径的
/// 合法重定向（如 `/old` → `/new`、尾斜线归一化）误判为环并返回 403。
fn canonical_url(target: &Target) -> String {
    target.full_url()
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
            path: "/".into(),
            query: String::new(),
        }
    }

    #[allow(dead_code)]
    fn http_target(host: &str, port: u16) -> Target {
        Target {
            scheme: Scheme::Http,
            host: Host::Domain(host.into()),
            port,
            path: "/".into(),
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

    /// E1 回归：同源换路径的重定向不能被误判为环
    ///
    /// proxy.rs 会先 visit 初始 target，随后 handle_redirect 再 visit 新 target。
    /// 修复前 canonical_url 只含 scheme://authority，同源换路径会与初始 URL 撞 key，
    /// 导致合法的 `/old` → `/new`、尾斜线归一化等重定向被误判为环返回 403。
    #[test]
    fn test_same_host_redirect_not_false_loop() {
        let mut rm = RedirectMachine::new();
        let initial = https_target("example.com", 443); // path "/"
        rm.visit(&initial); // 模拟 proxy.rs:forward_request 的首次记录

        let action = rm
            .handle_redirect(Method::Get, 301, Some("https://example.com/new"), &initial)
            .unwrap();
        assert!(
            matches!(action, RedirectAction::Follow { .. }),
            "同源换路径的重定向应跟随，而不是被误判为环"
        );
    }

    /// E1 回归：同源真环（回到已访问的完整 URL）仍必须被检测
    #[test]
    fn test_same_host_true_loop_detected() {
        let mut rm = RedirectMachine::new();
        let initial = https_target("example.com", 443); // path "/"
        rm.visit(&initial);

        // /  -> /page
        let action = rm
            .handle_redirect(Method::Get, 302, Some("https://example.com/page"), &initial)
            .unwrap();
        let page = match action {
            RedirectAction::Follow { target, .. } => target,
            _ => panic!("expected Follow"),
        };

        // /page -> /（回到初始完整 URL，构成环）
        let result = rm.handle_redirect(Method::Get, 302, Some("https://example.com/"), &page);
        assert!(result.is_err(), "回到已访问的完整 URL 应检测为环");
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

    /// N2 回归：无前导斜线的相对 Location 不改写主机名
    ///
    /// 修复前字符串拼接会得到 `example.comnext`（主机名被改写）。
    #[test]
    fn test_relative_no_slash_keeps_host() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/dir/page").unwrap();
        let action = rm
            .handle_redirect(Method::Get, 302, Some("next"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.host, Host::Domain("example.com".into()));
                assert_eq!(target.path, "/dir/next");
            }
            _ => panic!("应跟随到 example.com/dir/next"),
        }
    }

    /// N2 回归：协议相对 Location 解析到新 host，而非拼在当前 host 后
    #[test]
    fn test_protocol_relative_location() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/dir/page").unwrap();
        let action = rm
            .handle_redirect(Method::Get, 302, Some("//other.com/x"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.host, Host::Domain("other.com".into()));
                assert_eq!(target.scheme, Scheme::Https); // 继承当前 scheme
            }
            _ => panic!("协议相对应解析到 other.com"),
        }
    }

    /// N2 回归：纯 query 的 Location 保留原 path
    #[test]
    fn test_query_only_location_keeps_path() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/dir/page").unwrap();
        let action = rm
            .handle_redirect(Method::Get, 302, Some("?only=query"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.path, "/dir/page");
                assert_eq!(target.query, "?only=query");
            }
            _ => panic!("纯 query 应保留 path"),
        }
    }

    /// N2 回归：非 http(s) scheme 的 Location 原样返回 3xx，不跟随也不报错
    #[test]
    fn test_non_http_location_passthrough() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/").unwrap();
        let action = rm
            .handle_redirect(Method::Get, 302, Some("mailto:a@b.com"), &current)
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    /// N2 回归：大写 scheme 的绝对 Location 被规范化后正常跟随
    #[test]
    fn test_uppercase_scheme_location() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/").unwrap();
        let action = rm
            .handle_redirect(Method::Get, 301, Some("HTTPS://other.com/A"), &current)
            .unwrap();
        match action {
            RedirectAction::Follow { target, .. } => {
                assert_eq!(target.host, Host::Domain("other.com".into()));
                assert_eq!(target.path, "/A");
            }
            _ => panic!("大写 scheme 应规范化后跟随"),
        }
    }

    /// N3 回归：304 Not Modified 即便带 Location 也不跟随
    #[test]
    fn test_304_not_followed() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/").unwrap();
        let action = rm
            .handle_redirect(
                Method::Get,
                304,
                Some("https://example.com/other"),
                &current,
            )
            .unwrap();
        assert_eq!(action, RedirectAction::PassThrough);
    }

    /// N3 回归：300 / 305 / 306 均不跟随
    #[test]
    fn test_300_305_306_not_followed() {
        let current = crate::target::parse_target("/https://example.com/").unwrap();
        for status in [300u16, 305, 306] {
            let mut rm = RedirectMachine::new();
            let action = rm
                .handle_redirect(
                    Method::Get,
                    status,
                    Some("https://example.com/other"),
                    &current,
                )
                .unwrap();
            assert_eq!(
                action,
                RedirectAction::PassThrough,
                "status {status} 不应跟随"
            );
        }
    }

    /// 303 See Other 仍跟随
    #[test]
    fn test_303_followed() {
        let mut rm = RedirectMachine::new();
        let current = crate::target::parse_target("/https://example.com/").unwrap();
        let action = rm
            .handle_redirect(
                Method::Get,
                303,
                Some("https://example.com/other"),
                &current,
            )
            .unwrap();
        assert!(matches!(action, RedirectAction::Follow { .. }));
    }
}
