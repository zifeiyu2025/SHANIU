//! `web_search`：provider 抽象 + 内置免 key 实现（Bing）。
//!
//! 为什么抓公开搜索页而不是接搜索 API：内置默认实现不能要求用户先配 key。
//! 代价是解析脆弱、可能被限流、处于 ToS 灰区——这些边界写在 docs/02，
//! 工具描述也明确告知模型「结果可能不完整或过期」。
//!
//! 目前唯一内置 provider 是 Bing。`SearchProvider` trait 与 [`FallbackProvider`]
//! 保留：以后接插件或换搜索源从这里进，不改调用方。
//!
//! **三种失败必须区分**（用户明确要求）：
//! - 无结果：请求成功、页面正常、确实没有匹配 → 空列表，不是错误；
//! - 解析失败：页面拿到了但结构对不上（改版） → `Parse`；
//! - 被拦截：验证码/限流/403/429/空壳页 → `Blocked`。
//!   后两种绝不能返回空列表，否则模型会误判成「网上没有」。

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

use super::client::{WebClient, WebError, WebResponse};
use super::{new_nonce, truncate_chars, wrap_untrusted};

/// 单次返回条数上限。
pub const MAX_RESULTS: usize = 10;
/// 默认返回条数。
pub const DEFAULT_RESULTS: usize = 5;
/// 摘要上限（字符，不是字节）。
const SNIPPET_CHARS: usize = 500;
/// 查询长度上限，防止把整段文本当 query 打出去。
const MAX_QUERY_CHARS: usize = 400;
/// 连续失败多少次进入冷却。
const FAILURES_BEFORE_COOLDOWN: u32 = 3;
/// 被限流/改版后的冷却时长：短退避，等页面恢复。
const BLOCKED_COOLDOWN: Duration = Duration::from_secs(60);
/// 网络不可达后的冷却时长：连续失败后短时间不再打请求，等网络恢复。
const NETWORK_COOLDOWN: Duration = Duration::from_secs(600);
/// 200 但正文小于此值：视为风控空壳页，而不是「确实没有结果」。
const MIN_BODY_BYTES: usize = 500;

/// Bing 是**容器式**结构：
/// `li.b_algo` 里第一个 `h2 > a` 是结果链接，`div.b_caption` 里是摘要。
#[derive(Clone, Copy)]
struct BingSpec {
    endpoint: &'static str,
    source: &'static str,
    container_tag: &'static str,
    container_class: &'static str,
    heading_tag: &'static str,
    caption_tag: &'static str,
    caption_class: &'static str,
}

const BING: BingSpec = BingSpec {
    endpoint: "https://www.bing.com/search",
    source: "bing",
    container_tag: "li",
    container_class: "b_algo",
    heading_tag: "h2",
    caption_tag: "div",
    caption_class: "b_caption",
};

/// 失败分类。`NoResults` 不在这里——空结果是成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFailure {
    /// 页面拿到了，但结构对不上（大概率改版）。
    Parse,
    /// 验证码、限流、403/429、空壳页。
    Blocked,
    /// 连不上、超时、TLS 失败。
    Network,
}

#[derive(Debug, Clone)]
pub struct SearchError {
    pub kind: SearchFailure,
    pub detail: String,
}

impl SearchError {
    fn new(kind: SearchFailure, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// 给模型看的说明：不同分类给出不同的下一步建议。
    pub fn guidance(&self) -> String {
        match self.kind {
            SearchFailure::Blocked => format!(
                "搜索被限流或拦截（{}）。请稍后重试，或改用 web_fetch 直接抓已知 URL。",
                self.detail
            ),
            SearchFailure::Parse => format!(
                "结果解析失败（{}）。搜索引擎页面结构可能已变化，请改用 web_fetch 打开具体页面。",
                self.detail
            ),
            SearchFailure::Network => format!(
                "网络请求失败（{}）。可稍后重试；若持续失败，改用 web_fetch 或换关键词。",
                self.detail
            ),
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.guidance())
    }
}

/// 一条搜索结果。缺字段就留空，不编造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// 来源标识（例如 `bing`），多 provider 时用于区分。
    pub source: String,
}

/// provider 扩展点：换搜索引擎 / 接插件只加实现，不改调用方。
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn id(&self) -> &'static str;
    async fn search(
        &self,
        query: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchHit>, SearchError>;
}

