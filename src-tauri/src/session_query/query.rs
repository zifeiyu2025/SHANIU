//! 请求归一化、参数化谓词与 snippet（SH-113）。
//!
//! 上游 `session-query-sqlite/src/query.ts` 的 Rust 等价：`normalizeQuery` 的空白归一
//! 与 NUL/长度校验、`quoteFtsData` 的 literal phrase 转义、`makeSnippet` 的按码点截断。
//! 另含一处 SHANIU 特有补充：CJK 单字化（见 `fts_text`）——SQLite FTS5 `unicode61`
//! 默认把连续 CJK 字符合成**一个** token，单字查询无法命中；在文档入库与查询归一化
//! 两侧对称插入零宽分隔，让每个汉字独立成 token，同时连续中文短语以 phrase 命中。
//! tokenizer 仍是 `unicode61`，查询仍是单个 literal phrase，语法字符仍是数据。
//!
//! 命令面按 vendored wire 收敛为 `query-only`：没有 cursor/limit 入参，故这里不保留
//! 任何分页游标能力（`session.search` 前端永远传不进来，留着即死接口）。

use crate::session_query::SearchError;

/// FTS5 `highlight()` 的碰撞-free 起止标记（上游 `FTS_HIGHLIGHT_START/END`）。
pub const FTS_HIGHLIGHT_START: &str = "\u{FDD0}";
pub const FTS_HIGHLIGHT_END: &str = "\u{FDD1}";
const HL_START_CHAR: char = '\u{FDD0}';
const HL_END_CHAR: char = '\u{FDD1}';

/// CJK 单字化用的零宽分隔符：unicode61 把它当分隔符（Cf 不在默认 categories 里），
/// 视觉上不可见，snippet 生成时剥掉即可还原原文观感。
const ZWSP: char = '\u{200B}';

/// 与 DSH wire `SEARCH_QUERY_MAX_CODE_UNITS` 同源（WorkspaceBrowser.tsx:53）。
pub const MAX_QUERY_UTF16: usize = 500;

/// 归一化后的查询：`expression` 是唯一送往 MATCH 的字符串；`data` 供测试对照分词口径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedQuery {
    pub data: String,
    pub expression: String,
}

/// 校验并规范化一次搜索请求：空白归一、拒绝 NUL 与超长输入。
/// 生产路径上空/纯空白由 `session_query::search` 短路成空表（上游语义），这里的
/// 空串分支只是防止误调时构造出非法空 phrase 的内部防御。
pub fn normalize_query(raw: &str) -> Result<NormalizedQuery, SearchError> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(SearchError::InvalidQuery(
            "搜索词不能为空（首尾空白已忽略）".to_string(),
        ));
    }
    if normalized.contains('\0') {
        return Err(SearchError::InvalidQuery("搜索词不能含 NUL 字符".to_string()));
    }
    if normalized.encode_utf16().count() > MAX_QUERY_UTF16 {
        return Err(SearchError::QueryTooLong(format!(
            "搜索词超过 {MAX_QUERY_UTF16} 个 UTF-16 码元，请缩短后再搜"
        )));
    }
    let data = fts_text(&normalized);
    let expression = quote_fts_data(&data);
    Ok(NormalizedQuery { data, expression })
}

/// 把调用方文本 quoted 成**单个** FTS5 phrase。引号翻倍转义，其余字符
/// （`OR` / `NEAR` / `*` / `-` / `(` / `:` …）在 phrase 内一律是数据，
/// 绝不拼成可执行 MATCH 表达式（防注入 + 防语法报错）。
pub fn quote_fts_data(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// 归一化文本使其进入 FTS5：先脱敏保留标记，再做 CJK 单字化。
///
/// 对文档与查询**对称**应用，保证 phrase 的 token 序列与索引一致。
pub fn fts_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    // 开头不需要前导分隔符；记录上一位是否已是分隔符，避免连续插入两个。
    let mut prev_separator = true;
    for original in text.chars() {
        let ch = match original {
            '\0' | HL_START_CHAR | HL_END_CHAR => '\u{FFFD}',
            other => other,
        };
        if ch == ZWSP {
            out.push(ZWSP);
            prev_separator = true;
            continue;
        }
        if is_cjk_char(ch) {
            if !prev_separator {
                out.push(ZWSP);
            }
            out.push(ch);
            out.push(ZWSP);
            prev_separator = true;
        } else {
            out.push(ch);
            prev_separator = false;
        }
    }
    out.trim_matches(ZWSP).to_string()
}

/// CJK 表意文字、日文假名、韩文音节：这些文字在 unicode61 下会连续成 token，
/// 需要逐字切开才能让单字查询命中（SH-113 验收「中文单字成词」）。
fn is_cjk_char(ch: char) -> bool {
    let code = ch as u32;
    (0x3400..=0x4DBF).contains(&code)        // 扩展 A
        || (0x4E00..=0x9FFF).contains(&code) // 基本区
        || (0xF900..=0xFAFF).contains(&code) // 兼容表意文字
        || (0x20000..=0x2FA1F).contains(&code) // 扩展 B–F / 兼容扩展
        || (0x3040..=0x30FF).contains(&code) // 平假名 / 片假名
        || (0xAC00..=0xD7AF).contains(&code) // 韩文音节
}

