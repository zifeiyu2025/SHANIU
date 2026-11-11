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
use super::{decode_entities, new_nonce, truncate_chars, wrap_untrusted};

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
    /// 用户/宿主取消。单独成类，才能一路映射回 `ToolError::Cancelled`，
    /// 让本轮按取消收口，而不是当成一次工具失败继续跑。
    Cancelled,
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
            SearchFailure::Cancelled => "已取消".to_string(),
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
        match finish(
            &response,
            |body| parse_bing(body, BING, limit),
            bing_confirms_no_results,
        ) {
            Ok(hits) => {
                self.record_success();
                Ok(hits)
            }
            Err(error) => {
                // 取消不是「搜索不健康」：不能把它记进失败计数、更不能由此进冷却。
                if error.kind != SearchFailure::Cancelled {
                    self.record_failure(error.kind);
                }
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
                return Err(SearchError::new(SearchFailure::Cancelled, "已取消"));
            }
            match provider.search(query, limit, cancel).await {
                Ok(hits) => return Ok(hits),
                Err(error) => failures.push((
                    error.kind,
                    format!("{}: {}", provider.id(), error.detail),
                )),
            }
        }
        // 取消优先：它不是「某一家挂了」，而是整轮不该继续。
        let kind = if failures.iter().any(|(kind, _)| *kind == SearchFailure::Cancelled) {
            SearchFailure::Cancelled
        } else if failures.iter().any(|(kind, _)| *kind == SearchFailure::Blocked) {
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
        WebError::Cancelled => SearchError::new(SearchFailure::Cancelled, "已取消"),
        other => SearchError::new(SearchFailure::Network, other.to_string()),
    }
}