/// HTTP 抽象：生产用 [`WebClient`]，测试注入脚本化响应。
///
/// 搜索与抓取必须走同一套目标校验，所以 `WebClient` 是唯一的生产实现。
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch(&self, url: &str, cancel: &CancellationToken) -> Result<WebResponse, WebError>;
}

#[async_trait]
impl Fetcher for WebClient {
    async fn fetch(&self, url: &str, cancel: &CancellationToken) -> Result<WebResponse, WebError> {
        WebClient::fetch(self, url, cancel).await
    }
}

#[derive(Default)]
struct Health {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

impl Health {
    fn cooldown_remaining(&self) -> Option<Duration> {
        let until = self.cooldown_until?;
        let now = Instant::now();
        if until > now {
            Some(until - now)
        } else {
            None
        }
    }

    fn success(&mut self) {
        self.consecutive_failures = 0;
        self.cooldown_until = None;
    }

    fn failure(&mut self, kind: SearchFailure) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= FAILURES_BEFORE_COOLDOWN {
            let cooldown = if kind == SearchFailure::Network {
                NETWORK_COOLDOWN
            } else {
                BLOCKED_COOLDOWN
            };
            self.cooldown_until = Some(Instant::now() + cooldown);
            self.consecutive_failures = 0;
        }
    }
}

/// Bing provider。
///
/// 目前唯一的免 key provider。解析结构集中在 [`BING`]，改版时只改那一处。
pub struct Bing {
    fetcher: Arc<dyn Fetcher>,
    health: Mutex<Health>,
}

impl Bing {
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            fetcher,
            health: Mutex::new(Health::default()),
        }
    }

    fn cooldown_remaining(&self) -> Option<Duration> {
        let guard = self.health.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.cooldown_remaining()
    }

    fn record_success(&self) {
        let mut guard = self.health.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.success();
    }

    fn record_failure(&self, kind: SearchFailure) {
        let mut guard = self.health.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.failure(kind);
    }
}

#[async_trait]
impl SearchProvider for Bing {
    fn id(&self) -> &'static str {
        "bing"
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if let Some(remaining) = self.cooldown_remaining() {
            return Err(SearchError::new(
                SearchFailure::Blocked,
                format!("连续失败后冷却中，还剩 {} 秒", remaining.as_secs() + 1),
            ));
        }
        let mut url = reqwest::Url::parse(BING.endpoint)
            .map_err(|error| SearchError::new(SearchFailure::Network, error.to_string()))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", query);
            // 多要一些再截断：Bing 的结果里会混入广告与站内模块。
            pairs.append_pair("count", &(limit * 2).to_string());
        }
        let response = self
            .fetcher
            .fetch(url.as_str(), cancel)
            .await
            .map_err(map_web_error)?;
        match finish(&response, |body| parse_bing(body, BING, limit)) {
            Ok(hits) => {
                self.record_success();
                Ok(hits)
            }
            Err(error) => {
                self.record_failure(error.kind);
                Err(error)
            }
        }
    }
}

/// provider 串行回退链：前一家的任何失败都会尝试下一家。
///
/// 为什么连 `Blocked` 也回退：不同引擎的反爬彼此独立，一家限流不代表另一家不可用。
/// **空结果是成功**，不再问下一家，否则「确实没有」会被下一家的偶发命中覆盖成「有结果」。
pub struct FallbackProvider {
    providers: Vec<Arc<dyn SearchProvider>>,
}