/// 从 `highlight()` 的标记文本生成有界纯文本片段（上游 `makeSnippet` 逐行等价）。
/// 按 Unicode 码点截断、以首个命中位置居中、空白归一、剥掉零宽分隔符与标记。
pub fn make_snippet(marked: &str, max_chars: usize) -> String {
    let (clean, match_start) = normalize_marked_text(marked);
    let characters: Vec<char> = clean.chars().collect();
    if characters.len() <= max_chars {
        return clean;
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let matched_index = match_start.min(characters.len() - 1);
    let mut start = matched_index.saturating_sub(max_chars / 3);
    let prefix = if start > 0 { "…" } else { "" };
    let mut suffix = "…";
    let mut content_length = max_chars - prefix.chars().count() - suffix.chars().count();
    if content_length < 1 {
        start = matched_index;
        suffix = "";
        content_length = max_chars - prefix.chars().count();
    } else if matched_index >= start + content_length {
        start = matched_index + 1 - content_length;
    }
    let mut end = (start + content_length).min(characters.len());
    if end == characters.len() {
        suffix = "";
        content_length = max_chars - prefix.chars().count();
        start = end.saturating_sub(content_length);
    }
    end = (start + content_length).min(characters.len());
    let body: String = characters[start..end].iter().collect();
    format!("{prefix}{body}{suffix}")
}

/// 去掉 highlight 标记、空白归一为单空格、剥掉零宽分隔符；返回首个命中的字符位置。
fn normalize_marked_text(marked: &str) -> (String, usize) {
    let mut characters: Vec<char> = Vec::new();
    let mut match_start: Option<usize> = None;
    for ch in marked.chars() {
        if ch == HL_START_CHAR {
            if match_start.is_none() {
                match_start = Some(characters.len());
            }
            continue;
        }
        if ch == HL_END_CHAR || ch == ZWSP {
            continue;
        }
        if ch.is_whitespace() {
            if !characters.is_empty() && characters.last() != Some(&' ') {
                characters.push(' ');
            }
        } else {
            characters.push(ch);
        }
    }
    if characters.last() == Some(&' ') {
        characters.pop();
    }
    (characters.iter().collect(), match_start.unwrap_or(0))
}

/// 跨会话搜索：persisted base + connection-local live TEMP 表 UNION，按每会话最强命中
/// 分组（`event_rank = 1` 保证一个 sessionId 只出一条），再按「命中 span 多 → 文档短 →
/// 时间 → session id → seq」排名并限量。没有 OFFSET：wire 没有续页请求。
///
/// 参数顺序固定（见 `session_query::run_query`）：
/// `[HL_START, HL_END, expression, HL_START, HL_END, expression, HL_START, marker_bytes, limit]`。
pub const SEARCH_SQL: &str = "
WITH candidates AS (
  SELECT
    persisted_docs.session_id AS session_id,
    0 AS live,
    1 AS persisted,
    CAST(persisted_docs.seq AS INTEGER) AS seq,
    persisted_docs.type AS type,
    CAST(persisted_docs.time AS INTEGER) AS time,
    persisted_docs.surface AS surface,
    highlight(persisted_docs, 0, ?1, ?2) AS marked_text,
    CAST(persisted_docs.codepoint_length AS INTEGER) AS document_length
  FROM persisted_docs
  JOIN persisted_sessions ON persisted_sessions.id = persisted_docs.session_id
  WHERE persisted_docs MATCH ?3
    AND NOT EXISTS (SELECT 1 FROM temp.live_sessions AS ls WHERE ls.id = persisted_docs.session_id)
  UNION ALL
  SELECT
    live_docs.session_id AS session_id,
    1 AS live,
    live_sessions.persisted AS persisted,
    CAST(live_docs.seq AS INTEGER) AS seq,
    live_docs.type AS type,
    CAST(live_docs.time AS INTEGER) AS time,
    live_docs.surface AS surface,
    highlight(live_docs, 0, ?4, ?5) AS marked_text,
    CAST(live_docs.codepoint_length AS INTEGER) AS document_length
  FROM temp.live_docs AS live_docs
  JOIN temp.live_sessions AS live_sessions ON live_sessions.id = live_docs.session_id
  WHERE live_docs MATCH ?6
), matched AS (
  SELECT *,
    (
      length(CAST(marked_text AS BLOB))
      - length(CAST(replace(marked_text, ?7, '') AS BLOB))
    ) / ?8 AS match_count
  FROM candidates
), ranked AS (
  SELECT *, ROW_NUMBER() OVER (
    PARTITION BY session_id
    ORDER BY match_count DESC, document_length ASC, time DESC, seq DESC
  ) AS event_rank
  FROM matched
)
SELECT * FROM ranked
WHERE event_rank = 1
ORDER BY match_count DESC, document_length ASC, time DESC, session_id ASC, seq DESC
LIMIT ?9
";
