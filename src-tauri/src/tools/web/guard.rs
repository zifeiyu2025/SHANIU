//! 目标校验：scheme 白名单 + 私网/本机地址黑名单 + DNS 解析后校验。
//!
//! 这一层是 `web_search` 与 `web_fetch` 共用的安全入口，**不允许任何调用方跳过**：
//! 校验域名不够（`127.0.0.1.nip.io` 会解析到本机），必须在解析出真实 IP 之后再判一次。
//!
//! 解析器做成 trait 是为了测试：私网用例注入固定映射，不依赖外网 DNS。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use async_trait::async_trait;

/// 校验失败原因。每一条都能直接告诉模型「为什么抓不到」，不做笼统的 "invalid url"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// 非 http/https（file、ftp、data、javascript 等）。
    Scheme(String),
    /// URL 带用户名/密码，避免 `http://trusted@10.0.0.1/` 这类视觉欺骗。
    UserInfo,
    /// 解析到本机/私网/链路本地/保留地址。
    PrivateIp(IpAddr),
    /// 域名解析失败。
    Resolve(String),
    /// 解析成功但没有任何地址。
    NoAddress(String),
    /// URL 本身无法解析。
    InvalidUrl(String),
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheme(scheme) => write!(
                f,
                "仅允许 http/https，收到 `{scheme}`（file/ftp/data 等 scheme 一律拒绝）"
            ),
            Self::UserInfo => write!(f, "URL 不得包含用户名或密码"),
            Self::PrivateIp(ip) => write!(
                f,
                "目标解析到受保护地址 {ip}（本机/私网/链路本地/保留地址），已拒绝"
            ),
            Self::Resolve(message) => write!(f, "域名解析失败：{message}"),
            Self::NoAddress(host) => write!(f, "域名 {host} 没有解析到任何地址"),
            Self::InvalidUrl(message) => write!(f, "URL 无法解析：{message}"),
        }
    }
}

/// DNS 解析抽象。生产用 [`SystemResolver`]，测试注入固定表。
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// 系统解析器（`tokio::net::lookup_host`）。
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let addrs = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| error.to_string())?;
        Ok(addrs.map(|addr| addr.ip()).collect())
    }
}

/// 是否为禁止访问的 IPv4 地址。
///
/// 覆盖 RFC 6890 里「不可作为公网目标」的主要段，外加云元数据常用的
/// 链路本地段（169.254.0.0/16，含 169.254.169.254）。
pub fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224
}

/// 是否为禁止访问的 IPv6 地址（含 IPv4 映射与 6to4/NAT64 内嵌地址）。
pub fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(mapped);
    }
    let segments = ip.segments();
    // fc00::/7 唯一本地地址
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 链路本地
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // ff00::/8 组播
    if (segments[0] & 0xff00) == 0xff00 {
        return true;
    }
    // 2002::/16 6to4：中间嵌的是 IPv4，必须按 IPv4 规则再看一次
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_blocked_ipv4(embedded);
    }
    false
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// 校验 scheme、用户信息与主机存在性（不做 DNS）。
pub fn validate_url(url: &reqwest::Url) -> Result<(), GuardError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(GuardError::Scheme(other.to_string())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(GuardError::UserInfo);
    }
    if url.host_str().is_none() {
        return Err(GuardError::InvalidUrl("缺少主机名".into()));
    }
    Ok(())
}

/// 解析目标并把真实 IP 全部过一遍黑名单；任一命中即整体拒绝。
///
/// 为什么「任一命中」而不是「取第一个」：一个域名同时解析到公网与私网时，
/// 取第一个等于把结果交给 DNS 轮询的随机性，必须保守拒绝。
pub async fn resolve_checked(
    resolver: &dyn Resolver,
    url: &reqwest::Url,
) -> Result<Vec<IpAddr>, GuardError> {
    validate_url(url)?;
    let host = url.host_str().ok_or_else(|| GuardError::InvalidUrl("缺少主机名".into()))?;
    // 端口：显式端口优先，否则按 scheme 取默认值（只用于解析，不改变请求目标）。
    let port = url
        .port_or_known_default()
        .ok_or_else(|| GuardError::InvalidUrl("无法确定端口".into()))?;

    let ips = if let Ok(literal) = host.parse::<IpAddr>() {
        vec![literal]
    } else {
        resolver
            .resolve(host, port)
            .await
            .map_err(GuardError::Resolve)?
    };
    if ips.is_empty() {
        return Err(GuardError::NoAddress(host.to_string()));
    }
    if let Some(blocked) = ips.iter().find(|ip| is_blocked_ip(**ip)) {
        return Err(GuardError::PrivateIp(*blocked));
    }
    Ok(ips)
}