impl FallbackProvider {
    pub fn new(providers: Vec<Arc<dyn SearchProvider>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl SearchProvider for FallbackProvider {
    fn id(&self) -> &'static str {
        "local-chain"
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if self.providers.is_empty() {
            return Err(SearchError::new(
                SearchFailure::Network,
                "没有可用的搜索 provider",
            ));
        }
        let mut failures: Vec<(SearchFailure, String)> = Vec::new();
        for provider in &self.providers {
            if cancel.is_cancelled() {
                return Err(SearchError::new(SearchFailure::Network, "已取消"));
            }
            match provider.search(query, limit, cancel).await {
                Ok(hits) => return Ok(hits),
                Err(error) => failures.push((
                    error.kind,
                    format!("{}: {}", provider.id(), error.detail),
                )),
            }
        }
        // 分类取「最可执行」的那一种：被拦截可以稍后重试或转 web_fetch，
        // 改版说明结构变了，网络错误只能重试。
        let kind = if failures.iter().any(|(kind, _)| *kind == SearchFailure::Blocked) {
            SearchFailure::Blocked
        } else if failures.iter().any(|(kind, _)| *kind == SearchFailure::Parse) {
            SearchFailure::Parse
        } else {
            SearchFailure::Network
        };
        let detail = failures
            .iter()
            .map(|(_, detail)| detail.as_str())
            .collect::<Vec<_>>()
            .join("；");
        Err(SearchError::new(kind, detail))
    }
}

fn map_web_error(error: WebError) -> SearchError {
    match error {
        WebError::Cancelled => SearchError::new(SearchFailure::Network, "已取消"),
        other => SearchError::new(SearchFailure::Network, other.to_string()),
    }
}

/// 共享分类：状态码与拦截标记先判，然后交给具体解析器；解析不出再区分
/// 「确实没有结果」与「结构变了」。
fn finish(
    response: &WebResponse,
    parse: impl FnOnce(&str) -> Vec<SearchHit>,
) -> Result<Vec<SearchHit>, SearchError> {
    let body = precheck(response)?;
    let mut hits = parse(&body);
    hits.dedup_by(|left, right| left.url == right.url);
    if !hits.is_empty() {
        return Ok(hits);
    }
    let lower = body.to_ascii_lowercase();
    if NO_RESULT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return Ok(Vec::new());
    }
    if body.len() < MIN_BODY_BYTES {
        // 200 但几乎没有内容：常见于风控返回的空壳页，不能当成"没有结果"。
        return Err(SearchError::new(
            SearchFailure::Blocked,
            format!("返回空壳页（{} 字节）", body.len()),
        ));
    }
    Err(SearchError::new(
        SearchFailure::Parse,
        "页面里找不到结果容器",
    ))
}

/// 状态码与反爬标记检查通过后返回正文。
fn precheck(response: &WebResponse) -> Result<String, SearchError> {
    if matches!(response.status, 403 | 429 | 503) {
        return Err(SearchError::new(
            SearchFailure::Blocked,
            format!("HTTP {}", response.status),
        ));
    }
    if (500..600).contains(&response.status) {
        return Err(SearchError::new(
            SearchFailure::Network,
            format!("HTTP {}", response.status),
        ));
    }
    if response.status != 200 {
        return Err(SearchError::new(
            SearchFailure::Blocked,
            format!("HTTP {}（非 200）", response.status),
        ));
    }
    let body = response.text();
    let lower = body.to_ascii_lowercase();
    if BLOCK_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return Err(SearchError::new(
            SearchFailure::Blocked,
            "页面出现验证码/反爬提示",
        ));
    }
    Ok(body)
}

const BLOCK_MARKERS: &[&str] = &[
    "captcha",
    "unusual traffic",
    "challenge-form",
    "anomaly",
    "detected automated",
    "rate limit",
    "too many requests",
    "access denied",
];

const NO_RESULT_MARKERS: &[&str] = &[
    "no results",
    "no results found",
    "class=\"no-results\"",
    "没有找到",
];

// ============================== 解析 ==============================

/// Bing 的容器式解析：先找 `li.b_algo`，再在容器内找标题链接与摘要。
fn parse_bing(html: &str, spec: BingSpec, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for container in elements_by_class(html, spec.container_tag, spec.container_class) {
        if hits.len() >= limit {
            break;
        }
        let Some((attrs, inner)) = elements_by_tag(&container, spec.heading_tag)
            .iter()
            .find_map(|heading| scan_anchors(heading).into_iter().next())
        else {
            continue;
        };
        let Some(href) = attr_value(&attrs, "href") else { continue };
        let Some(url) = normalize_bing_href(&href) else { continue };
        let title = text_of(&inner);
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title,
            url,
            snippet: bing_snippet(&container, spec),
            source: spec.source.to_string(),
        });
    }
    hits
}

