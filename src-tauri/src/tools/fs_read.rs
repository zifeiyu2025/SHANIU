//! 只读文件工具（docs §8 tools/fs_read.rs）：read_file / list_dir / search。
//!
//! 边界：所有路径经 `resolve_in_workspace` 逻辑边界校验（M2 沙箱接管强制力）；
//! 遍历用 `ignore` crate（ripgrep 引擎），自动尊重 .gitignore 与隐藏文件规则。

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;

use crate::tool_router::{
    resolve_in_workspace, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

const DEFAULT_READ_BYTES: u64 = 65_536;
const MAX_READ_BYTES: u64 = 1_048_576;
/// 单次 read_file 结果进入上下文的 token 硬上限。
///
/// 为什么按 token 而不是只看字节：1MB 英文约 25–30 万 token，一个文件就能撞穿 128K 窗口，
/// 而字节数看不出这个量级（CJK 与英文的 token/字节比差 3 倍）。超限按行边界截断，并给出
/// 续读行号，模型才知道"没读完"，而不是拿着半截内容以为读完了。
const MAX_READ_TOKENS: usize = 25_000;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_SEARCH_FILE_BYTES: u64 = 1_048_576;

/// 按 `agent_loop::rules::estimate_tokens` 的口径（CJK 1 字符 1 token、ASCII 4 字节 1 token）
/// 找出首个超限的字符起始位置。返回 `None` 表示全文都在预算内。
fn token_cut_index(text: &str, limit: usize) -> Option<usize> {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
        if wide + narrow.div_ceil(4) > limit {
            return Some(idx);
        }
    }
    None
}

fn estimate_tokens(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

fn truncate_to_tokens(text: &str, limit: usize) -> String {
    match token_cut_index(text, limit) {
        Some(cut) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 统计总行数（1 起算的行数，末行无换行也算一行），并给出第 `start_line` 行的起始字节偏移。
///
/// 只顺序扫字节、不整文件驻留内存：越界报总行数需要真实全量行数，
/// 但一个几 MB 的文件不能为了读 20 行就整份塞进内存。
fn count_lines_and_locate(path: &Path, start_line: usize) -> std::io::Result<(usize, u64, Option<u64>)> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut total_lines = 0usize;
    let mut current_line = 1usize;
    let mut position = 0u64;
    let mut start = if start_line == 1 { Some(0u64) } else { None };
    let mut last_byte = None;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            if byte == b'\n' {
                total_lines += 1;
                current_line += 1;
                if start.is_none() && current_line == start_line {
                    start = Some(position + 1);
                }
            }
            position += 1;
            last_byte = Some(byte);
        }
    }
    if position > 0 && last_byte != Some(b'\n') {
        total_lines += 1;
    }
    Ok((total_lines, position, start))
}

