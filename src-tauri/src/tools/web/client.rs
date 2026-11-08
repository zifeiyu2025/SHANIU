//! 共享 HTTP 客户端：`web_search` 与 `web_fetch` 都只走这里。
//!
//! 四条安全约束（改任何一条都要同时改测试）：
//!
//! 1. 只允许 http/https，且拒绝带用户信息的 URL（见 [`super::guard`]）。
//! 2. **每一跳**重定向都重新做 scheme + DNS + IP 校验，跳数上限 5。
//! 3. 解析结果固定给 reqwest（`ClientBuilder::resolve`），并关闭系统代理——
//!    否则真正建立连接的地址可能不是我们校验过的那个（DNS rebinding / 代理旁路）。
//! 4. 响应体有硬上限（默认 2 MB），超限截断并打 `truncated` 标记，不静默丢弃。

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::guard::{self, GuardError, Resolver, SystemResolver};

/// 默认响应体上限：2 MB。超过部分截断，截图类页面不适用本工具。
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;
/// 默认总超时（含连接、重定向、读体）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 默认连接超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 默认最大重定向跳数。
pub const DEFAULT_MAX_REDIRECTS: usize = 5;
/// 默认并发上限：同时对外的抓取数。
pub const DEFAULT_CONCURRENCY: usize = 4;
/// 默认缓存 TTL：同一 URL 几分钟内不重复抓。
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
/// 缓存条目上限，防止长会话把网页正文攒在内存里。
const MAX_CACHE_ENTRIES: usize = 32;

/// 真实浏览器 UA。用爬虫 UA 只会更早被拦，这里不是为了伪装，是减少误判。
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";

#[derive(Debug, Clone)]
pub enum WebError {
    Guard(GuardError),
    /// 网络层失败（DNS 之外的连接、TLS、读体）。
    Http(String),
    /// 等待并发名额超时。
    Busy,
    /// 超过重定向上限。
    TooManyRedirects(usize),
    Cancelled,
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Guard(error) => write!(f, "{error}"),
            Self::Http(message) => write!(f, "网络请求失败：{message}"),
            Self::Busy => write!(f, "同时进行的抓取已达上限，请稍后重试"),
            Self::TooManyRedirects(limit) => {
                write!(f, "重定向超过 {limit} 跳，已停止（可能是跳转链或反爬）")
            }
            Self::Cancelled => write!(f, "已取消"),
        }
    }
}

impl From<GuardError> for WebError {
    fn from(error: GuardError) -> Self {
        Self::Guard(error)
    }
}

/// 一次成功的 HTTP 往返（状态码不保证 2xx，由调用方判断）。
#[derive(Debug, Clone)]
pub struct WebResponse {
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub elapsed: Duration,
    pub redirects: usize,
}

impl WebResponse {
    /// 正文按 UTF-8 宽松解码；非 UTF-8 站点会出现替换字符（已知限制，见 docs）。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone)]
struct CacheEntry {
    stored_at: Instant,
    response: WebResponse,
}

pub struct WebClient {
    resolver: Arc<dyn Resolver>,
    permits: Semaphore,
    cache: Mutex<HashMap<String, CacheEntry>>,
    max_bytes: usize,
    timeout: Duration,
    max_redirects: usize,
    cache_ttl: Duration,
}

impl Default for WebClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebClient {
    pub fn new() -> Self {
        Self::with_resolver(Arc::new(SystemResolver))
    }

    pub fn with_resolver(resolver: Arc<dyn Resolver>) -> Self {
        Self {
            resolver,
            permits: Semaphore::new(DEFAULT_CONCURRENCY),
            cache: Mutex::new(HashMap::new()),
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: DEFAULT_TIMEOUT,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            cache_ttl: DEFAULT_CACHE_TTL,
        }
    }