/// 与 [`resolve_checked`] 相同，但连 [`validate_url`] 的 scheme 检查一起做在开头；
/// 返回可直接用于 `ClientBuilder::resolve` 的地址。
pub async fn resolve_target(
    resolver: &dyn Resolver,
    url: &reqwest::Url,
) -> Result<Vec<SocketAddr>, GuardError> {
    // 先判 scheme，再取端口：`file://` 没有默认端口，先取端口会把
    // 「非法 scheme」误报成「无法确定端口」。
    validate_url(url)?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| GuardError::InvalidUrl("无法确定端口".into()))?;
    let ips = resolve_checked(resolver, url).await?;
    Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FixedResolver(HashMap<String, Vec<IpAddr>>);

    #[async_trait]
    impl Resolver for FixedResolver {
        async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| format!("未知主机 {host}"))
        }
    }

    fn url(raw: &str) -> reqwest::Url {
        reqwest::Url::parse(raw).unwrap()
    }

    #[test]
    fn scheme_whitelist_rejects_file_ftp_data() {
        for raw in [
            "file:///C:/Windows/win.ini",
            "ftp://example.com/a",
            "data:text/plain,hi",
            "javascript:alert(1)",
        ] {
            assert!(
                matches!(validate_url(&url(raw)), Err(GuardError::Scheme(_))),
                "必须拒绝: {raw}"
            );
        }
        assert!(validate_url(&url("https://example.com/a")).is_ok());
        assert!(validate_url(&url("http://example.com")).is_ok());
    }

    #[test]
    fn userinfo_is_rejected() {
        assert_eq!(validate_url(&url("http://user:pw@example.com/")), Err(GuardError::UserInfo));
        assert_eq!(validate_url(&url("http://user@example.com/")), Err(GuardError::UserInfo));
    }

    #[test]
    fn private_and_metadata_ranges_are_blocked() {
        for raw in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(is_blocked_ip(ip), "必须拦截: {raw}");
        }
        for raw in ["::1", "::", "fc00::1", "fe80::1", "ff02::1", "::ffff:127.0.0.1", "2002:7f00:1::1"] {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(is_blocked_ip(ip), "必须拦截: {raw}");
        }
        for raw in ["93.184.216.34", "2606:2800:220:1::1"] {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(!is_blocked_ip(ip), "公网地址不应被拦: {raw}");
        }
    }

    #[tokio::test]
    async fn hostname_resolving_to_loopback_is_rejected() {
        let resolver = FixedResolver(HashMap::from([(
            "127.0.0.1.nip.io".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )]));
        let error = resolve_checked(&resolver, &url("http://127.0.0.1.nip.io:8080/")).await.unwrap_err();
        assert!(matches!(error, GuardError::PrivateIp(_)), "实际: {error}");
    }

    #[tokio::test]
    async fn any_private_answer_rejects_the_whole_target() {
        let resolver = FixedResolver(HashMap::from([(
            "mixed.example".to_string(),
            vec!["93.184.216.34".parse().unwrap(), "10.0.0.7".parse().unwrap()],
        )]));
        let error = resolve_checked(&resolver, &url("https://mixed.example/")).await.unwrap_err();
        assert!(matches!(error, GuardError::PrivateIp(_)), "实际: {error}");
    }

    #[tokio::test]
    async fn ip_literal_is_checked_without_dns() {
        let resolver = FixedResolver(HashMap::new());
        let error = resolve_checked(&resolver, &url("http://192.168.0.10/")).await.unwrap_err();
        assert!(matches!(error, GuardError::PrivateIp(_)), "实际: {error}");
        let ok = resolve_checked(&resolver, &url("http://93.184.216.34/")).await.unwrap();
        assert_eq!(ok, vec!["93.184.216.34".parse::<IpAddr>().unwrap()]);
    }
}