fn bing_snippet(container: &str, spec: BingSpec) -> String {
    let Some(caption) = elements_by_class(container, spec.caption_tag, spec.caption_class)
        .into_iter()
        .next()
    else {
        return String::new();
    };
    // 摘要优先取 `<p>`；没有 `p` 时退回整个 caption 的文本（会带上 cite，但聊胜于无）。
    let text = elements_by_tag(&caption, "p")
        .into_iter()
        .map(|paragraph| text_of(&paragraph))
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| text_of(&caption));
    truncate_chars(&text, SNIPPET_CHARS)
}

/// 扫描所有 `<a ...>...</a>`，返回（属性串, 内层 HTML）。
///
/// 只做这一种标签是够的：Bing 的标题链接在 `h2` 内，仍是 `<a>`。
fn scan_anchors(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(offset) = html[index..].find("<a") {
        let start = index + offset;
        let after = &html[start + 2..];
        match after.chars().next() {
            Some(character) if character == '>' || character.is_whitespace() => {}
            _ => {
                index = start + 2;
                continue;
            }
        }
        let Some(gt) = html[start..].find('>') else { break };
        let attrs_start = start + 2;
        let attrs_end = start + gt;
        let inner_start = attrs_end + 1;
        let Some(end) = html[inner_start..].find("</a>") else { break };
        out.push((
            html[attrs_start..attrs_end].to_string(),
            html[inner_start..inner_start + end].to_string(),
        ));
        index = inner_start + end + 4;
    }
    out
}

/// 扫描指定标签里 class 命中的元素，返回内层 HTML。
fn elements_by_class(html: &str, tag: &str, class: &str) -> Vec<String> {
    collect_elements(html, tag, Some(class))
}

/// 扫描指定标签的全部元素（不按 class 过滤），返回内层 HTML。
fn elements_by_tag(html: &str, tag: &str) -> Vec<String> {
    collect_elements(html, tag, None)
}

fn collect_elements(html: &str, tag: &str, class: Option<&str>) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(offset) = html[index..].find(&open) {
        let start = index + offset;
        let after = &html[start + open.len()..];
        match after.chars().next() {
            Some(character) if character == '>' || character.is_whitespace() => {}
            _ => {
                index = start + open.len();
                continue;
            }
        }
        let Some(gt) = html[start..].find('>') else { break };
        let attrs = &html[start + open.len()..start + gt];
        let inner_start = start + gt + 1;
        // 同标签可以嵌套（Bing 的 caption 里还有 b_attribution），必须按深度找闭合标签。
        let Some(end_offset) = matching_close(html, inner_start, tag) else { break };
        let end = end_offset - inner_start;
        if class.is_none_or(|class| has_class(attrs, class)) {
            out.push(html[inner_start..inner_start + end].to_string());
        }
        index = inner_start + end + close.len();
    }
    out
}

/// 从 `from` 起找与当前开始标签配对的闭合标签（考虑同标签嵌套）。
fn matching_close(html: &str, from: usize, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut depth = 1usize;
    let mut index = from;
    while index < html.len() {
        let next_open = html[index..].find(&open).map(|offset| index + offset);
        let next_close = html[index..].find(&close).map(|offset| index + offset);
        match (next_open, next_close) {
            (None, None) => return None,
            (Some(open_at), Some(close_at)) if open_at < close_at => {
                let after = &html[open_at + open.len()..];
                // `<div` 与 `<divx` 不是同一种标签。
                if after.starts_with(|character: char| character == '>' || character.is_whitespace()) {
                    depth += 1;
                }
                index = open_at + open.len();
            }
            (_, Some(close_at)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(close_at);
                }
                index = close_at + close.len();
            }
            (Some(_), None) => return None,
        }
    }
    None
}

fn has_class(attrs: &str, class: &str) -> bool {
    attr_value(attrs, "class")
        .map(|value| value.split_whitespace().any(|token| token == class))
        .unwrap_or(false)
}

/// 取属性值，支持单引号、双引号与无引号三种写法。
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower[from..].find(name) {
        let at = from + offset;
        let boundary_before =
            at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'-');
        let mut cursor = at + name.len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if boundary_before && cursor < bytes.len() && bytes[cursor] == b'=' {
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                return None;
            }
            let quote = bytes[cursor];
            if quote == b'"' || quote == b'\'' {
                let end = lower[cursor + 1..].find(quote as char)? + cursor + 1;
                return Some(attrs[cursor + 1..end].to_string());
            }
            let end = lower[cursor..]
                .find(|character: char| character.is_ascii_whitespace() || character == '>')
                .map(|offset| cursor + offset)
                .unwrap_or(lower.len());
            return Some(attrs[cursor..end].to_string());
        }
        from = at + name.len();
    }
    None
}