fn strip_line_ending(raw: &[u8]) -> &[u8] {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    raw.strip_suffix(b"\r").unwrap_or(raw)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCut {
    /// 已读满调用方指定的行数。
    LineCount,
    /// 达到 max_bytes 上限。
    Bytes,
    /// 达到单次结果的 token 上限。
    Tokens,
}

struct WindowLine {
    number: usize,
    text: String,
}

struct LineWindow {
    lines: Vec<WindowLine>,
    /// 本次实际读出的原始字节（用于读状态登记；可能与显示内容不等长）。
    consumed: Vec<u8>,
    /// 文件在本次窗口之后还有内容。
    more_below: bool,
    cut: Option<WindowCut>,
    /// 首行本身超限，只显示了开头部分（无法按行续读）。
    partial_line: bool,
}

/// 从 `start_byte`（必须是行首）按行读取，最多 `max_lines` 行 / `max_bytes` 字节 / token 上限。
fn read_line_window(
    path: &Path,
    start_byte: u64,
    start_line: usize,
    max_lines: Option<usize>,
    max_bytes: u64,
    file_len: u64,
) -> std::io::Result<LineWindow> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_byte))?;
    let mut reader = BufReader::new(file.take(max_bytes));
    let mut lines: Vec<WindowLine> = Vec::new();
    let mut consumed: Vec<u8> = Vec::new();
    let mut tokens = 0usize;
    let mut cut = None;
    let mut partial_line = false;
    let mut line_no = start_line;
    loop {
        if max_lines.is_some_and(|limit| lines.len() >= limit) {
            cut = Some(WindowCut::LineCount);
            break;
        }
        let mut raw = Vec::new();
        let read = reader.read_until(b'\n', &mut raw)?;
        if read == 0 {
            break;
        }
        let ended_with_newline = raw.last() == Some(&b'\n');
        let more_file = start_byte + (consumed.len() as u64) + (read as u64) < file_len;
        if !ended_with_newline && more_file {
            // max_bytes 把当前行切断了：只剩半行时不把半行内容喂给模型，
            // 但首行例外——否则 max_bytes 太小会返回空结果。
            if lines.is_empty() {
                let text = String::from_utf8_lossy(strip_line_ending(&raw)).to_string();
                let prefix = format!("{line_no}: ");
                let budget = MAX_READ_TOKENS.saturating_sub(estimate_tokens(&prefix));
                lines.push(WindowLine {
                    number: line_no,
                    text: truncate_to_tokens(&text, budget),
                });
                consumed.extend_from_slice(&raw);
                partial_line = true;
            }
            cut = Some(WindowCut::Bytes);
            break;
        }
        let text = String::from_utf8_lossy(strip_line_ending(&raw)).to_string();
        let prefix = format!("{line_no}: ");
        let cost = estimate_tokens(&prefix) + estimate_tokens(&text);
        if tokens + cost > MAX_READ_TOKENS {
            if lines.is_empty() {
                let budget = MAX_READ_TOKENS.saturating_sub(estimate_tokens(&prefix));
                lines.push(WindowLine {
                    number: line_no,
                    text: truncate_to_tokens(&text, budget),
                });
                consumed.extend_from_slice(&raw);
                partial_line = true;
            }
            cut = Some(WindowCut::Tokens);
            break;
        }
        tokens += cost;
        consumed.extend_from_slice(&raw);
        lines.push(WindowLine { number: line_no, text });
        line_no += 1;
    }
    let more_below = start_byte + (consumed.len() as u64) < file_len;
    Ok(LineWindow { lines, consumed, more_below, cut, partial_line })
}

/// 目录预览（read_file 误传目录时给出可行动的事实）。
fn dir_preview(path: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(path) else {
        return "（无法读取目录）".to_string();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return "（空目录）".to_string();
    }
    names.truncate(5);
    names.join("、")
}

fn positive_int_arg(args: &Value, name: &str) -> Result<Option<u64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::InvalidArgs(format!("{name} 必须是正整数，实际收到 {value}"))),
    }
}

// ============================== read_file ==============================

pub struct ReadFile {
    spec: ToolSpec,
}

