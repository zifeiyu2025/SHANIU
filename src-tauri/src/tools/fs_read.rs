//! 只读文件工具（docs §8 tools/fs_read.rs）：read_file / list_dir / search。
//!
//! 边界：所有路径经 `resolve_in_workspace` 逻辑边界校验（M2 沙箱接管强制力）；
//! 遍历用 `ignore` crate（ripgrep 引擎），自动尊重 .gitignore 与隐藏文件规则。

use std::io::{Read, Seek, SeekFrom};

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
/// 续读偏移，模型才知道"没读完"，而不是拿着半截内容以为读完了。
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
                "读取工作区内文本文件的内容。超长文件会按 token 上限截断，并给出续读 offset。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "工作区相对路径（或绝对路径）" },
                        "max_bytes": { "type": "integer", "description": "最大读取字节数，默认 65536，上限 1048576" },
                        "offset": { "type": "integer", "description": "从第几个字节开始读，默认 0；续读时填上次截断标注里的 offset" }
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
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_READ_BYTES)
            .clamp(1, MAX_READ_BYTES);
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let full = resolve_in_workspace(&ctx.workspace_root, path)?;
        let meta = std::fs::metadata(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        if !meta.is_file() {
            return Err(ToolError::InvalidArgs(format!("不是文件: {path}")));
        }
        if offset >= meta.len() && offset > 0 {
            return Ok(ToolOutput::ok(format!(
                "[已到文件末尾：文件共 {} 字节，offset={offset} 之后无内容]",
                meta.len()
            )));
        }
        let limit = max_bytes.min(meta.len() - offset) as usize;
        let mut file = std::fs::File::open(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        }
        let mut buffer = Vec::with_capacity(limit);
        file.take(limit as u64)
            .read_to_end(&mut buffer)
            .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        let mut text = String::from_utf8_lossy(&buffer).to_string();
        // SH-124①：记录读状态（会话 + 路径），写工具落盘前据此判定「没读就改」。
        if let Some(reads) = &ctx.reads {
            reads.record(
                ctx.session_id.as_deref(),
                &full,
                crate::tool_router::FileStamp::of_read(&buffer, &meta),
            );
        }
        // 原始字节与解码后字节只有在合法 UTF-8 时才一一对应；非法序列会被替换成 U+FFFD，
        // 此时不给续读偏移（宁可让模型用 max_bytes 分段，也不给一个错位的 offset）。
        let exact_utf8 = std::str::from_utf8(&buffer).is_ok();
        let mut consumed = buffer.len();
        let mut token_capped = false;
        if let Some(cut) = token_cut_index(&text, MAX_READ_TOKENS) {
            // 按行边界收口（不切半行）；单行本身超限时只能按字符切。
            let cut = text[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
            consumed = if exact_utf8 { cut } else { buffer.len() };
            text.truncate(cut);
            token_capped = true;
        }
        let next = offset + consumed as u64;
        let note = if token_capped && exact_utf8 {
            format!(
                "\n\n[已截断：文件共 {} 字节，本次显示第 {offset}–{next} 字节（≤{MAX_READ_TOKENS} tokens）。\
                 继续读：read_file path=\"{path}\" offset={next}]",
                meta.len()
            )
        } else if token_capped {
            format!(
                "\n\n[已截断：文件共 {} 字节；文件含非法 UTF-8 字节，无法给出可靠续读偏移，\
                 请用 max_bytes 分段读取]",
                meta.len()
            )
        } else if next < meta.len() {
            format!(
                "\n\n[已截断：文件共 {} 字节，本次显示第 {offset}–{next} 字节。\
                 继续读：read_file path=\"{path}\" offset={next}]",
                meta.len()
            )
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(format!("{text}{note}")))
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
        assert!(out.output.starts_with("hello shaniu"));
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
        assert!(out.output.contains("[已截断"));
        assert!(out.output.len() < 2000);
    }

    /// 只按字节截断挡不住窗口：1MB 英文 ≈ 25–30 万 token。30k 个 CJK 字符（≈30k token）
    /// 在 1MB 字节上限内，必须被 token 上限截断并给出续读偏移。
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
        assert!(out.output.contains("[已截断"), "30k token 必须被截断");
        assert!(out.output.contains("offset="), "截断必须给出续读偏移");
        assert!(
            out.output.chars().count() < 26_000,
            "截断后（含标注）必须回到 token 上限附近，实际 {} 字符",
            out.output.chars().count()
        );
    }

    /// 续读必须从精确偏移接上：既不能重复上一段，也不能跳过中间内容。
    #[tokio::test]
    async fn read_file_offset_resumes_without_gap() {
        let ws = workspace();
        let content: String = ('a'..='z').cycle().take(200_000).collect();
        std::fs::write(ws.join("long.txt"), &content).unwrap();
        let first = ReadFile::new()
            .execute(
                serde_json::json!({"path": "long.txt", "max_bytes": 65_536}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        let offset: usize = first
            .output
            .rsplit("offset=")
            .next()
            .unwrap()
            .trim_end_matches(']')
            .parse()
            .expect("截断标注里必须能解析出 offset");
        assert_eq!(offset, 65_536);

        let rest = ReadFile::new()
            .execute(serde_json::json!({"path": "long.txt", "offset": offset}), &ctx(&ws))
            .await
            .unwrap();
        assert!(
            rest.output.starts_with(&content[offset..offset + 16]),
            "续读内容必须从 offset 处接上"
        );
        assert!(rest.output.contains("[已截断"), "剩余内容仍超默认字节上限，应继续标注");
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
        assert!(none.output.contains("无匹配"));
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