    /// 测试与受限场景用：缩小上限、缩短超时。
    pub fn with_limits(mut self, max_bytes: usize, timeout: Duration, max_redirects: usize) -> Self {
        self.max_bytes = max_bytes;
        self.timeout = timeout;
        self.max_redirects = max_redirects;
        self
    }

    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// GET 一个 URL，跟随重定向（逐跳校验），返回截断后的正文。
    pub async fn fetch(
        &self,
        raw_url: &str,
        cancel: &CancellationToken,
    ) -> Result<WebResponse, WebError> {
        if let Some(hit) = self.cache_get(raw_url) {
            return Ok(hit);
        }
        // 并发名额：拿到才发请求，等待也受取消控制，避免排队把一轮拖死。
        let _permit = tokio::select! {
            permit = self.permits.acquire() => permit.map_err(|_| WebError::Busy)?,
            _ = cancel.cancelled() => return Err(WebError::Cancelled),
        };

        let mut current = reqwest::Url::parse(raw_url)
            .map_err(|error| WebError::Guard(GuardError::InvalidUrl(error.to_string())))?;
        let started = Instant::now();
        let mut redirects = 0usize;

        loop {
            let addrs = guard::resolve_target(self.resolver.as_ref(), &current).await?;
            // 固定解析结果 + 关闭代理：真正连的就是这里校验过的 IP。
            let host = current
                .host_str()
                .ok_or_else(|| WebError::Guard(GuardError::InvalidUrl("缺少主机名".into())))?
                .to_string();
            let mut builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(self.timeout)
                .no_proxy()
                .user_agent(USER_AGENT);
            if host.parse::<IpAddr>().is_err() {
                if let Some(addr) = addrs.first() {
                    // reqwest 只接受一个固定地址；多 A 记录时取第一个，
                    // 其余地址在失败后不会回退（已知限制，保守优先）。
                    builder = builder.resolve(&host, SocketAddr::new(addr.ip(), addr.port()));
                }
            }
            let client = builder
                .build()
                .map_err(|error| WebError::Http(error.to_string()))?;

            let response = tokio::select! {
                result = client
                    .get(current.clone())
                    .header(
                        reqwest::header::ACCEPT,
                        "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5",
                    )
                    .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
                    .send() => result.map_err(|error| WebError::Http(error.to_string()))?,
                _ = cancel.cancelled() => return Err(WebError::Cancelled),
            };

            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let Some(location) = location else {
                    // 3xx 却没有 Location：当成普通响应返回，由调用方报状态码。
                    return self.read_body(response, current, status, started, redirects, cancel).await;
                };
                if redirects >= self.max_redirects {
                    return Err(WebError::TooManyRedirects(self.max_redirects));
                }
                let next = current
                    .join(&location)
                    .map_err(|error| WebError::Guard(GuardError::InvalidUrl(error.to_string())))?;
                // 下一轮循环开头会重新校验；这里提前给出一条可读的错误。
                guard::validate_url(&next)?;
                current = next;
                redirects += 1;
                continue;
            }

            let response = self
                .read_body(response, current, status, started, redirects, cancel)
                .await?;
            if response.status == 200 && !response.truncated {
                self.cache_put(raw_url, response.clone());
            }
            return Ok(response);
        }
    }

    async fn read_body(
        &self,
        response: reqwest::Response,
        url: reqwest::Url,
        status: reqwest::StatusCode,
        started: Instant,
        redirects: usize,
        cancel: &CancellationToken,
    ) -> Result<WebResponse, WebError> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancel.cancelled() => return Err(WebError::Cancelled),
            };
            let Some(chunk) = chunk else { break };
            let bytes = chunk.map_err(|error| WebError::Http(error.to_string()))?;
            let remaining = self.max_bytes.saturating_sub(body.len());
            if bytes.len() > remaining {
                body.extend_from_slice(&bytes[..remaining]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&bytes);
        }
        Ok(WebResponse {
            final_url: url.to_string(),
            status: status.as_u16(),
            content_type,
            body,
            truncated,
            elapsed: started.elapsed(),
            redirects,
        })
    }

    fn cache_get(&self, key: &str) -> Option<WebResponse> {
        let guard = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard.get(key)?;
        if entry.stored_at.elapsed() > self.cache_ttl {
            return None;
        }
        Some(entry.response.clone())
    }

    fn cache_put(&self, key: &str, response: WebResponse) {
        let mut guard = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(key, _)| key.clone())
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(
            key.to_string(),
            CacheEntry {
                stored_at: Instant::now(),
                response,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::web::guard::Resolver;
    use async_trait::async_trait;
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

    #[tokio::test]
    async fn private_target_fails_before_any_request() {
        let resolver = Arc::new(FixedResolver(HashMap::from([(
            "internal.example".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])));
        let client = WebClient::with_resolver(resolver);
        let error = client
            .fetch("http://internal.example:9/", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, WebError::Guard(GuardError::PrivateIp(_))), "实际: {error}");
    }

    #[tokio::test]
    async fn non_http_scheme_fails_before_any_request() {
        let client = WebClient::new();
        let error = client
            .fetch("file:///C:/Windows/win.ini", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, WebError::Guard(GuardError::Scheme(_))), "实际: {error}");
    }

    #[tokio::test]
    async fn cancelled_before_start() {
        let client = WebClient::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = client.fetch("https://example.com/", &cancel).await.unwrap_err();
        assert!(matches!(error, WebError::Cancelled), "实际: {error}");
    }
}
