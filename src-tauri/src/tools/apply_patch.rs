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

/// 解析出的单个段，附带补丁文本里的行号，失败时能指到具体哪一段的哪一行。
#[derive(Debug, Clone)]
struct PatchSection {
    /// 1 起算的段号。
    index: usize,
    /// 段头（`*** Xxx File: ...`）在补丁文本里的行号（1 起算）。
    start_line: usize,
    /// 段内最后一行的行号（1 起算）。
    end_line: usize,
    op: PatchOp,
}

impl PatchSection {
    fn path(&self) -> &str {
        match &self.op {
            PatchOp::Add { path, .. } | PatchOp::Update { path, .. } | PatchOp::Delete { path } => path,
        }
    }

    fn kind(&self) -> &'static str {
        match self.op {
            PatchOp::Add { .. } => "Add File",
            PatchOp::Update { .. } => "Update File",
            PatchOp::Delete { .. } => "Delete File",
        }
    }

    fn label(&self) -> String {
        let span = if self.start_line == self.end_line {
            format!("补丁第 {} 行", self.start_line)
        } else {
            format!("补丁第 {}–{} 行", self.start_line, self.end_line)
        };
        format!("第 {} 段（{}: {}，{span}）", self.index, self.kind(), self.path())
    }
}

fn section_head(index: usize, kind: &str, path: &str, start_line: usize) -> String {
    format!("第 {index} 段（{kind}: {path}，从补丁第 {start_line} 行开始）")
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

fn parse_patch(patch: &str) -> Result<Vec<PatchSection>, ToolError> {
    let lines: Vec<&str> = patch.lines().collect();
    let begin = lines
        .iter()
        .position(|line| line.trim_start().starts_with(BEGIN));
    let Some(begin_idx) = begin else {
        let first = lines.first().copied().unwrap_or("");
        return Err(ToolError::InvalidArgs(format!(
            "补丁格式错误：补丁第 1 行必须是 `*** Begin Patch`，实际是 {first:?}。\
             期望形状：第 1 行 `*** Begin Patch`，随后是 `*** Add File: <路径>` / \
             `*** Update File: <路径>` / `*** Delete File: <路径>` 段，最后一行 `*** End Patch`"
        )));
    };
    if lines[..begin_idx].iter().any(|line| !line.trim().is_empty()) {
        let first = lines.first().copied().unwrap_or("");
        return Err(ToolError::InvalidArgs(format!(
            "补丁格式错误：`*** Begin Patch` 之前不能有非空内容，补丁第 1 行是 {first:?}。\
             期望形状：补丁第 1 行直接写 `*** Begin Patch`"
        )));
    }

    let mut sections: Vec<PatchSection> = Vec::new();
    let mut i = begin_idx + 1;
    let mut saw_end = false;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == END {
            saw_end = true;
            break;
        }
        let index = sections.len() + 1;
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            let start_line = i + 1;
            i += 1;
            let mut content = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let raw = lines[i];
                let Some(rest) = raw.strip_prefix('+') else {
                    return Err(ToolError::InvalidArgs(format!(
                        "补丁格式错误：{} 的补丁第 {} 行必须以 `+` 开头，实际是 {raw:?}。\
                         期望形状：`+<新增内容>`；下一段以 `*** ` 开头",
                        section_head(index, "Add File", &path, start_line),
                        i + 1
                    )));
                };
                content.push(rest.to_string());
                i += 1;
            }
            sections.push(PatchSection {
                index,
                start_line,
                end_line: i.max(start_line),
                op: PatchOp::Add { path, lines: content },
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            let start_line = i + 1;
            i += 1;
            let mut seq: Vec<HunkLine> = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let raw = lines[i];
                if raw == "@@" {
                    i += 1;
                    continue; // 分组标记在连续匹配语义下不改变序列
                }
                if let Some(rest) = raw.strip_prefix(' ') {
                    seq.push(HunkLine::Keep(rest.to_string()));
                } else if let Some(rest) = raw.strip_prefix('-') {
                    seq.push(HunkLine::Remove(rest.to_string()));
                } else if let Some(rest) = raw.strip_prefix('+') {
                    seq.push(HunkLine::Add(rest.to_string()));
                } else if raw.is_empty() {
                    seq.push(HunkLine::Keep(String::new()));
                } else {
                    return Err(ToolError::InvalidArgs(format!(
                        "补丁格式错误：{} 的补丁第 {} 行无法识别，原文是 {raw:?}。\
                         期望形状：上下文行以空格开头（` 原内容`）、删除行以 `-` 开头、新增行以 `+` 开头；\
                         整行留空视为空上下文行",
                        section_head(index, "Update File", &path, start_line),
                        i + 1
                    )));
                }
                i += 1;
            }
            if seq.iter().all(|h| matches!(h, HunkLine::Keep(_))) {
                return Err(ToolError::InvalidArgs(format!(
                    "补丁格式错误：{} 只有上下文行，没有任何 `-` 删除行或 `+` 新增行。\
                     期望形状：至少一行以 `-` 或 `+` 开头",
                    section_head(index, "Update File", &path, start_line)
                )));
            }
            sections.push(PatchSection {
                index,
                start_line,
                end_line: i.max(start_line),
                op: PatchOp::Update { path, seq },
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            let start_line = i + 1;
            sections.push(PatchSection {
                index,
                start_line,
                end_line: start_line,
                op: PatchOp::Delete { path: path.trim().to_string() },
            });
            i += 1;
        } else {
            return Err(ToolError::InvalidArgs(format!(
                "补丁格式错误：补丁第 {} 行无法识别，原文是 {line:?}。\
                 期望形状：`*** Add File: <路径>`、`*** Update File: <路径>`、\
                 `*** Delete File: <路径>` 或 `*** End Patch`",
                i + 1
            )));
        }
    }
    if !saw_end {
        let last = sections
            .last()
            .map(PatchSection::label)
            .unwrap_or_else(|| "（没有识别到任何段）".to_string());
        return Err(ToolError::InvalidArgs(format!(
            "补丁不完整：补丁共 {} 行，到第 {} 行结束仍未出现 `*** End Patch`。\
             最后一个段是 {last}。期望形状：最后一行写 `*** End Patch`",
            lines.len(),
            lines.len()
        )));
    }
    if sections.is_empty() {
        return Err(ToolError::InvalidArgs(
            "补丁为空：`*** Begin Patch` 与 `*** End Patch` 之间没有任何段。\
             期望形状：至少一个 `*** Add File:` / `*** Update File:` / `*** Delete File:` 段"
                .to_string(),
        ));
    }
    Ok(sections)
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

const MISMATCH_EXCERPT_LINES: usize = 8;
const MISMATCH_LINE_CHARS: usize = 160;

fn truncate_for_display(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.chars().count() <= MISMATCH_LINE_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MISMATCH_LINE_CHARS).collect();
    out.push('…');
    out
}

/// 在文件里找与期望块最相似的位置（按整块平均相似度，缺失行记 0 分）。
/// 找不到精确匹配时用来回答「模型以为文件在这里，实际最像的是哪里」。
fn nearest_block(file: &[String], expected: &[String]) -> Option<(usize, f64)> {
    if file.is_empty() || expected.is_empty() {
        return None;
    }
    let width = expected.len();
    let mut best: Option<(usize, f64)> = None;
    for start in 0..file.len() {
        let available = (file.len() - start).min(width);
        let total: f64 = (0..available)
            .map(|offset| {
                line_similarity(expected[offset].trim(), file[start + offset].trim())
            })
            .sum();
        let score = total / width as f64;
        if best.is_none_or(|(_, current)| score > current) {
            best = Some((start, score));
        }
    }
    best
}

/// 上下文不匹配时的「可照着改」对比：期望块原文、文件最近似位置、首个差异行。
fn mismatch_detail(file: &[String], expected: &[String]) -> String {
    let width = expected.len();
    let shown = width.min(MISMATCH_EXCERPT_LINES);
    let mut out = format!("\n期望匹配的 {width} 行（块内序号）：");
    for (offset, line) in expected.iter().take(shown).enumerate() {
        out.push_str(&format!("\n  块内 {}| {}", offset + 1, truncate_for_display(line)));
    }
    if shown < width {
        out.push_str(&format!("\n  …（期望块共 {width} 行，只显示前 {shown} 行）"));
    }
    match nearest_block(file, expected) {
        Some((start, score)) => {
            let available = (file.len() - start).min(width);
            let shown_actual = available.min(MISMATCH_EXCERPT_LINES);
            out.push_str(&format!(
                "\n文件中最接近的位置：第 {} 行起（整块相似度 {:.0}%，文件共 {} 行）：",
                start + 1,
                score * 100.0,
                file.len()
            ));
            for offset in 0..shown_actual {
                out.push_str(&format!(
                    "\n  文件第 {}| {}",
                    start + offset + 1,
                    truncate_for_display(&file[start + offset])
                ));
            }
            if shown_actual < available {
                out.push_str("\n  …");
            }
            if let Some(offset) =
                (0..available).find(|&offset| expected[offset].trim() != file[start + offset].trim())
            {
                out.push_str(&format!(
                    "\n首个不一致：块内第 {} 行 vs 文件第 {} 行；期望 `{}`，实际读到 `{}`",
                    offset + 1,
                    start + offset + 1,
                    truncate_for_display(expected[offset].trim()),
                    truncate_for_display(file[start + offset].trim())
                ));
            }
            if available < width {
                out.push_str(&format!(
                    "\n边界：文件从第 {} 行起只剩 {available} 行，补丁期望 {width} 行，无法完整比对",
                    start + 1
                ));
            }
        }
        None => out.push_str("\n文件为空，没有可比对的位置"),
    }
    out
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
            "上下文不匹配：补丁期望 {width} 行上下文，文件只有 {} 行，无法完整匹配。{}",
            file_lines.len(),
            mismatch_detail(file_lines, &expected)
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
            "上下文不匹配：未找到补丁期望的 {width} 行序列（已尝试：{}）。{}\n\
             请按上面「文件中最接近的位置」的真实内容重建补丁，不要原样重试",
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
            .join("、"),
            mismatch_detail(file_lines, &expected)
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

// ============================== 写前读状态（SH-124） ==============================

const STALE_EXCERPT_LINES: usize = 80;
/// 回填给模型的上限（字节）。几 MB 的文件全文回填会直接吃光上下文，
/// 而模型重建补丁只需要一处接缝附近的真实内容——字节上限与行数上限同时生效，
/// 取先到者（SH-126③）。
const STALE_EXCERPT_BYTES: usize = 8 * 1024;

/// 冲突回填：给出文件当前内容（带行号，行数 + 字节双上限截断），让模型基于真实内容重建补丁。
fn current_excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut truncated = false;
    for (idx, line) in text.lines().enumerate() {
        // 逐行累加：按整行截断才不会切断 UTF-8 边界。
        if idx >= STALE_EXCERPT_LINES || out.len() + line.len() + 8 > STALE_EXCERPT_BYTES {
            truncated = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{}| {line}", idx + 1));
    }
    if truncated {
        out.push_str(&format!(
            "\n…（已截断：最多回填前 {STALE_EXCERPT_LINES} 行 / {STALE_EXCERPT_BYTES} 字节）"
        ));
    }
    out
}

/// 写前校验：必须先读过（同会话 + 同路径），且内容没被外部改过。
/// 返回用于写后刷新登记表的路径键（None = 未启用登记表）。
fn verify_read_state(
    ctx: &ToolContext,
    path: &str,
    full: &std::path::Path,
    current: &[u8],
) -> Result<Option<std::path::PathBuf>, ToolError> {
    let Some(reads) = &ctx.reads else { return Ok(None) };
    let key = crate::tool_router::canonical_key(full);
    let Some(recorded) = reads.get(ctx.session_id.as_deref(), &key) else {
        return Err(ToolError::Execution(format!(
            "write_file_not_read: 未读取过 {path} 就尝试修改。请先用 read_file 读取当前内容，再基于真实内容生成补丁"
        )));
    };
    let meta = std::fs::metadata(full)
        .map_err(|e| ToolError::Execution(format!("读取失败: {e}")))?;
    if !crate::tool_router::content_matches(&recorded, current, &meta) {
        return Err(ToolError::Execution(format!(
            "write_file_stale: {path} 在读取后被外部修改，已拒绝覆盖（本轮未写入该文件）。当前文件内容：\n{}",
            current_excerpt(current)
        )));
    }
    Ok(Some(key))
}

/// 写成功后刷新登记表：模型自此持有的是文件最新内容。
fn refresh_read_state(ctx: &ToolContext, key: Option<std::path::PathBuf>, full: &std::path::Path) {
    let (Some(reads), Some(key)) = (&ctx.reads, key) else { return };
    if let Ok(stamp) = crate::tool_router::FileStamp::of_full_file(full) {
        reads.record(ctx.session_id.as_deref(), &key, stamp);
    }
}

/// 把本次写入登记进写日志（SH-126）：自愈闭环据此判断「是否靠改测试变绿」。
fn record_write(ctx: &ToolContext, full: &std::path::Path, before: Option<&str>, after: Option<&str>) {
    if let Some(writes) = &ctx.writes {
        writes.record(full, before, after);
    }
}

/// 回滚结果：`restored` 已改回，`failed` 未能改回（`路径: 原因`）。
/// 路径排序，保证错误文案稳定、可断言。
#[derive(Default)]
struct RollbackReport {
    restored: Vec<String>,
    failed: Vec<String>,
}

/// 把快照里的文件写回原状（`None` = 原本不存在，回滚即删除）。
fn rollback(snapshot: &HashMap<String, Option<Vec<u8>>>) -> RollbackReport {
    let mut report = RollbackReport::default();
    let mut paths: Vec<&String> = snapshot.keys().collect();
    paths.sort();
    for path in paths {
        let result = match &snapshot[path] {
            Some(bytes) => std::fs::write(path, bytes),
            None => std::fs::remove_file(path),
        };
        match result {
            Ok(()) => report.restored.push(path.clone()),
            Err(error) => report.failed.push(format!("{path}: {error}")),
        }
    }
    report
}

/// 给错误加上「第几段 / 补丁第几行」。
fn with_section(error: ToolError, section: &PatchSection) -> ToolError {
    let label = section.label();
    match error {
        ToolError::InvalidArgs(message) => ToolError::InvalidArgs(format!("{label}：{message}")),
        ToolError::Execution(message) => ToolError::Execution(format!("{label}：{message}")),
        ToolError::SandboxDenied(message) => ToolError::SandboxDenied(format!("{label}：{message}")),
        other => other,
    }
}

/// 把回滚结果附在原始错误后面。
///
/// 全部改回成功 → 原样返回：正常失败路径（回滚成功）的文案仍是「写入失败: x」一句话，
/// 不拿回滚细节污染它。
///
/// 有文件没改回 → 单列两份清单。多文件补丁中途失败且回滚也失败时，工作区停在半修改
/// 状态；只说「写入失败: {path}」会让模型误以为什么都没变，按错误前提继续下一步。
fn with_rollback(error: ToolError, report: RollbackReport) -> ToolError {
    if report.failed.is_empty() {
        return error;
    }
    let restored = if report.restored.is_empty() {
        String::new()
    } else {
        format!("（{}）", report.restored.join("、"))
    };
    ToolError::Execution(format!(
        "{error}；回滚未完成：已改回 {} 个文件{restored}，未能改回 {} 个文件 [{}]。\
         工作区停在半修改状态——请先检查这些文件，不要按「什么都没变」继续",
        report.restored.len(),
        report.failed.len(),
        report.failed.join("；"),
    ))
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
        let sections = parse_patch(patch)?;

        // dry-run：全部路径先过边界校验
        for section in &sections {
            resolve_for_write(&ctx.workspace_root, section.path())
                .map_err(|error| with_section(error, section))?;
        }

        // 应用 + 回滚快照：path → 原（None=原不存在，回滚=删除）
        let mut snapshot: HashMap<String, Option<Vec<u8>>> = HashMap::new();
        let mut summary: Vec<String> = Vec::new();

        for section in &sections {
            match &section.op {
                PatchOp::Add { path, lines } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)
                        .map_err(|error| with_section(error, section))?;
                    if full.exists() {
                        return Err(with_rollback(
                            with_section(
                                ToolError::Execution(format!(
                                    "Add File 目标已存在: {path}。如要修改请改用 `*** Update File: {path}`，\
                                     如要重建先 `*** Delete File: {path}`"
                                )),
                                section,
                            ),
                            rollback(&snapshot),
                        ));
                    }
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let mut content = lines.join("\n");
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    if std::fs::write(&full, &content).is_err() {
                        return Err(with_rollback(
                            with_section(
                                ToolError::Execution(format!("写入失败: {path}")),
                                section,
                            ),
                            rollback(&snapshot),
                        ));
                    }
                    snapshot.insert(full.to_string_lossy().to_string(), None);
                    record_write(ctx, &full, None, Some(&content));
                    refresh_read_state(
                        ctx,
                        Some(crate::tool_router::canonical_key(&full)),
                        &full,
                    );
                    summary.push(format!("+ {path}"));
                }
                PatchOp::Update { path, seq } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)
                        .map_err(|error| with_section(error, section))?;
                    let original = match std::fs::read(&full) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let message = if full.exists() {
                                format!("读取失败: {error}")
                            } else {
                                format!(
                                    "Update File 目标不存在: {path}。新建文件请用 `*** Add File: {path}`"
                                )
                            };
                            return Err(with_rollback(
                                with_section(ToolError::Execution(message), section),
                                rollback(&snapshot),
                            ));
                        }
                    };
                    // SH-124：先读过、且内容未被外部改动，才允许修改
                    let read_key = match verify_read_state(ctx, path, &full, &original) {
                        Ok(key) => key,
                        Err(error) => {
                            return Err(with_rollback(
                                with_section(error, section),
                                rollback(&snapshot),
                            ))
                        }
                    };
                    let text = String::from_utf8(original.clone()).map_err(|_| {
                        with_section(
                            ToolError::Execution(format!("非 UTF-8 文本文件: {path}")),
                            section,
                        )
                    })?;
                    let had_trailing_newline = text.ends_with('\n');
                    let (new_lines, level) = apply_hunk_seq(
                        &text.lines().map(String::from).collect::<Vec<_>>(),
                        seq,
                    )
                    .map_err(|e| {
                        with_rollback(
                            with_section(ToolError::Execution(e), section),
                            rollback(&snapshot),
                        )
                    })?;
                    let mut new_text = new_lines.join("\n");
                    if had_trailing_newline && !new_text.is_empty() {
                        new_text.push('\n');
                    }
                    if std::fs::write(&full, &new_text).is_err() {
                        return Err(with_rollback(
                            with_section(
                                ToolError::Execution(format!("写入失败: {path}")),
                                section,
                            ),
                            rollback(&snapshot),
                        ));
                    }
                    snapshot.entry(full.to_string_lossy().to_string()).or_insert(Some(original));
                    record_write(ctx, &full, Some(&text), Some(&new_text));
                    refresh_read_state(ctx, read_key, &full);
                    summary.push(if level == MatchLevel::Exact {
                        format!("~ {path}")
                    } else {
                        format!("~ {path}（{}匹配）", level.label())
                    });
                }
                PatchOp::Delete { path } => {
                    let full = resolve_for_write(&ctx.workspace_root, path)
                        .map_err(|error| with_section(error, section))?;
                    let original = match std::fs::read(&full) {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            return Err(with_rollback(
                                with_section(
                                    ToolError::Execution(format!("Delete File 目标不存在: {path}")),
                                    section,
                                ),
                                rollback(&snapshot),
                            ));
                        }
                    };
                    let read_key = match verify_read_state(ctx, path, &full, &original) {
                        Ok(key) => key,
                        Err(error) => {
                            return Err(with_rollback(
                                with_section(error, section),
                                rollback(&snapshot),
                            ))
                        }
                    };
                    record_write(ctx, &full, Some(&String::from_utf8_lossy(&original)), None);
                    if std::fs::remove_file(&full).is_err() {
                        return Err(with_rollback(
                            with_section(
                                ToolError::Execution(format!("删除失败: {path}")),
                                section,
                            ),
                            rollback(&snapshot),
                        ));
                    }
                    snapshot.insert(full.to_string_lossy().to_string(), Some(original));
                    if let (Some(reads), Some(key)) = (&ctx.reads, read_key) {
                        reads.forget(ctx.session_id.as_deref(), &key);
                    }
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

    fn workspace() -> crate::test_support::TestTempDir {
        let dir = crate::test_support::TestTempDir::new("patch");
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
    }

    #[tokio::test]
    async fn update_replaces_context_block() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+第二行(改)\n line3\n*** End Patch";
        let out = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap();
        assert!(out.success);
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\n第二行(改)\nline3\n");
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        let out = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap();
        assert!(out.success);
        assert!(!ws.join("a.txt").exists());
    }

    #[tokio::test]
    async fn context_mismatch_rolls_back_earlier_ops() {
        let ws = workspace();
        // 先 Add 再 Update（上下文故意错），失败后 Add 的文件必须被回滚
        let patch = "*** Begin Patch\n*** Add File: temp.txt\n+x\n*** Update File: a.txt\n 不存在的上下文\n-y\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        let ToolError::Execution(text) = &err else { unreachable!() };
        assert!(
            !text.contains("回滚"),
            "回滚成功时错误文案必须保持一句话，不许混入回滚噪音: {text}"
        );
        assert!(!ws.join("temp.txt").exists(), "回滚必须删除本轮新增的文件");
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\nline2\nline3\n");
    }

    /// M4：回滚本身失败时，错误文案必须单列「已改回」与「未能改回」两份清单。
    ///
    /// 构造方式：把一个**目录**放进快照当文件写回（`fs::write` 必然失败），
    /// 同时放一个正常文件验证成功清单。反向对照：旧实现用 `let _ = fs::write`，
    /// 这个用例的断言全部落空——文案只有原始的「写入失败: …」一句。
    #[test]
    fn rollback_reports_files_it_could_not_restore() {
        let ws = workspace();
        std::fs::write(ws.join("a.txt"), "被改坏\n").unwrap();
        std::fs::create_dir(ws.join("not-a-file")).unwrap();
        let mut snapshot = HashMap::new();
        snapshot.insert(
            ws.join("a.txt").to_string_lossy().to_string(),
            Some(b"line1\nline2\nline3\n".to_vec()),
        );
        snapshot.insert(
            ws.join("not-a-file").to_string_lossy().to_string(),
            Some("写不进去".as_bytes().to_vec()),
        );

        let report = rollback(&snapshot);
        assert_eq!(report.restored.len(), 1, "可写文件应改回: {:?}", report.restored);
        assert_eq!(report.failed.len(), 1, "目录不能当文件写回: {:?}", report.failed);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\nline2\nline3\n"
        );

        let text = with_rollback(
            ToolError::Execution("写入失败: later.txt".into()),
            report,
        )
        .to_string();
        // 原始错误保留：模型仍要知道「本来哪一步失败」
        assert!(text.starts_with("写入失败: later.txt"), "{text}");
        // 两份清单都在：哪些改回来了、哪些没有
        assert!(text.contains("已改回 1 个文件") && text.contains("a.txt"), "{text}");
        assert!(
            text.contains("未能改回 1 个文件") && text.contains("not-a-file"),
            "{text}"
        );
        assert!(text.contains("半修改状态"), "{text}");
    }

    /// 反向对照的另一半：回滚全部成功时不得给文案加料。
    #[test]
    fn clean_rollback_keeps_failure_message_concise() {
        let report = RollbackReport {
            restored: vec!["a.txt".into()],
            failed: Vec::new(),
        };
        let error = with_rollback(ToolError::Execution("写入失败: b.txt".into()), report);
        assert_eq!(error.to_string(), "写入失败: b.txt");
    }

    #[tokio::test]
    async fn add_existing_file_fails() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "line1\nline2\nline3\n");
    }

    #[tokio::test]
    async fn out_of_workspace_path_rejected() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: ../evil.txt\n+x\n*** End Patch";
        let err = ApplyPatch::new().execute(serde_json::json!({"patch": patch}), &ctx(&ws)).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!ws.parent().unwrap().join("evil.txt").exists());
    }

    #[tokio::test]
    async fn malformed_patch_is_invalid_args() {
        let ws = workspace();
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": "随便写点东西"}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    /// 证伪旧文案「行必须以 + 开头」：必须给出补丁行号、该行原文、期望形状与段号。
    #[test]
    fn malformed_add_line_reports_patch_line_and_shape() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+ok\noops\n*** End Patch";
        let err = parse_patch(patch).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("补丁第 4 行"), "{message}");
        assert!(message.contains("oops"), "必须回显出错行原文: {message}");
        assert!(message.contains("`+`"), "{message}");
        assert!(message.contains("第 1 段"), "{message}");
        assert!(message.contains("Add File: new.txt"), "{message}");
    }

    /// Update 段的非法行同样要指到补丁行号与期望形状。
    #[test]
    fn malformed_update_line_reports_patch_line_and_shape() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\nbad line\n*** End Patch";
        let err = parse_patch(patch).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("补丁第 4 行"), "{message}");
        assert!(message.contains("bad line"), "{message}");
        assert!(message.contains("空格开头"), "{message}");
        assert!(message.contains("`-`"), "{message}");
        assert!(message.contains("`+`"), "{message}");
        assert!(message.contains("第 1 段"), "{message}");
    }

    /// 缺 `*** End Patch` 不能被当成完整补丁静默写入。
    #[tokio::test]
    async fn missing_end_patch_is_rejected_without_writing() {
        let ws = workspace();
        let patch = "*** Begin Patch\n*** Add File: half.txt\n+x";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        assert!(message.contains("不完整"), "{message}");
        assert!(message.contains("`*** End Patch`"), "{message}");
        assert!(message.contains("第 1 段"), "{message}");
        assert!(!ws.join("half.txt").exists(), "不完整补丁不得落盘");
    }

    /// 上下文不匹配必须给出「最接近的位置」与首个差异行，而不是只报一句不匹配。
    #[tokio::test]
    async fn context_mismatch_reports_nearest_location_and_diff() {
        let ws = workspace();
        std::fs::write(ws.join("a.txt"), "alpha\nbravo\ncharlie\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: a.txt\n alpha\n-zulu\n+zulu2\n charlie\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx(&ws))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("最接近的位置"), "{message}");
        assert!(message.contains("文件第 2 行"), "{message}");
        assert!(message.contains("期望 `zulu`"), "{message}");
        assert!(message.contains("实际读到 `bravo`"), "{message}");
    }

    /// 补丁比文件长时要明确说出「无法完整比对」的边界。
    #[test]
    fn mismatch_wider_than_file_states_boundary() {
        let file: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let seq = vec![
            HunkLine::Keep("a".into()),
            HunkLine::Keep("b".into()),
            HunkLine::Keep("c".into()),
            HunkLine::Remove("d".into()),
            HunkLine::Remove("e".into()),
        ];
        let message = apply_hunk_seq(&file, &seq).unwrap_err();
        assert!(message.contains("文件只有 3 行"), "{message}");
        assert!(message.contains("边界"), "{message}");
        assert!(message.contains("无法完整比对"), "{message}");
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
    }

    // ---------- SH-124 写前读状态 + stale 检测 ----------

    fn ctx_with_reads(ws: &Path, reads: Arc<crate::tool_router::ReadRegistry>) -> ToolContext {
        ctx(ws).with_reads(reads)
    }

    #[tokio::test]
    async fn update_without_read_is_rejected() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+改过\n line3\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx_with_reads(&ws, reads))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("write_file_not_read"), "{err}");
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\nline2\nline3\n"
        );
    }

    #[tokio::test]
    async fn update_after_read_succeeds_and_read_state_follows_write() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        crate::tools::fs_read::ReadFile::new()
            .execute(
                serde_json::json!({"path": "a.txt"}),
                &ctx_with_reads(&ws, reads.clone()),
            )
            .await
            .unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+改过\n line3\n*** End Patch";
        ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx_with_reads(&ws, reads.clone()))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\n改过\nline3\n"
        );
        // 写成功后登记表已刷新：同一会话的第二次补丁不需要再读
        let patch2 = "*** Begin Patch\n*** Update File: a.txt\n line1\n-改过\n+再改\n line3\n*** End Patch";
        ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch2}), &ctx_with_reads(&ws, reads.clone()))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "line1\n再改\nline3\n"
        );
    }

    #[tokio::test]
    async fn stale_after_external_change_is_rejected_and_current_content_returned() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        crate::tools::fs_read::ReadFile::new()
            .execute(
                serde_json::json!({"path": "a.txt"}),
                &ctx_with_reads(&ws, reads.clone()),
            )
            .await
            .unwrap();
        // 外部进程改动文件（长度也变，确保不依赖 mtime 分辨率）
        std::fs::write(ws.join("a.txt"), "外部写入\n第二行\n第三行\n第四行\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n line1\n-line2\n+改过\n line3\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx_with_reads(&ws, reads.clone()))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("write_file_stale"), "{message}");
        assert!(message.contains("当前文件内容"), "{message}");
        assert!(message.contains("外部写入"), "必须把当前内容回填模型: {message}");
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "外部写入\n第二行\n第三行\n第四行\n",
            "stale 时不得覆盖"
        );
    }

    #[tokio::test]
    async fn delete_without_read_is_rejected() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx_with_reads(&ws, reads))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("write_file_not_read"), "{err}");
        assert!(ws.join("a.txt").exists());
    }

    /// SH-126③：stale 回填必须有字节上限——几 MB 的文件全文回填会直接吃光上下文。
    #[tokio::test]
    async fn stale_excerpt_is_capped_by_bytes_for_large_files() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        std::fs::write(ws.join("big.txt"), "seed\n").unwrap();
        crate::tools::fs_read::ReadFile::new()
            .execute(
                serde_json::json!({"path": "big.txt"}),
                &ctx_with_reads(&ws, reads.clone()),
            )
            .await
            .unwrap();
        // 外部改成大文件：行数与字节都远超上限（约 800KB）
        let filler = "x".repeat(190);
        let big: String = (1..=4000).map(|i| format!("line{i:04} {filler}\n")).collect();
        assert!(big.len() > 700_000, "构造的文件应足够大: {}", big.len());
        std::fs::write(ws.join("big.txt"), &big).unwrap();

        let patch = "*** Begin Patch\n*** Update File: big.txt\n-seed\n+changed\n*** End Patch";
        let err = ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx_with_reads(&ws, reads))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("write_file_stale"), "{message}");
        assert!(
            message.len() < 16 * 1024,
            "回填必须被字节上限截断，实际 {} 字节",
            message.len()
        );
        assert!(message.contains("已截断"), "必须明示已截断: {message}");
        // 截断后的内容仍可用：首行能看到行号与真实内容
        assert!(message.contains("1| line0001"), "{message}");
    }

    /// SH-126：写入必须进写日志，删减断言要被标成「放宽」。
    #[tokio::test]
    async fn update_records_assertion_weakening_in_write_journal() {
        let ws = workspace();
        let reads = Arc::new(crate::tool_router::ReadRegistry::new());
        let writes = Arc::new(crate::tool_router::WriteJournal::new());
        let original = "#[test]\nfn adds() {\n    assert_eq!(add(2, 2), 4);\n}\n";
        std::fs::write(ws.join("t.rs"), original).unwrap();
        let ctx = ctx(&ws).with_reads(reads).with_writes(writes.clone());
        crate::tools::fs_read::ReadFile::new()
            .execute(serde_json::json!({"path": "t.rs"}), &ctx)
            .await
            .unwrap();

        let patch = "*** Begin Patch\n*** Update File: t.rs\n #[test]\n fn adds() {\n-    assert_eq!(add(2, 2), 4);\n }\n*** End Patch";
        ApplyPatch::new()
            .execute(serde_json::json!({"patch": patch}), &ctx)
            .await
            .unwrap();

        let entries = writes.take();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].weakened, "删掉断言必须标为放宽: {entries:?}");
        assert_eq!(entries[0].assertion_delta, -1);
        assert!(writes.take().is_empty(), "写日志必须被消费干净");
    }
}