/// Bing 结果链接规范化：`/ck/a?...&u=a1<base64url>` 是跳转中间页，需要还原成目标 URL。
///
/// Bing 的有机结果大多也是直链，只有部分场景包一层 `ck/a`；站内链接（图片、词典等）
/// 一律丢弃，避免把「必应内部页」当成搜索结果返回。
fn normalize_bing_href(href: &str) -> Option<String> {
    let decoded = decode_entities(href);
    let href = decoded.trim();
    if href.is_empty() {
        return None;
    }
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else if href.starts_with('/') {
        format!("https://www.bing.com{href}")
    } else {
        href.to_string()
    };
    let parsed = reqwest::Url::parse(&absolute).ok()?;
    let host = parsed.host_str().unwrap_or_default();
    let is_bing = host == "bing.com" || host.ends_with(".bing.com");
    let target = if is_bing && parsed.path().starts_with("/ck/a") {
        let encoded = parsed
            .query_pairs()
            .find(|(key, _)| key == "u")
            .map(|(_, value)| value.into_owned())?;
        // 值形如 `a1<base64url>`，前缀 a1 是 Bing 的标记位。
        let payload = encoded.strip_prefix("a1").unwrap_or(&encoded);
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        String::from_utf8(bytes).ok()?
    } else if is_bing {
        // 站内导航、图片、词典等不是有机结果。
        return None;
    } else {
        parsed.to_string()
    };
    let final_url = reqwest::Url::parse(&target).ok()?;
    match final_url.scheme() {
        "http" | "https" => Some(final_url.to_string()),
        _ => None,
    }
}

/// 去标签 + 解实体 + 压空白。
fn text_of(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            other if !in_tag => text.push(other),
            _ => {}
        }
    }
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // 分号是单字节，按字节窗口找即可；`;` 的位置一定是合法 char 边界。
        let window = &tail.as_bytes()[..tail.len().min(12)];
        let Some(byte_index) = window.iter().position(|byte| *byte == b';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let semicolon = byte_index;
        let entity = &tail[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "rsquo" => Some('\u{2019}'),
            "lsquo" => Some('\u{2018}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            other if other.starts_with("#x") || other.starts_with("#X") => {
                u32::from_str_radix(&other[2..], 16).ok().and_then(char::from_u32)
            }
            other if other.starts_with('#') => {
                other[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &tail[semicolon + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ============================== 工具 ==============================

pub struct WebSearch {
    spec: ToolSpec,
    provider: Arc<dyn SearchProvider>,
}

impl WebSearch {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            spec: ToolSpec::new(
                "web_search",
                "Search the public web via Bing result pages. Returns titles, URLs \
                 and snippets; results may be incomplete or stale. Use web_fetch to verify. \
                 Network is available to this tool even when sandboxed commands are offline.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索关键词（不要塞整段文本）" },
                        "max_results": {
                            "type": "integer",
                            "description": "返回条数，默认 5，上限 10"
                        }
                    },
                    "required": ["query"]
                }),
            ),
            provider,
        }
    }
}

