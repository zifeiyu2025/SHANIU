//! apply_patch（docs §8 / ADR-004）：结构化补丁——禁止任意写文件绕过审批。
//!
//! M1 支持的 V4A 子集（codex 补丁格式）：
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +新文件内容
//! *** Update File: <path>
//!  上下文行
//! -要删除的行
//! +要新增的行
//! *** Delete File: <path>
//! *** End Patch
//! ```
//! 流程：解析 → dry-run 校验（路径边界/存在性/上下文匹配）→ 应用；
//! 任一步失败即回滚全部已写入文件（内存快照恢复），错误回填模型。

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::tool_router::{
    resolve_for_write, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

#[derive(Debug, Clone, PartialEq, Eq)]
enum HunkLine {
    Keep(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOp {
    Add { path: String, lines: Vec<String> },
    Update { path: String, seq: Vec<HunkLine> },
    Delete { path: String },
}

pub struct ApplyPatch {
    spec: ToolSpec,
}

impl Default for ApplyPatch {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplyPatch {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "apply_patch",
                "对工作区文件应用结构化补丁（V4A 格式：Add/Update/Delete File）。上下文先精确匹配，失败时依次尝试剥离行号前缀、缩进弹性、空白折叠、首尾锚点模糊定位；整单失败自动回滚。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "以 *** Begin Patch 开头、*** End Patch 结尾的补丁文本"
                        }
                    },
                    "required": ["patch"]
                }),
            ),
        }
    }
}

// ============================== 解析（纯函数） ==============================

fn parse_patch(patch: &str) -> Result<Vec<PatchOp>, ToolError> {
    let trimmed = patch.trim_start();
    if !trimmed.starts_with(BEGIN) {
        return Err(ToolError::InvalidArgs(
            "补丁必须以 *** Begin Patch 开头".into(),
        ));
    }
    let body = &trimmed[BEGIN.len()..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    let lines: Vec<&str> = body.lines().collect();

    let mut ops: Vec<PatchOp> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == END {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("*** ") {
                    break;
                }
                let Some(rest) = l.strip_prefix('+') else {
                    return Err(ToolError::InvalidArgs(format!(
                        "Add File 段的行必须以 + 开头: {l:?}"
                    )));
                };
                content.push(rest.to_string());
                i += 1;
            }
            ops.push(PatchOp::Add { path: path.trim().to_string(), lines: content });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut seq: Vec<HunkLine> = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                if l == "@@" {
                    i += 1;
                    continue; // 分组标记在连续匹配语义下不改变序列
                }
                if let Some(rest) = l.strip_prefix(' ') {
                    seq.push(HunkLine::Keep(rest.to_string()));
                } else if let Some(rest) = l.strip_prefix('-') {
                    seq.push(HunkLine::Remove(rest.to_string()));
                } else if let Some(rest) = l.strip_prefix('+') {
                    seq.push(HunkLine::Add(rest.to_string()));
                } else if l.is_empty() {
                    seq.push(HunkLine::Keep(String::new()));
                } else {
                    return Err(ToolError::InvalidArgs(format!(
                        "Update File 段的行必须以空格/-/+ 开头: {l:?}"
                    )));
                }
                i += 1;
            }
            if seq.iter().all(|h| matches!(h, HunkLine::Keep(_))) {
                return Err(ToolError::InvalidArgs(
                    "Update File 需要至少一行删除(+前缀为-)或新增内容".into(),
                ));
            }
            ops.push(PatchOp::Update { path: path.trim().to_string(), seq });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete { path: path.trim().to_string() });
            i += 1;
        } else {
            return Err(ToolError::InvalidArgs(format!("无法识别的补丁行: {line:?}")));
        }
    }
    if ops.is_empty() {
        return Err(ToolError::InvalidArgs("补丁为空".into()));
    }
    Ok(ops)
}