impl Default for ReadFile {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadFile {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "read_file",
                "按行读取工作区内文本文件，返回内容带行号前缀（如 `12: 内容`）。\
                 offset 是 1 起算的行号（不是字节偏移），limit 是最多读多少行。\
                 结尾括号给出总行数与下一次的 offset；越界会报出文件总行数与大小。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "工作区相对路径（或绝对路径）" },
                        "offset": { "type": "integer", "description": "从第几行开始读，1 起算的行号（不是字节偏移），默认 1" },
                        "limit": { "type": "integer", "description": "最多读多少行；省略则读到文件末尾（受 max_bytes 与 token 上限约束）" },
                        "max_bytes": { "type": "integer", "description": "本次最多读取的字节数，默认 65536，上限 1048576" }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 path".into()))?;
        // 语义变更说明：offset 曾经是字节偏移，模型普遍当行号用。现在它就是 1 起算的行号，
        // 不再有「拒绝 offset」的兼容分支——旧用法本来就不成立，改名只会违背模型先验。
        let offset = positive_int_arg(&args, "offset")?.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::InvalidArgs(
                "offset 从 1 起算（第 1 行 = 1），不是字节偏移".to_string(),
            ));
        }
        let limit = positive_int_arg(&args, "limit")?;
        if limit == Some(0) {
            return Err(ToolError::InvalidArgs(
                "limit 至少为 1；要读到文件末尾请省略该参数".to_string(),
            ));
        }
        let start_line = offset;
        let max_lines: Option<usize> = limit
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX));
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_READ_BYTES)
            .clamp(1, MAX_READ_BYTES);

        let full = match resolve_in_workspace(&ctx.workspace_root, path) {
            Ok(full) => full,
            Err(ToolError::InvalidArgs(_)) => {
                return Err(ToolError::InvalidArgs(format!(
                    "文件不存在: {path}（路径须位于工作区内；要列目录请用 list_dir）"
                )))
            }
            Err(other) => return Err(other),
        };
        let meta = std::fs::metadata(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        if meta.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "read_file 只能读文件：{path} 是目录。请改用 list_dir 查看，前 5 项：{}",
                dir_preview(&full)
            )));
        }
        if !meta.is_file() {
            return Err(ToolError::InvalidArgs(format!(
                "不是普通文件: {path}（既不是文件也不是目录）"
            )));
        }

        let start_line_usize = usize::try_from(start_line).unwrap_or(usize::MAX);
        let (total_lines, file_len, start_byte) = count_lines_and_locate(&full, start_line_usize)
            .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        // 空文件 + offset=1 是正常 EOF（对齐 dsh：total 0 不算越界），只有从这里往后才算越界。
        let Some(start_byte) = start_byte else {
            return Err(ToolError::Execution(format!(
                "读取越界：{path} 共 {total_lines} 行（{}），请求 offset={offset}（第 {offset} 行起），超出文件末尾",
                human_bytes(file_len)
            )));
        };

        let window = read_line_window(
            &full,
            start_byte,
            start_line_usize,
            max_lines,
            max_bytes,
            file_len,
        )
        .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;

        // SH-124①：记录读状态（会话 + 路径），写工具落盘前据此判定「没读就改」。
        // 只有从文件开头读、且没有截断在行中间时，读到的字节才是文件字节前缀；
        // 从中间行读或半行截断时改为登记整文件哈希，否则后续 apply_patch 会被误判为「未读先改」。
        if let Some(reads) = &ctx.reads {
            let stamp = if start_byte == 0 && !window.partial_line {
                crate::tool_router::FileStamp::of_read(&window.consumed, &meta)
            } else {
                crate::tool_router::FileStamp::of_full_file(&full).unwrap_or_else(|_| {
                    crate::tool_router::FileStamp::of_read(&window.consumed, &meta)
                })
            };
            reads.record(ctx.session_id.as_deref(), &full, stamp);
        }

        let last_line = window
            .lines
            .last()
            .map(|line| line.number)
            .unwrap_or(start_line_usize);
        let body = window
            .lines
            .iter()
            .map(|line| format!("{}: {}", line.number, line.text))
            .collect::<Vec<_>>()
            .join("\n");
        // 结尾括号是模型据以续读的唯一锚点（对齐 dsh read-render 的三态 footer）：
        // 读完给总行数，未完给区间 + 可直接照抄的 offset，单行超限则明说不能按行续读。
        let footer = if window.partial_line {
            let reason = match window.cut {
                Some(WindowCut::Bytes) => format!("max_bytes={max_bytes}"),
                _ => format!("单次 {MAX_READ_TOKENS} token 上限"),
            };
            format!(
                "（已截断：第 {last_line} 行单行内容超出本次上限（{reason}），只显示了开头；\
                 剩余内容不能按行续读，请先用 search 定位更具体的片段）"
            )
        } else if window.more_below {
            let next = last_line + 1;
            match window.cut {
                Some(WindowCut::LineCount) => format!(
                    "（已显示第 {start_line}–{last_line} 行，共 {total_lines} 行。继续读：offset={next}）"
                ),
                reason => {
                    let reason = match reason {
                        Some(WindowCut::Bytes) => format!("达到 max_bytes={max_bytes} 上限"),
                        _ => format!("达到单次 {MAX_READ_TOKENS} token 上限"),
                    };
                    format!(
                        "（已截断：显示第 {start_line}–{last_line} 行，共 {total_lines} 行，{reason}。继续读：offset={next}）"
                    )
                }
            }
        } else {
            format!("（文件结束：共 {total_lines} 行）")
        };
        let output = if body.is_empty() {
            footer
        } else {
            format!("{body}\n\n{footer}")
        };
        Ok(ToolOutput::ok(output))
    }
}

