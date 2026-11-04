//! 工具路由（docs §8）：ToolSpec / ToolExecutor trait / 注册表。
//!
//! 职责：注册表管理 + 按名分发执行 + 向 model_client 下发工具声明。
//! 执行逻辑在各 `tools/*` 模块；风险分级与审批在 M2 permission 模块接入
//! （届时 execute 前置裁决，本模块保持纯分发）。

pub mod spec;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::EventSink;
use crate::model_client::ToolDeclaration;

pub use spec::ToolSpec;

// ============================== 执行契约 ==============================

/// 工具执行错误。`Execution` 的文本会回填给模型（风险登记册：失败自动回填）。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("参数非法: {0}")]
    InvalidArgs(String),
    #[error("{0}")]
    Execution(String),
    #[error("已取消")]
    Cancelled,
}

/// 工具输出（回填给模型的文本 + 成功标记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
    pub success: bool,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), success: true }
    }
    pub fn fail(output: impl Into<String>) -> Self {
        Self { output: output.into(), success: false }
    }
}

/// 工具执行上下文：工作区边界、取消令牌、流式输出通道。
pub struct ToolContext {
    /// 工作区根目录（沙箱 workspace-write 的作用域，docs §9.3）。
    pub workspace_root: PathBuf,
    pub cancel: CancellationToken,
    /// 本次调用的 call_id（unified_exec 等流式工具经 sink 发 ToolCallOutputDelta）。
    pub call_id: String,
    pub sink: Arc<dyn EventSink>,
}

impl ToolContext {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        cancel: CancellationToken,
        call_id: impl Into<String>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            cancel,
            call_id: call_id.into(),
            sink,
        }
    }
}

/// 工具执行器：一个实现 = 一个工具。
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

// ============================== 注册表 ==============================

#[derive(Default)]
pub struct ToolRouter {
    tools: HashMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, executor: Arc<dyn ToolExecutor>) {
        self.tools.insert(executor.spec().name.clone(), executor);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolExecutor>> {
        self.tools.get(name)
    }

    /// 全部工具契约（下发模型用）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.values().map(|t| t.spec().clone()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.specs().iter().map(|s| s.to_declaration()).collect()
    }

    /// 按名分发执行；未知工具返回 InvalidArgs（回填模型可自我纠正）。
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let executor = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs(format!("未知工具: {name}")))?;
        executor.execute(args, ctx).await
    }
}

// ============================== 工作区路径边界（共享助手） ==============================

/// 解析工作区内路径：绝对路径与相对路径统一，canonicalize 后校验不越界。
/// 要求目标**已存在**（读类工具用）。这是 M1 的逻辑边界；M2 sandbox 模块落地后
/// 由 deny-ACE 接管强制力（ADR-002）。
pub fn resolve_in_workspace(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径为空".into()));
    }
    let raw = PathBuf::from(path);
    let joined = if raw.is_absolute() { raw } else { workspace_root.join(raw) };
    let canonical = joined
        .canonicalize()
        .map_err(|_| ToolError::InvalidArgs(format!("路径不存在: {path}")))?;
    let root = workspace_root
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    if !canonical.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    Ok(canonical)
}

/// 写类操作的工作区路径校验：允许目标**尚不存在**（如 Add File）。
/// 规则：拒绝包含 `..` 组件；对最深存在的祖先做 canonicalize 并校验不越界，
/// 尾部组件原样拼接（不存在 `..` 故不可能向上逃逸）。
pub fn resolve_for_write(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径为空".into()));
    }
    let raw = PathBuf::from(path);
    let joined = if raw.is_absolute() { raw } else { workspace_root.join(raw) };
    if joined
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ToolError::Execution(format!(
            "路径越界（包含 ..）: {path}"
        )));
    }
    // 自底向上找最深存在祖先
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = joined.clone();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) if parent != probe => {
                tail.push(probe.file_name().map(|n| n.to_os_string()).unwrap_or_default());
                probe = parent.to_path_buf();
            }
            _ => return Err(ToolError::InvalidArgs(format!("路径无法定位: {path}"))),
        }
    }
    let canonical_base = probe
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    let root = workspace_root
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    if !canonical_base.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    let mut result = canonical_base;
    for name in tail.iter().rev() {
        result.push(name);
    }
    if !result.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 工具级单测的最小上下文（sink 丢弃事件）
    pub fn test_context(workspace_root: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn EventSink>> = std::sync::OnceLock::new();
        let sink = SINK.get_or_init(|| -> Arc<dyn EventSink> {
            Arc::new(crate::events::NullEventSink)
        })
        .clone();
        ToolContext::new(workspace_root, CancellationToken::new(), "call-test", sink)
    }

    fn router_with_noop() -> ToolRouter {
        struct Owned {
            spec: ToolSpec,
        }
        #[async_trait]
        impl ToolExecutor for Owned {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("done"))
            }
        }
        let mut router = ToolRouter::new();
        router.register(Arc::new(Owned {
            spec: ToolSpec::new("noop", "no-op", serde_json::json!({"type":"object"})),
        }));
        router
    }

    #[tokio::test]
    async fn executes_registered_tool_by_name() {
        let router = router_with_noop();
        let out = router
            .execute("noop", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.output, "done");
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_args() {
        let router = router_with_noop();
        let err = router
            .execute("missing", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn cancelled_context_short_circuits() {
        let router = router_with_noop();
        let ctx = test_context(Path::new("."));
        ctx.cancel.cancel();
        let err = router.execute("noop", serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn specs_and_declarations_are_sorted_and_complete() {
        let router = router_with_noop();
        let specs = router.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "noop");
        assert_eq!(router.declarations()[0].name, "noop");
    }

    // resolve_in_workspace 的行为契约
    fn temp_workspace() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-ws-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let ws = temp_workspace();
        let rel = resolve_in_workspace(&ws, "a.txt").unwrap();
        assert!(rel.ends_with("a.txt"));
        let abs = resolve_in_workspace(&ws, &ws.join("a.txt").to_string_lossy()).unwrap();
        assert!(abs.ends_with("a.txt"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn rejects_out_of_workspace_paths() {
        let ws = temp_workspace();
        // ".." 真实存在（temp 父目录）：canonicalize 成功后触发越界判定
        let err = resolve_in_workspace(&ws, "..").unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn write_resolver_allows_missing_targets_but_rejects_escape() {
        let ws = temp_workspace();
        // 允许尚不存在的目标（Add File 场景），含多级新目录
        let ok = resolve_for_write(&ws, "new/deep/file.txt").unwrap();
        assert!(ok.starts_with(ws.canonicalize().unwrap().as_path()));
        assert_eq!(ok.file_name().unwrap(), "file.txt");
        // 含 .. 的路径直接越界拒绝
        let err = resolve_for_write(&ws, "../evil.txt").unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn rejects_missing_paths_as_invalid_args() {
        let ws = temp_workspace();
        let err = resolve_in_workspace(&ws, "no-such-file.txt").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&ws).ok();
    }
}