/// 匹配级别（照 ZCode `tool/edit-matchers.ts` 8 级思路 + codex `seek_sequence` 递减严格度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchLevel {
    Exact,
    LineNumberStripped,
    IndentationFlexible,
    WhitespaceCollapsed,
    BlockAnchor,
}

impl MatchLevel {
    fn label(self) -> &'static str {
        match self {
            MatchLevel::Exact => "精确",
            MatchLevel::LineNumberStripped => "剥离行号前缀",
            MatchLevel::IndentationFlexible => "缩进弹性",
            MatchLevel::WhitespaceCollapsed => "空白折叠",
            MatchLevel::BlockAnchor => "首尾锚点",
        }
    }
}

/// 块锚点级别要求首尾行之外的中段平均相似度不低于此值（同 ZCode）。
const BLOCK_ANCHOR_MIN_SIMILARITY: f64 = 0.8;

/// 剥离 Read 输出的行号前缀：`12: x` / `12\tx` / `12→x`（模型照抄行号是高频错误）。
fn strip_line_number_prefix(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    let rest = rest
        .strip_prefix(':')
        .or_else(|| rest.strip_prefix('\t'))
        .or_else(|| rest.strip_prefix('→'))?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// 仅当**全部**上下文/删除行都带行号前缀时才剥离（避免误伤代码里形如 `1: x` 的字面量）。
fn strip_line_number_prefixes(lines: &[String]) -> Option<Vec<String>> {
    lines
        .iter()
        .map(|line| strip_line_number_prefix(line).map(str::to_string))
        .collect()
}

fn leading_indent(line: &str) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    Some(line.len() - line.trim_start_matches([' ', '\t']).len())
}

/// 去掉整块的公共缩进（相对缩进保留）。
fn remove_common_indent(lines: &[String]) -> Vec<String> {
    let min = lines.iter().filter_map(|l| leading_indent(l)).min().unwrap_or(0);
    lines
        .iter()
        .map(|line| match leading_indent(line) {
            Some(_) => line[min..].to_string(),
            None => line.trim_end().to_string(),
        })
        .collect()
}