// ============================== list_dir ==============================

pub struct ListDir {
    spec: ToolSpec,
}

impl Default for ListDir {
    fn default() -> Self {
        Self::new()
    }
}

impl ListDir {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "list_dir",
                "列出工作区内目录的内容（名称/类型/大小），最多 500 条。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "目录路径，默认工作区根" }
                    }
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for ListDir {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let full = resolve_in_workspace(&ctx.workspace_root, path)?;
        if !full.is_dir() {
            return Err(ToolError::InvalidArgs(format!("不是目录: {path}")));
        }
        let mut entries = std::fs::read_dir(&full)
            .map_err(|e| ToolError::Execution(format!("读取目录失败: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::Execution(format!("读取目录失败: {e}")))?;
        entries.sort_by_key(|e| e.file_name());
        let mut lines = Vec::new();
        for entry in entries.iter().take(MAX_LIST_ENTRIES) {
            let name = entry.file_name().to_string_lossy().to_string();
            let line = match entry.file_type() {
                Ok(t) if t.is_dir() => format!("{name}/"),
                Ok(t) if t.is_file() => {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    format!("{name}  ({size} B)")
                }
                _ => name,
            };
            lines.push(line);
        }
        let hidden_note = if entries.len() > MAX_LIST_ENTRIES {
            format!("\n[其余 {} 项未显示]", entries.len() - MAX_LIST_ENTRIES)
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(format!("{}\n{}", lines.join("\n"), hidden_note)))
    }
}

// ============================== search ==============================

pub struct Search {
    spec: ToolSpec,
}

impl Default for Search {
    fn default() -> Self {
        Self::new()
    }
}

impl Search {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "search",
                "在工作区文件内容中搜索（字面量或正则），自动尊重 .gitignore，最多返回 50 条。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索内容（默认字面量，is_regex=true 时为正则）" },
                        "path": { "type": "string", "description": "搜索根目录，默认工作区根" },
                        "is_regex": { "type": "boolean", "description": "query 是否为正则表达式" },
                        "max_results": { "type": "integer", "description": "最大结果数，默认 50" }
                    },
                    "required": ["query"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for Search {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 query".into()))?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let is_regex = args.get("is_regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_SEARCH_RESULTS as u64)
            .clamp(1, 200) as usize;

        let pattern = if is_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let re = regex::Regex::new(&pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("正则非法: {e}")))?;
        let root = resolve_in_workspace(&ctx.workspace_root, path)?;

        let mut results = Vec::new();
        let mut total_matches = 0usize;
        for entry in WalkBuilder::new(&root).build() {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > MAX_SEARCH_FILE_BYTES {
                continue; // 大文件跳过（二进制/日志防御）
            }
            let content = match std::fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let text = String::from_utf8_lossy(&content);
            let display_path = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/"); // 统一 POSIX 风格分隔符（模型消费友好）
            for (idx, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    total_matches += 1;
                    if results.len() < max_results {
                        results.push(format!("{}:{}: {}", display_path, idx + 1, line.trim()));
                    }
                    if total_matches >= max_results * 4 {
                        break; // 防御：命中过多时提前收敛
                    }
                }
            }
            if results.len() >= max_results {
                break;
            }
        }
        if results.is_empty() {
            return Ok(ToolOutput::ok(format!("无匹配: {query}")));
        }
        let more = if total_matches > results.len() {
            format!("\n[共约 {total_matches} 处命中，仅显示前 {} 条]", results.len())
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(format!("{}\n{}", results.join("\n"), more)))
    }
}

