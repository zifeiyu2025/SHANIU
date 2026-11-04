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
                "对工作区文件应用结构化补丁（V4A 格式：Add/Update/Delete File），精确上下文匹配，失败自动回滚。",
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

/// Update 段：keep+remove 序列在文件中做**精确连续匹配**，替换为 keep+add。
fn apply_hunk_seq(file_lines: &[String], seq: &[HunkLine]) -> Result<Vec<String>, String> {
    let expected: Vec<&str> = seq
        .iter()
        .filter(|h| !matches!(h, HunkLine::Add(_)))
        .map(|h| match h {
            HunkLine::Keep(s) | HunkLine::Remove(s) => s.as_str(),
            HunkLine::Add(_) => unreachable!(),
        })
        .collect();
    let mut replacement: Vec<String> = Vec::new();
    for h in seq {
        match h {
            HunkLine::Keep(s) | HunkLine::Add(s) => replacement.push(s.clone()),
            HunkLine::Remove(_) => {}
        }
    }
    if expected.is_empty() {
        return Err("Update 段需要至少一行上下文或删除行".into());
    }
    if file_lines.len() < expected.len() {
        return Err(format!(
            "上下文不匹配：文件仅 {} 行，补丁期望 {} 行",
            file_lines.len(),
            expected.len()
        ));
    }
    for start in 0..=file_lines.len() - expected.len() {
        if file_lines[start..start + expected.len()]
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            let mut out = file_lines[..start].to_vec();
            out.extend(replacement);
            out.extend(file_lines[start + expected.len()..].to_vec());
            return Ok(out);
        }
    }
    Err(format!(
        "上下文不匹配：未在文件中找到补丁期望的 {} 行序列",
        expected.len()
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
                    let new_lines = apply_hunk_seq(
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
                    summary.push(format!("~ {path}"));
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
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call-patch", sink)
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
}