/// 空白折叠：行首尾裁剪 + 行内连续空白压成一个空格。
fn collapse_whitespace(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn levenshtein(left: &str, right: &str) -> usize {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn line_similarity(left: &str, right: &str) -> f64 {
    if left == right {
        return 1.0;
    }
    let max_len = left.chars().count().max(right.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - levenshtein(left, right) as f64 / max_len as f64
}

/// 返回首个匹配起点（codex 语义：从文件头向下找第一条，不猜多解）。
fn first_match(file: &[String], width: usize, at: impl Fn(usize) -> bool) -> Option<usize> {
    if width == 0 || width > file.len() {
        return None;
    }
    (0..=file.len() - width).find(|&start| at(start))
}

fn anchor_similarity(block: &[String], expected: &[String]) -> f64 {
    if block.len() <= 2 {
        return 1.0;
    }
    let middle = 1..block.len() - 1;
    let count = middle.len();
    let total: f64 = middle
        .map(|i| line_similarity(block[i].trim(), expected[i].trim()))
        .sum();
    total / count as f64
}

/// Update 段：keep+remove 序列在文件中按递减严格度定位，替换为 keep+add。
///
/// Keep 行一律取**文件原文**（保留原缩进/空白），只有 Add 行用补丁文本。
fn apply_hunk_seq(
    file_lines: &[String],
    seq: &[HunkLine],
) -> Result<(Vec<String>, MatchLevel), String> {
    let expected: Vec<String> = seq
        .iter()
        .filter_map(|h| match h {
            HunkLine::Keep(s) | HunkLine::Remove(s) => Some(s.clone()),
            HunkLine::Add(_) => None,
        })
        .collect();
    if expected.is_empty() {
        return Err("Update 段需要至少一行上下文或删除行".into());
    }
    let width = expected.len();
    if file_lines.len() < width {
        return Err(format!(
            "上下文不匹配：文件仅 {} 行，补丁期望 {width} 行",
            file_lines.len()
        ));
    }

    let found: Option<(usize, MatchLevel)> = None
        .or_else(|| {
            first_match(file_lines, width, |start| file_lines[start..start + width] == expected[..])
                .map(|start| (start, MatchLevel::Exact))
        })
        .or_else(|| {
            let stripped = strip_line_number_prefixes(&expected)?;
            first_match(file_lines, width, |start| {
                file_lines[start..start + width] == stripped[..]
            })
            .map(|start| (start, MatchLevel::LineNumberStripped))
        })
        .or_else(|| {
            let normalized = remove_common_indent(&expected);
            first_match(file_lines, width, |start| {
                remove_common_indent(&file_lines[start..start + width]) == normalized
            })
            .map(|start| (start, MatchLevel::IndentationFlexible))
        })
        .or_else(|| {
            let collapsed: Vec<String> = expected.iter().map(|l| collapse_whitespace(l)).collect();
            first_match(file_lines, width, |start| {
                let actual: Vec<String> = file_lines[start..start + width]
                    .iter()
                    .map(|l| collapse_whitespace(l))
                    .collect();
                actual == collapsed
            })
            .map(|start| (start, MatchLevel::WhitespaceCollapsed))
        })
        .or_else(|| {
            if width < 3 {
                return None;
            }
            let first = expected[0].trim();
            let last = expected[width - 1].trim();
            first_match(file_lines, width, |start| {
                let block = &file_lines[start..start + width];
                block[0].trim() == first
                    && block[width - 1].trim() == last
                    && anchor_similarity(block, &expected) >= BLOCK_ANCHOR_MIN_SIMILARITY
            })
            .map(|start| (start, MatchLevel::BlockAnchor))
        });

    let Some((start, level)) = found else {
        return Err(format!(
            "上下文不匹配：未找到补丁期望的 {width} 行序列（已尝试：{}）。请先读取文件当前内容再重试",
            [
                MatchLevel::Exact,
                MatchLevel::LineNumberStripped,
                MatchLevel::IndentationFlexible,
                MatchLevel::WhitespaceCollapsed,
                MatchLevel::BlockAnchor,
            ]
            .iter()
            .map(|l| l.label())
            .collect::<Vec<_>>()
            .join("、")
        ));
    };

    // 缩进弹性级别下，新增行按「文件块公共缩进 - 补丁块公共缩进」补齐缩进，
    // 否则模型整块降级缩进时会把代码插到第 0 列。
    let indent_delta = if level == MatchLevel::IndentationFlexible {
        let file_min = file_lines[start..start + width]
            .iter()
            .filter_map(|l| leading_indent(l))
            .min()
            .unwrap_or(0);
        let patch_min = expected.iter().filter_map(|l| leading_indent(l)).min().unwrap_or(0);
        file_min.saturating_sub(patch_min)
    } else {
        0
    };

    let mut replacement: Vec<String> = Vec::new();
    let mut cursor = start;
    for h in seq {
        match h {
            HunkLine::Keep(_) => {
                replacement.push(file_lines[cursor].clone());
                cursor += 1;
            }
            HunkLine::Remove(_) => cursor += 1,
            HunkLine::Add(s) => {
                if indent_delta > 0 && !s.trim().is_empty() {
                    replacement.push(format!("{}{s}", " ".repeat(indent_delta)));
                } else {
                    replacement.push(s.clone());
                }
            }
        }
    }
    let mut out = file_lines[..start].to_vec();
    out.extend(replacement);
    out.extend(file_lines[start + width..].to_vec());
    Ok((out, level))
}

// ============================== 执行 ==============================

#[async_trait]
impl ToolExecutor for ApplyPatch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let patch = args
            .get("patch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 patch".into()))?;
        let ops = parse_patch(patch)?;

        // dry-run：全部路径先过边界校验
        for op in &ops {
            let path = match op {
                PatchOp::Add { path, .. }
                | PatchOp::Update { path, .. }
                | PatchOp::Delete { path } => path,
            };
            resolve_for_write(&ctx.workspace_root, path)?;
        }

        // 应用 + 回滚快照：path → 原（None=原不存在，回滚=删除）
        let mut snapshot: HashMap<String, Option<Vec<u8>>> = HashMap::new();
        let mut summary: Vec<String> = Vec::new();
        let rollback = |snapshot: &HashMap<String, Option<Vec<u8>>>| {
            for (path, old) in snapshot {
                match old {
                    Some(bytes) => {
                        let _ = std::fs::write(path, bytes);
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        };

        for op in &ops {
            match op {
                PatchOp::Add { path, lines } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)?;
                    if full.exists() {
                        rollback(&snapshot);
                        return Err(ToolError::Execution(format!(
                            "Add File 目标已存在: {path}"
                        )));
                    }
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let mut content = lines.join("\n");
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    if std::fs::write(&full, &content).is_err() {
                        rollback(&snapshot);
                        return Err(ToolError::Execution(format!("写入失败: {path}")));
                    }
                    snapshot.insert(full.to_string_lossy().to_string(), None);
                    summary.push(format!("+ {path}"));
                }
                PatchOp::Update { path, seq } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)?;
                    let original =
                        std::fs::read(&full).map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
                    let text = String::from_utf8(original.clone())
                        .map_err(|_| ToolError::Execution(format!("非 UTF-8 文本文件: {path}")))?;
                    let had_trailing_newline = text.ends_with('\n');
                    let (new_lines, level) = apply_hunk_seq(
                        &text.lines().map(String::from).collect::<Vec<_>>(),
                        seq,
                    )
                    .map_err(|e| {
                        rollback(&snapshot);
                        ToolError::Execution(format!("{path}: {e}"))
                    })?;
                    let mut new_text = new_lines.join("\n");
                    if had_trailing_newline && !new_text.is_empty() {
                        new_text.push('\n');
                    }
                    if std::fs::write(&full, new_text).is_err() {
                        rollback(&snapshot);
                        return Err(ToolError::Execution(format!("写入失败: {path}")));
                    }
                    snapshot.entry(full.to_string_lossy().to_string()).or_insert(Some(original));
                    summary.push(if level == MatchLevel::Exact {
                        format!("~ {path}")
                    } else {
                        format!("~ {path}（{}匹配）", level.label())
                    });
                }
                PatchOp::Delete { path } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)?;
                    let original = match std::fs::read(&full) {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            rollback(&snapshot);
                            return Err(ToolError::Execution(format!(
                                "Delete File 目标不存在: {path}"
                            )));
                        }
                    };
                    if std::fs::remove_file(&full).is_err() {
                        rollback(&snapshot);
                        return Err(ToolError::Execution(format!("删除失败: {path}")));
                    }
                    snapshot.insert(full.to_string_lossy().to_string(), Some(original));
                    summary.push(format!("- {path}"));
                }
            }
        }
        Ok(ToolOutput::ok(format!(
            "补丁应用成功（{} 处）：\n{}",
            summary.len(),
            summary.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_router::ToolContext;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx(ws: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(crate::events::NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call-patch", sink, crate::config::SandboxMode::WorkspaceWrite)
    }

    fn workspace() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-patch-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "line1\nline2\nline3\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn add_creates_new_file() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+第一行\n+第二行\n*** End Patch";
        let out = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap();
        assert!(out.success);
        assert_eq!(std::fs::read_to_string(ws.join("new.txt")).unwrap(), "第一行\n第二行\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn update_replaces_context_block() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+第二行(改)\n line3\n*** End Patch";
        let out = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap();
        assert!(out.success);
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\n第二行(改)\nline3\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        let out = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap();
        assert!(out.success);
        assert!(!ws.join("a.txt").exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn context_mismatch_rolls_back_earlier_ops() {
        let ws = workspace();
        // 先 Add 再 Update（上下文故意错），失败后 Add 的文件必须被回滚
        let patch = "*** Begin Patch\n*** Add File: temp.txt\n+x\n*** Update File: a.txt\n 不存在的上下文\n-y\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!ws.join("temp.txt").exists(), "回滚必须删除本轮新增的文件");
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\nline2\nline3\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn add_existing_file_fails() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\nline2\nline3\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn out_of_workspace_path_rejected() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: ../evil.txt\n+x\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!ws.parent().unwrap().join("evil.txt").exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn malformed_patch_is_invalid_args() {
        let ws = workspace();
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": "随便写点东西"}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    // ---------- SH-123 匹配级别：8 类样本 ----------

    #[tokio::test]
    async fn strips_colon_line_number_prefixes() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n 1: line1\n-2: line2\n+第二行(改)\n 3: line3\n*** End Patch";
        let out = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("剥离行号前缀匹配"), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\n第二行(改)\nline3\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn strips_tab_line_number_prefixes() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n 1\tline1\n-2\tline2\n+第二行(改)\n 3\tline3\n*** End Patch";
        let out = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("剥离行号前缀匹配"), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\n第二行(改)\nline3\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn indentation_flexible_preserves_file_indent() {
        let ws = workspace();
        std::fs::write(ws.join("b.txt"), "        alpha();\n        beta();\n        gamma();\n").unwrap();
        // 补丁整块少 8 个空格（模型抄写时降级缩进）
        let patch = "*** Begin Patch\n*** Update File: b.txt\n alpha();\n-beta();\n+BETA();\n gamma();\n*** End Patch";
        let out = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("缩进弹性匹配"), "{}", out.output);
        // 未改动的行保留文件原缩进；新增行补齐到同级缩进
        assert_eq!(
            std::fs::read_to_string(ws.join("b.txt")).unwrap(),
            "        alpha();\n        BETA();\n        gamma();\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn whitespace_collapsed_matches_internal_spacing() {
        let ws = workspace();
        std::fs::write(ws.join("c.txt"), "let x =    1;\nlet y =\t2;\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: c.txt\n let x = 1;\n-let y = 2;\n+let y = 3;\n*** End Patch";
        let out = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("空白折叠匹配"), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(ws.join("c.txt")).unwrap(),
            "let x =    1;\nlet y = 3;\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn block_anchor_matches_similar_middle_line() {
        let ws = workspace();
        std::fs::write(ws.join("d.txt"), "begin();\nhandle(item_a);\nfinish();\n").unwrap();
        // 中段一行与文件不一致（相似度 >= 0.8），首尾锚点确定位置
        let patch = "*** Begin Patch\n*** Update File: d.txt\n begin();\n-handle(item_b);\n+handle(item_c);\n finish();\n*** End Patch";
        let out = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("首尾锚点匹配"), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(ws.join("d.txt")).unwrap(),
            "begin();\nhandle(item_c);\nfinish();\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn no_match_reports_tried_levels_and_writes_nothing() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: temp2.txt\n+x\n*** Update File: a.txt\n nope1\n-nope2\n nope3\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("已尝试"), "{message}");
        assert!(message.contains("首尾锚点"), "{message}");
        assert!(!ws.join("temp2.txt").exists(), "失败不得留下半截写入");
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\nline2\nline3\n"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn later_update_failure_restores_earlier_update() {
        let ws = workspace();
        std::fs::write(ws.join("c.txt"), "line1\nline2\nline3\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+改过\n line3\n*** Update File: c.txt\n 不存在的上下文\n-x\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\nline2\nline3\n",
            "前一个 Update 必须回滚"
        );
        std::fs::remove_dir_all(&ws).ok();
    }
}