/// 注册全部只读工具（保留独立入口；全量注册用 tools::register_builtin）。
pub fn register_all(router: &mut crate::tool_router::ToolRouter) {
    router.register(std::sync::Arc::new(ReadFile::new()));
    router.register(std::sync::Arc::new(ListDir::new()));
    router.register(std::sync::Arc::new(Search::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_router::ToolRouter;
    use std::path::Path;
    use std::sync::Arc;

    fn workspace() -> crate::test_support::TestTempDir {
        let dir = crate::test_support::TestTempDir::new("fs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("README.md"), "hello shaniu\nsecond line\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        dir
    }

    fn ctx(ws: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK.get_or_init(|| Arc::new(crate::events::NullEventSink) as Arc<dyn crate::events::EventSink>).clone();
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call", sink, crate::config::SandboxMode::WorkspaceWrite)
    }

    #[tokio::test]
    async fn read_file_returns_content() {
        let ws = workspace();
        let out = ReadFile::new()
            .execute(serde_json::json!({"path": "README.md"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(
            out.output.starts_with("1: hello shaniu\n2: second line"),
            "读取结果必须带行号前缀，实际: {}",
            out.output
        );
        assert!(
            out.output.contains("（文件结束：共 2 行）"),
            "读完要给总行数 footer，实际: {}",
            out.output
        );
    }

    /// 证伪旧的字节偏移语义：offset=3 必须落在第 3 行上（不是第 3 个字节）。
    #[tokio::test]
    async fn read_file_offset_points_at_requested_line() {
        let ws = workspace();
        std::fs::write(ws.join("three.txt"), "alpha\nbravo\ncharlie\n").unwrap();
        let out = ReadFile::new()
            .execute(
                serde_json::json!({"path": "three.txt", "offset": 3, "limit": 1}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert_eq!(
            out.output.lines().next().unwrap_or(""),
            "3: charlie",
            "offset=3 必须是第 3 行，实际: {}",
            out.output
        );
        assert!(
            out.output.contains("（文件结束：共 3 行）"),
            "第 3 行就是末行，footer 必须这样说，实际: {}",
            out.output
        );
    }

    /// 空文件 offset=1 是正常 EOF（对齐 dsh），不能报错。
    #[tokio::test]
    async fn read_file_empty_file_is_normal_eof() {
        let ws = workspace();
        std::fs::write(ws.join("empty.txt"), "").unwrap();
        let out = ReadFile::new()
            .execute(serde_json::json!({"path": "empty.txt"}), &ctx(&ws))
            .await
            .unwrap();
        assert_eq!(out.output, "（文件结束：共 0 行）");
        // offset 超过空文件的行数仍要报越界，并说明文件有几行。
        let err = ReadFile::new()
            .execute(serde_json::json!({"path": "empty.txt", "offset": 2}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("共 0 行"), "{err}");
        assert!(err.to_string().contains("请求 offset=2"), "{err}");
    }

    /// 越界必须报总行数与文件大小，而不是空结果或原样回显。
    #[tokio::test]
    async fn read_file_out_of_range_reports_total_lines() {
        let ws = workspace();
        std::fs::write(ws.join("three.txt"), "a\nb\nc\n").unwrap();
        let err = ReadFile::new()
            .execute(serde_json::json!({"path": "three.txt", "offset": 120}), &ctx(&ws))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("共 3 行"), "{message}");
        assert!(message.contains("请求 offset=120"), "{message}");
        assert!(message.contains("6 B"), "{message}");
    }

    /// 请求范围部分越界时，footer 给真实总行数（不再单开一条「注意」）。
    #[tokio::test]
    async fn read_file_partial_overrun_states_actual_range() {
        let ws = workspace();
        std::fs::write(ws.join("three.txt"), "a\nb\nc\n").unwrap();
        let out = ReadFile::new()
            .execute(
                serde_json::json!({"path": "three.txt", "offset": 2, "limit": 10}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(out.output.contains("2: b"), "{}", out.output);
        assert!(out.output.contains("3: c"), "{}", out.output);
        assert!(out.output.contains("共 3 行"), "{}", out.output);
        assert!(out.output.contains("文件结束"), "{}", out.output);
    }

    /// offset=0 不是合法行号，必须明确拒绝（不能猜成第 1 行）。
    #[tokio::test]
    async fn read_file_rejects_offset_zero() {
        let ws = workspace();
        let err = ReadFile::new()
            .execute(
                serde_json::json!({"path": "README.md", "offset": 0}),
                &ctx(&ws),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        assert!(err.to_string().contains("1 起算"), "{err}");
    }

    /// 目录误传给 read_file 时要说明并给出前几项，而不是一句「不是文件」。
    #[tokio::test]
    async fn read_file_on_directory_lists_preview() {
        let ws = workspace();
        let err = ReadFile::new()
            .execute(serde_json::json!({"path": "src"}), &ctx(&ws))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("是目录"), "{message}");
        assert!(message.contains("main.rs"), "{message}");
    }

    /// 文件不存在必须明确说，而不是含混的「路径不存在」。
    #[tokio::test]
    async fn read_file_missing_file_is_explicit() {
        let ws = workspace();
        let err = ReadFile::new()
            .execute(serde_json::json!({"path": "nope.txt"}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("文件不存在: nope.txt"), "{err}");
    }

    /// 从中间行读取后，登记的是整文件哈希；否则后续 apply_patch 会被误判「没读就改」。
    #[tokio::test]
    async fn read_file_from_middle_registers_whole_file_stamp() {
        let ws = workspace();
        let reads = std::sync::Arc::new(crate::tool_router::ReadRegistry::new());
        ReadFile::new()
            .execute(
                serde_json::json!({"path": "README.md", "offset": 2}),
                &ctx(&ws).with_reads(reads.clone()),
            )
            .await
            .unwrap();
        let canonical = crate::tool_router::canonical_key(&ws.join("README.md"));
        let stamp = reads.get(None, &canonical).expect("必须登记读状态");
        let file_len = std::fs::metadata(ws.join("README.md")).unwrap().len();
        assert_eq!(stamp.len, file_len);
        assert_eq!(stamp.hashed_len, file_len);
    }

    #[tokio::test]
    async fn read_file_records_state_for_writers() {
        let ws = workspace();
        let reads = std::sync::Arc::new(crate::tool_router::ReadRegistry::new());
        ReadFile::new()
            .execute(
                serde_json::json!({"path": "README.md"}),
                &ctx(&ws).with_reads(reads.clone()),
            )
            .await
            .unwrap();
        let canonical = crate::tool_router::canonical_key(&ws.join("README.md"));
        assert!(reads.get(None, &canonical).is_some(), "读成功必须登记读状态");
    }

    #[tokio::test]
    async fn read_file_truncates_large_file() {
        let ws = workspace();
        std::fs::write(ws.join("big.txt"), "x".repeat(200_000)).unwrap();
        let out = ReadFile::new()
            .execute(serde_json::json!({"path": "big.txt", "max_bytes": 1000}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("已截断"));
        assert!(out.output.len() < 2000);
        assert!(
            out.output.contains("不能按行续读"),
            "单行被字节上限切断时要说明无法按行续读: {}",
            out.output
        );
    }

    /// 只按字节截断挡不住窗口：1MB 英文 ≈ 25–30 万 token。30k 个 CJK 字符（≈30k token）
    /// 在 1MB 字节上限内，必须被 token 上限截断。
    #[tokio::test]
    async fn read_file_caps_result_by_tokens() {
        let ws = workspace();
        std::fs::write(ws.join("wide.txt"), "中".repeat(30_000)).unwrap();
        let out = ReadFile::new()
            .execute(
                serde_json::json!({"path": "wide.txt", "max_bytes": 1_048_576}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(out.output.contains("已截断"), "30k token 必须被截断");
        assert!(
            out.output.contains("token 上限"),
            "截断原因必须是 token 上限: {}",
            out.output
        );
        assert!(
            out.output.chars().count() < 26_000,
            "截断后（含标注）必须回到 token 上限附近，实际 {} 字符",
            out.output.chars().count()
        );
    }

    /// 续读必须从下一行接上：既不能重复上一段，也不能跳过中间内容。
    #[tokio::test]
    async fn read_file_offset_resumes_without_gap() {
        let ws = workspace();
        let content: String = (1..=5_000).map(|n| format!("line{n}\n")).collect();
        std::fs::write(ws.join("long.txt"), &content).unwrap();
        let first = ReadFile::new()
            .execute(
                serde_json::json!({"path": "long.txt", "max_bytes": 2000}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        let next: usize = first
            .output
            .rsplit("offset=")
            .next()
            .unwrap()
            .trim_end_matches('）')
            .parse()
            .expect("截断标注里必须能解析出下一次 offset");
        let last_no: usize = first
            .output
            .lines()
            .rfind(|line| line.contains(": line"))
            .and_then(|line| line.split(':').next())
            .and_then(|number| number.parse().ok())
            .expect("首段必须带行号");
        assert_eq!(next, last_no + 1, "续读行号必须紧接上一段末行");

        let rest = ReadFile::new()
            .execute(
                serde_json::json!({"path": "long.txt", "offset": next}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(
            rest.output.starts_with(&format!("{next}: line{next}\n")),
            "续读第一行必须是第 {next} 行，实际: {}",
            rest.output.lines().next().unwrap_or("")
        );
    }

    #[tokio::test]
    async fn read_file_rejects_out_of_workspace() {
        let ws = workspace();
        // ".." 真实存在：canonicalize 成功后触发越界判定（而非路径不存在）
        let err = ReadFile::new()
            .execute(serde_json::json!({"path": ".."}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn read_file_missing_path_is_invalid_args() {
        let ws = workspace();
        let err = ReadFile::new()
            .execute(serde_json::json!({}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn list_dir_lists_entries() {
        let ws = workspace();
        let out = ListDir::new()
            .execute(serde_json::json!({}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("README.md"));
        assert!(out.output.contains("src/"));
    }

    #[tokio::test]
    async fn search_finds_content_across_files() {
        let ws = workspace();
        let out = Search::new()
            .execute(serde_json::json!({"query": "hello"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("README.md:1"));
        assert!(out.output.contains("src/main.rs:2"));
    }

    #[tokio::test]
    async fn search_regex_mode_and_no_match() {
        let ws = workspace();
        let out = Search::new()
            .execute(serde_json::json!({"query": "^second", "is_regex": true}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("README.md:2"));
        let none = Search::new()
            .execute(serde_json::json!({"query": "不存在的字符串xyz"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(none.output.contains("无匹配"), "{}", none.output);
    }

    #[tokio::test]
    async fn registered_router_executes_fs_tools() {
        let ws = workspace();
        let mut router = ToolRouter::new();
        register_all(&mut router);
        assert_eq!(router.specs().len(), 3);
        let out = router
            .execute("list_dir", serde_json::json!({"path": "src"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("main.rs"));
    }
}
