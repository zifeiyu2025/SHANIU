//! 只读文件工具（docs §8 tools/fs_read.rs）：read_file / list_dir / search。
//!
//! 边界：所有路径经 `resolve_in_workspace` 逻辑边界校验（M2 沙箱接管强制力）；
//! 遍历用 `ignore` crate（ripgrep 引擎），自动尊重 .gitignore 与隐藏文件规则。

use std::io::Read;

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;

use crate::tool_router::{
    resolve_in_workspace, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

const DEFAULT_READ_BYTES: u64 = 65_536;
const MAX_READ_BYTES: u64 = 1_048_576;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_SEARCH_FILE_BYTES: u64 = 1_048_576;

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
                "读取工作区内文本文件的内容。超长文件会被截断。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "工作区相对路径（或绝对路径）" },
                        "max_bytes": { "type": "integer", "description": "最大读取字节数，默认 65536" }
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
        let full = resolve_in_workspace(&ctx.workspace_root, path)?;
        let meta = std::fs::metadata(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        if !meta.is_file() {
            return Err(ToolError::InvalidArgs(format!("不是文件: {path}")));
        }
        let limit = max_bytes.min(meta.len()) as usize;
        let file = std::fs::File::open(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        let mut buffer = Vec::with_capacity(limit);
        file.take(limit as u64)
            .read_to_end(&mut buffer)
            .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
        let text = String::from_utf8_lossy(&buffer);
        // SH-124①：记录读状态（会话 + 路径），写工具落盘前据此判定「没读就改」。
        if let Some(reads) = &ctx.reads {
            reads.record(
                ctx.session_id.as_deref(),
                &full,
                crate::tool_router::FileStamp::of_read(&buffer, &meta),
            );
        }
        let note = if meta.len() > limit as u64 {
            format!("\n\n[已截断：文件共 {} 字节，仅显示前 {limit} 字节]", meta.len())
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