/// 共享分类：状态码与拦截标记先判，然后交给具体解析器；解析不出再区分
/// 「确实没有结果」与「结构变了」。
///
/// **无结果只认结构**（`confirms_no_results`）：不再扫正文关键词。拦截页/错误页
/// 里出现 "no results" 并不证明搜索成功，按关键词判会把失败谎报成「确实没找到」——
/// 用户以为这件事不存在，实际是我们自己搜失败了。
fn finish(
    response: &WebResponse,
    parse: impl FnOnce(&str) -> Vec<SearchHit>,
    confirms_no_results: impl FnOnce(&str) -> bool,
) -> Result<Vec<SearchHit>, SearchError> {
    let body = precheck(response)?;
    let mut hits = parse(&body);
    hits.dedup_by(|left, right| left.url == right.url);
    if !hits.is_empty() {
        return Ok(hits);
    }
    if confirms_no_results(&body) {
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

// ============================== 解析 ==============================

/// 结构化的「无结果」确认：结果区存在，且 Bing 明确给出无结果元素 `li.b_no`。
///
/// 只认结构、不认正文关键词——否则含 "no results" 字样的拦截页/错误页会被当成
/// 「确实没有匹配项」，正是最坏的失败形态。
fn bing_confirms_no_results(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let has_results_region =
        lower.contains("id=\"b_results\"") || lower.contains("id='b_results'");
    has_results_region && !elements_by_class(html, "li", "b_no").is_empty()
}

/// Bing 的容器式解析：先找 `li.b_algo`，再在容器内找标题链接与摘要。
fn parse_bing(html: &str, spec: BingSpec, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for (container_attrs, container) in
        elements_by_class_with_attrs(html, spec.container_tag, spec.container_class)
    {
        if hits.len() >= limit {
            break;
        }
        // 广告/推广位由 Bing 自己的结构标记声明，按标记剔除，不读正文猜。
        if is_promoted(&container_attrs, &container) {
            continue;
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

/// 扫描指定标签里 class 命中的元素，返回（属性串, 内层 HTML）。
///
/// 解析广告位需要看容器自身的 class，`elements_by_class` 把它丢了。
fn elements_by_class_with_attrs(html: &str, tag: &str, class: &str) -> Vec<(String, String)> {
    collect_elements_raw(html, tag, Some(class))
}

/// 扫描指定标签的全部元素（不按 class 过滤），返回内层 HTML。
fn elements_by_tag(html: &str, tag: &str) -> Vec<String> {
    collect_elements(html, tag, None)
}

fn collect_elements(html: &str, tag: &str, class: Option<&str>) -> Vec<String> {
    collect_elements_raw(html, tag, class)
        .into_iter()
        .map(|(_, inner)| inner)
        .collect()
}

fn collect_elements_raw(html: &str, tag: &str, class: Option<&str>) -> Vec<(String, String)> {
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
            out.push((
                attrs.to_string(),
                html[inner_start..inner_start + end].to_string(),
            ));
        }
        index = inner_start + end + close.len();
    }
    out
}

/// 广告/推广容器的结构类名。Bing 用它们标记赞助结果。
const AD_CONTAINER_CLASSES: &[&str] = &[
    "b_ad",
    "b_adTop",
    "b_adBottom",
    "b_adProvider",
    "b_promoted",
    "b_deepAds",
];

/// 广告徽标的结构类名（容器内出现即视为推广结果）。
const AD_BADGE_CLASSES: &[&str] = &["b_adBadge", "b_adsLabel", "b_promotedLabel"];

/// 是否推广结果：容器自身带广告类名，或内层出现广告徽标类名。
///
/// 只看 `class` 属性，不读正文——"广告"这两个字出现在正常网页里并不代表它是广告位。
fn is_promoted(container_attrs: &str, container: &str) -> bool {
    AD_CONTAINER_CLASSES
        .iter()
        .any(|class| has_class(container_attrs, class))
        || has_any_class(container, AD_BADGE_CLASSES)
}

/// 页面任意元素的 `class` 是否命中给定集合（大小写不敏感，按空白切分 token）。
fn has_any_class(html: &str, classes: &[&str]) -> bool {
    let lower = html.to_ascii_lowercase();
    let lowered: Vec<String> = classes.iter().map(|class| class.to_ascii_lowercase()).collect();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("class=") {
        let at = from + offset + "class=".len();
        let rest = lower[at..].trim_start_matches(['"', '\'']);
        let end = rest.find(['"', '\'']).unwrap_or(rest.len());
        if rest[..end]
            .split_whitespace()
            .any(|token| lowered.iter().any(|class| token == class))
        {
            return true;
        }
        from = at + 1;
    }
    false
}

// ============================== 查询覆盖率 ==============================

fn is_cjk(character: char) -> bool {
    matches!(character, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// 查询词切分。
///
/// 英文/数字按空白与 ASCII 标点切；含 CJK 时用字符 bigram——中文没有空格，
/// 整串相等会把「北京天气」和「上海天气」判成完全无关，而它们共享「天气」。
fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    if query.chars().any(is_cjk) {
        let chars: Vec<char> = query
            .chars()
            .filter(|character| !character.is_whitespace() && !character.is_ascii_punctuation())
            .collect();
        if chars.len() == 1 {
            terms.push(chars[0].to_string());
        }
        for window in chars.windows(2) {
            let term: String = window.iter().collect();
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
        return terms;
    }
    for term in query
        .split(|character: char| character.is_whitespace() || character.is_ascii_punctuation())
        .filter(|term| !term.is_empty())
    {
        let term = term.to_ascii_lowercase();
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// 查询词在标题/摘要中的覆盖率（百分比，整数）。查询没有可用词时返回 `None`。
///
/// 这是一个客观量，不做硬过滤：删掉结果的风险大于收益，判断权留给模型。
fn query_coverage(query: &str, title: &str, snippet: &str) -> Option<u32> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let haystack = format!("{title} {snippet}").to_lowercase();
    let hit = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count();
    Some((hit as u32 * 100) / terms.len() as u32)
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
        let hits = self.provider.search(query, limit, &ctx.cancel).await.map_err(|error| {
            // 取消必须映射回取消，不能变成一次普通工具失败：否则本轮会带着
            // 「搜索出错」继续跑，用户按了停止却看到模型继续。
            if error.kind == SearchFailure::Cancelled {
                ToolError::Cancelled
            } else {
                ToolError::Execution(format!("web_search 失败：{}", error.guidance()))
            }
        })?;
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
                body.push_str(&format!("{}. {}\n", index + 1, hit.title));
                // 逐条给出客观覆盖率：模型据此自己判断要不要采用，而不是被硬过滤掉。
                if let Some(coverage) = query_coverage(query, &hit.title, &hit.snippet) {
                    body.push_str(&format!("   命中率={coverage}%\n"));
                }
                body.push_str(&format!("   {}\n   来源={}\n", hit.url, hit.source));
                if !hit.snippet.is_empty() {
                    body.push_str(&format!("   摘要：{}\n", hit.snippet));
                }
            }
            body.push_str(
                "提示：命中率是查询词在标题/摘要中的覆盖率，只是客观参考。覆盖率低的结果可能与问题无关，\
                 不要硬套；如果都不相关，请直接说明没找到可靠来源。需要原文时用 web_fetch 打开链接核对。",
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
        finish(response, |body| parse_bing(body, BING, 5), bing_confirms_no_results)
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
    fn blocked_page_with_no_results_phrase_is_parse_failure_not_empty() {
        // 回归（判据级）：拦截页/错误页正文里出现 "no results" 时，旧实现会返回空列表，
        // 也就是把「我们搜失败了」谎报成「确实没找到」。无结果只能由结构确认——
        // 没有 b_results 结果区、没有 b_no 元素，就必须报 Parse。
        let body = format!(
            "<html><body><div class=\"oops\">Sorry, no results could be shown. Please try again.</div>{}</body></html>",
            "x".repeat(600)
        );
        let error = classify_bing(&response(200, &body)).unwrap_err();
        assert_eq!(
            error.kind,
            SearchFailure::Parse,
            "含 no results 字样的非结果页不得被当成空结果：{error:?}"
        );
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

    // ---------------------------- 取消语义 ----------------------------

    struct CancelledProvider;

    #[async_trait]
    impl SearchProvider for CancelledProvider {
        fn id(&self) -> &'static str {
            "cancelled"
        }
        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _cancel: &CancellationToken,
        ) -> Result<Vec<SearchHit>, SearchError> {
            Err(SearchError::new(SearchFailure::Cancelled, "已取消"))
        }
    }

    struct CancelledFetcher;

    #[async_trait]
    impl Fetcher for CancelledFetcher {
        async fn fetch(
            &self,
            _url: &str,
            _cancel: &CancellationToken,
        ) -> Result<WebResponse, WebError> {
            Err(WebError::Cancelled)
        }
    }

    #[test]
    fn web_error_cancelled_maps_to_search_cancelled() {
        assert_eq!(map_web_error(WebError::Cancelled).kind, SearchFailure::Cancelled);
    }

    #[tokio::test]
    async fn tool_maps_cancelled_search_to_tool_cancelled() {
        let tool = WebSearch::new(Arc::new(CancelledProvider));
        let error = tool
            .execute(serde_json::json!({"query": "x"}), &context())
            .await
            .unwrap_err();
        // 反向对照：把映射改回 `Execution`（或让 Cancelled 落进 Network），这条必红。
        assert!(matches!(error, ToolError::Cancelled), "取消不得被当成普通工具失败");
    }

    #[tokio::test]
    async fn cancelled_fetch_does_not_enter_cooldown() {
        let bing = Bing::new(Arc::new(CancelledFetcher));
        let cancel = CancellationToken::new();
        // 连续两次都应是取消：若把取消记进失败计数，第二次会变成「冷却中」的 Blocked。
        for _ in 0..2 {
            let error = bing.search("x", 5, &cancel).await.unwrap_err();
            assert_eq!(
                error.kind,
                SearchFailure::Cancelled,
                "取消不得被记成网络失败或触发冷却：{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn fallback_reports_cancelled() {
        let chain = FallbackProvider::new(vec![Arc::new(CancelledProvider)]);
        let error = chain.search("x", 5, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.kind, SearchFailure::Cancelled);
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

    #[test]
    fn ads_and_promoted_containers_are_dropped_by_structure() {
        // 广告位由 Bing 自己的 class 标记声明：容器带 b_ad，或内层带 b_adBadge。
        // 按结构剔除，不读正文——正常网页里出现「广告」两个字不算。
        let html = r#"
          <ol id="b_results">
            <li class="b_algo b_ad"><h2><a href="https://ad.example.com/x">推广结果</a></h2></li>
            <li class="b_algo"><h2><a href="https://ok.example.com/a">正常结果</a></h2></li>
            <li class="b_algo"><div class="b_adBadge">广告</div><h2><a href="https://ad2.example.com/y">带徽标的推广</a></h2></li>
          </ol>"#;
        let hits = parse_bing(html, BING, 10);
        assert_eq!(hits.len(), 1, "广告位必须被结构标记剔除：{hits:?}");
        assert_eq!(hits[0].url, "https://ok.example.com/a");
    }

    #[test]
    fn query_coverage_reports_objective_ratio() {
        assert_eq!(query_coverage("rust async", "Async Rust", "本书讲 rust"), Some(100));
        assert_eq!(query_coverage("rust tokio", "Async Rust", ""), Some(50));
        // 查询没有可用词时不给数字，免得输出一个 0% 的假信号。
        assert_eq!(query_coverage("", "标题", ""), None);
    }

    #[test]
    fn cjk_query_uses_bigrams_not_whole_string() {
        // 「北京天气」与「上海天气」共享「天气」，不能判成完全无关。
        assert_eq!(query_coverage("北京天气", "上海天气", ""), Some(33));
    }

    #[tokio::test]
    async fn tool_output_annotates_coverage_and_warns_about_low_match() {
        let tool = WebSearch::new(Arc::new(StubProvider {
            hits: vec![SearchHit {
                title: "完全无关的页面".into(),
                url: "https://x.example/1".into(),
                snippet: "别的内容".into(),
                source: "stub".into(),
            }],
        }));
        let output = tool
            .execute(serde_json::json!({"query": "rust async"}), &context())
            .await
            .unwrap();
        assert!(output.output.contains("命中率=0%"), "{}", output.output);
        // 低命中不能只给数字：必须明确告诉模型别硬套、并允许它说没找到。
        assert!(output.output.contains("不要硬套"), "{}", output.output);
        assert!(output.output.contains("没找到可靠来源"), "{}", output.output);
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