#[async_trait]
impl ToolExecutor for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let query = args
            .get("query")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 query".into()))?;
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(ToolError::InvalidArgs(format!(
                "query 超过 {MAX_QUERY_CHARS} 字符，请缩短关键词"
            )));
        }
        let limit = args
            .get("max_results")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_RESULTS);

        let started = Instant::now();
        let hits = self
            .provider
            .search(query, limit, &ctx.cancel)
            .await
            .map_err(|error| ToolError::Execution(format!("web_search 失败：{}", error.guidance())))?;
        let elapsed = started.elapsed();

        let mut body = String::new();
        if hits.is_empty() {
            body.push_str(&format!(
                "[web_search] provider={} query=\"{}\" 未找到结果（请求成功、页面结构正常，确实没有匹配项）。",
                self.provider.id(),
                query
            ));
        } else {
            body.push_str(&format!(
                "[web_search] provider={} query=\"{}\" results={} elapsed={}ms\n",
                self.provider.id(),
                query,
                hits.len(),
                elapsed.as_millis()
            ));
            for (index, hit) in hits.iter().enumerate() {
                body.push_str(&format!(
                    "{}. {}\n   {}\n   来源={}{}\n",
                    index + 1,
                    hit.title,
                    hit.url,
                    hit.source,
                    if hit.snippet.is_empty() {
                        "\n   摘要：（无）"
                    } else {
                        ""
                    }
                ));
                if !hit.snippet.is_empty() {
                    body.push_str(&format!("   摘要：{}\n", hit.snippet));
                }
            }
            body.push_str(
                "提示：结果来自公开搜索页解析，可能不完整或过期；需要原文时用 web_fetch 打开链接核对。",
            );
        }

        Ok(ToolOutput::ok(wrap_untrusted(
            "WEB_SEARCH",
            &new_nonce(),
            &body,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定夹具：Bing 搜索结果页的骨架。用 include_str! 锁住，
    /// 这样 Bing 改版导致的解析失败会在 CI 暴露，而不是等真机才发现。
    const BING_FIXTURE: &str = include_str!("../../../tests/fixtures/bing_results.html");

    fn classify_bing(response: &WebResponse) -> Result<Vec<SearchHit>, SearchError> {
        finish(response, |body| parse_bing(body, BING, 5))
    }

    #[test]
    fn status_429_is_blocked() {
        let error = classify_bing(&response(429, "slow down")).unwrap_err();
        assert_eq!(error.kind, SearchFailure::Blocked);
    }

    #[test]
    fn empty_shell_is_blocked_not_no_results() {
        let error = classify_bing(&response(200, "<html></html>")).unwrap_err();
        assert_eq!(error.kind, SearchFailure::Blocked);
    }

    #[test]
    fn entity_decoding() {
        assert_eq!(decode_entities("a &amp; b &#39;c&#x27; &unknown;"), "a & b 'c' &unknown;");
    }

    #[test]
    fn decode_entities_never_splits_multibyte_chars() {
        // 回归：`&` 后跟中文时，按字节窗口找分号不能切在多字节字符中间。
        assert_eq!(decode_entities("中文&nbsp;中文&amp;尾巴"), "中文 中文&尾巴");
        assert_eq!(decode_entities("a&程b"), "a&程b");
        assert_eq!(decode_entities("末尾&"), "末尾&");
    }

    fn response(status: u16, body: &str) -> WebResponse {
        WebResponse {
            final_url: "https://www.bing.com/search".into(),
            status,
            content_type: Some("text/html".into()),
            body: body.as_bytes().to_vec(),
            truncated: false,
            elapsed: Duration::from_millis(1),
            redirects: 0,
        }
    }

    // ---------------------------- Bing 解析 ----------------------------

    #[test]
    fn bing_fixture_parses_expected_results() {
        let hits = parse_bing(BING_FIXTURE, BING, 10);
        assert_eq!(hits.len(), 3, "应跳过站内图片链接与非结果节点");

        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].snippet.contains("reliable and efficient"));
        assert_eq!(hits[0].source, "bing");

        // ck/a 跳转中间页必须还原成目标 URL
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
        assert!(hits[1].snippet.contains("teaches Rust from scratch"));

        // caption 里没有 <p> 时退回整个 caption 的文本
        assert_eq!(hits[2].url, "https://tokio.rs/tokio/tutorial/select");
        assert!(hits[2].snippet.contains("Wait on multiple async operations"));
    }

    #[test]
    fn bing_no_results_is_success() {
        let body = format!(
            "<html><body><ol id=\"b_results\"><li class=\"b_no\">There are no results for x</li></ol>{}</body></html>",
            "x".repeat(600)
        );
        assert_eq!(classify_bing(&response(200, &body)).unwrap(), Vec::new());
    }

    #[test]
    fn bing_changed_layout_is_parse_failure() {
        let body = format!(
            "<html><body><div class=\"brand-new-layout\">hi</div>{}</body></html>",
            "x".repeat(600)
        );
        let error = classify_bing(&response(200, &body)).unwrap_err();
        assert_eq!(error.kind, SearchFailure::Parse);
    }

    #[test]
    fn bing_captcha_is_blocked() {
        let body = format!("<html><body>Please solve the CAPTCHA</body>{}</html>", "x".repeat(600));
        let error = classify_bing(&response(200, &body)).unwrap_err();
        assert_eq!(error.kind, SearchFailure::Blocked);
    }

    #[test]
    fn bing_parse_failure_message_is_actionable() {
        // 人工构造的畸形页：结构整体改掉，Bing 解析器找不到 b_algo。
        let body = format!(
            "<html><body><div class=\"totally-new-layout\">hi</div>{}</body></html>",
            "x".repeat(600)
        );
        let error = classify_bing(&response(200, &body)).unwrap_err();
        println!("Bing 解析失败文案：{}", error.guidance());
        assert_eq!(error.kind, SearchFailure::Parse);
        // 模型要能据此做出正确决策：改去 web_fetch 直接抓已知 URL。
        assert!(error.guidance().contains("web_fetch"));
    }

    // ---------------------------- provider 链 ----------------------------

    struct FailingProvider;

    #[async_trait]
    impl SearchProvider for FailingProvider {
        fn id(&self) -> &'static str {
            "failing"
        }
        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _cancel: &CancellationToken,
        ) -> Result<Vec<SearchHit>, SearchError> {
            Err(SearchError::new(SearchFailure::Network, "连接超时"))
        }
    }

    fn hit(url: &str) -> SearchHit {
        SearchHit {
            title: "t".into(),
            url: url.into(),
            snippet: String::new(),
            source: "stub".into(),
        }
    }

    #[tokio::test]
    async fn fallback_moves_to_next_provider_on_failure() {
        let chain = FallbackProvider::new(vec![
            Arc::new(FailingProvider),
            Arc::new(StubProvider { hits: vec![hit("https://b.example/")] }),
        ]);
        let hits = chain.search("x", 5, &CancellationToken::new()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://b.example/");
    }

    #[tokio::test]
    async fn fallback_stops_on_empty_success() {
        // 空结果是成功：不能再去问下一家，否则「确实没有」会被下一家覆盖。
        let chain = FallbackProvider::new(vec![
            Arc::new(StubProvider { hits: Vec::new() }),
            Arc::new(StubProvider { hits: vec![hit("https://b.example/")] }),
        ]);
        assert_eq!(chain.search("x", 5, &CancellationToken::new()).await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn fallback_reports_most_actionable_failure() {
        let chain = FallbackProvider::new(vec![Arc::new(FailingProvider), Arc::new(FailingProvider)]);
        let error = chain.search("x", 5, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.kind, SearchFailure::Network);
        assert!(error.detail.contains("failing"));
    }

    struct StubProvider {
        hits: Vec<SearchHit>,
    }

    #[async_trait]
    impl SearchProvider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _cancel: &CancellationToken,
        ) -> Result<Vec<SearchHit>, SearchError> {
            Ok(self.hits.clone())
        }
    }

    fn context() -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> =
            std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(crate::events::NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(
            std::env::temp_dir(),
            CancellationToken::new(),
            "call-web-search",
            sink,
            crate::config::SandboxMode::ReadOnly,
        )
    }

    #[tokio::test]
    async fn tool_wraps_results_as_untrusted() {
        let provider = Arc::new(StubProvider {
            hits: vec![SearchHit {
                title: "标题".into(),
                url: "https://example.com/".into(),
                snippet: "摘要".into(),
                source: "stub".into(),
            }],
        });
        let tool = WebSearch::new(provider);
        let output = tool
            .execute(serde_json::json!({"query": "测试"}), &context())
            .await
            .unwrap();
        assert!(output.output.starts_with("<<<WEB_SEARCH:"), "{}", output.output);
        assert!(output.output.contains("https://example.com/"));
        assert!(output.output.contains("END:"));
    }

    #[tokio::test]
    async fn tool_reports_empty_results_explicitly() {
        let tool = WebSearch::new(Arc::new(StubProvider { hits: Vec::new() }));
        let output = tool
            .execute(serde_json::json!({"query": "nothing"}), &context())
            .await
            .unwrap();
        assert!(output.output.contains("未找到结果"));
    }

    #[tokio::test]
    async fn tool_rejects_empty_query() {
        let tool = WebSearch::new(Arc::new(StubProvider { hits: Vec::new() }));
        let error = tool
            .execute(serde_json::json!({"query": "  "}), &context())
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs(_)));
    }

}
