//! 工具路由（docs §8）：ToolSpec / ToolExecutor trait / 注册表。
//!
//! 职责：注册表管理 + 按名分发执行 + 向 model_client 下发工具声明。
//! 执行逻辑在各 `tools/*` 模块；风险分级与审批在 M2 permission 模块接入
//! （届时 execute 前置裁决，本模块保持纯分发）。

pub mod orchestration;
pub mod spec;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::EventSink;
use crate::model_client::ToolDeclaration;

pub use spec::ToolSpec;

/// 计划态共享标志。
///
/// 放在共享内核（与 `ToolSpec` 同层）而非 `tools/plan`：`agent_loop` 要读它做
/// 计划态门控，若留在 `tools/*` 会逼出 `agent_loop → tools` 的反向依赖（docs §2.3）。
#[derive(Debug, Default)]
pub struct PlanState {
    active: AtomicBool,
}

impl PlanState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn set(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    /// 激活计划态（enter_plan_mode 调用；测试与恢复路径同样走此入口）。
    pub fn activate(&self) {
        self.set(true);
    }

    /// 退出计划态（exit_plan_mode 获批准后调用）。
    pub fn deactivate(&self) {
        self.set(false);
    }
}

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

/// 工具执行上下文：工作区边界、取消令牌、流式输出通道、沙箱策略。
pub struct ToolContext {
    /// 工作区根目录（沙箱 workspace-write 的作用域，docs §9.3）。
    pub workspace_root: PathBuf,
    pub cancel: CancellationToken,
    /// 本次调用的 call_id（unified_exec 等流式工具经 sink 发 ToolCallOutputDelta）。
    pub call_id: String,
    pub sink: Arc<dyn EventSink>,
    /// 沙箱策略（unified_exec 启动子进程时生效）。
    pub sandbox: crate::config::SandboxMode,
    /// 会话作用域（SH-089）：后台作业的属主。测试与无会话路径为 None。
    pub session_id: Option<String>,
    /// 进程级作业注册表：`run_in_background` 与 job 工具从这里读写。
    pub jobs: Option<Arc<crate::jobs::JobRegistry>>,
    /// 调用者身份（SH-052）：root 或子代理；None = 无代理上下文的测试路径。
    pub scope: Option<orchestration::AgentScope>,
    /// 编排能力（SH-052）：只有 depth=0 的 root 实例注入；子代理为 None。
    pub orchestrator: Option<Arc<dyn orchestration::AgentOrchestrator>>,
}

impl ToolContext {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        cancel: CancellationToken,
        call_id: impl Into<String>,
        sink: Arc<dyn EventSink>,
        sandbox: crate::config::SandboxMode,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            cancel,
            call_id: call_id.into(),
            sink,
            sandbox,
            session_id: None,
            jobs: None,
            scope: None,
            orchestrator: None,
        }
    }

    /// 只注入代理身份（子代理路径）：用于深度门控，不提供编排能力。
    pub fn with_scope(mut self, scope: orchestration::AgentScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 注入代理身份与编排能力（SH-052，仅 root）。两者要么同时给，要么都不给：
    /// 只有 scope 没有 orchestrator 时，编排工具必然失败，不如在声明层就隐藏。
    pub fn with_orchestration(
        mut self,
        scope: orchestration::AgentScope,
        orchestrator: Arc<dyn orchestration::AgentOrchestrator>,
    ) -> Self {
        self.scope = Some(scope);
        self.orchestrator = Some(orchestrator);
        self
    }

    /// 注入会话与作业注册表（agent_loop 在每个 turn 的工具上下文上调用）。
    pub fn with_jobs(
        mut self,
        session_id: Option<String>,
        jobs: Option<Arc<crate::jobs::JobRegistry>>,
    ) -> Self {
        self.session_id = session_id;
        self.jobs = jobs;
        self
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
        // 第二层深度门控（ADR-101/FF-106）：即使模型伪造工具名，子代理也无法调用编排工具。
        // 第一层在 `agent_loop::declarations_for`——模型根本看不到这些声明。
        if let Some(scope) = &ctx.scope {
            if scope.depth > 0 && orchestration::is_agent_tool(name) {
                return Err(ToolError::InvalidArgs(format!(
                    "当前代理深度 {} 不允许调用编排工具 {name}（只有 root 可以 spawn）",
                    scope.depth
                )));
            }
        } else if orchestration::is_agent_tool(name) {
            return Err(ToolError::InvalidArgs(format!(
                "没有代理上下文，无法调用编排工具 {name}"
            )));
        }
        let executor = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs(format!("未知工具: {name}")))?;
        executor.execute(args, ctx).await
    }
}

// ============================== 工作区路径边界（共享助手） ==============================

/// 去掉 Windows 的 `\\?\` verbatim 前缀，还原成 shell 可用的等价路径。
///
/// 为什么必须还原：`canonicalize()` 在 Windows 上**一定**产出 `\\?\C:\...` 形态，
/// 而 cmd.exe 认不了它——`cd /d "\\?\C:\..."` 失败、`dir /b > out.txt` 静默不产出文件。
/// 于是模型看到的 cwd 与它能用的 cwd 不是同一个东西，只能反复重试直到耗尽轮数预算
/// （SH-085 的真实成因，基线里那条任务就是这么死的）。
/// 只在**安全校验之后**调用：越界判定仍用 canonical 路径做，还原不改变指向的位置。
/// 非 verbatim（普通绝对/相对路径）原样返回。
pub fn shell_usable(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Prefix(prefix)) => match prefix.kind() {
            // drive 是 ASCII 码（u8），必须转成字符；根已在字符串里给出，
            // 后面的 RootDir 组件必须跳过，否则拼出 "D:\\\x"
            std::path::Prefix::VerbatimDisk(drive) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(drive)));
                out.extend(components.filter(|c| !matches!(c, std::path::Component::RootDir)));
                out
            }
            std::path::Prefix::VerbatimUNC(host, share) => {
                // host/share 是 OsStr，不能进 format!；用 push 逐段拼
                let mut out = PathBuf::from(r"\\");
                out.push(host);
                out.push(share);
                out.extend(components.filter(|c| !matches!(c, std::path::Component::RootDir)));
                out
            }
            // 其它前缀（\\.\、\?\ 设备路径等）不是 canonicalize 的产物，原样交出
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// 解析工作区内路径：绝对路径与相对路径统一，canonicalize 后校验不越界。
/// 要求目标**已存在**（读类工具用）。这是 M1 的逻辑边界；M2 sandbox 模块落地后
/// 由 deny-ACE 接管强制力（ADR-002）。返回 shell 可用形态（见 [`shell_usable`]）。
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
    Ok(shell_usable(&canonical))
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
    Ok(shell_usable(&result))
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
        ToolContext::new(workspace_root, CancellationToken::new(), "call-test", sink, crate::config::SandboxMode::WorkspaceWrite)
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
        // 契约（SH-085）：返回 shell 可用形态，故与"还原后的根"比较，而不是 canonicalize 原值
        assert!(ok.starts_with(shell_usable(&ws.canonicalize().unwrap()).as_path()));
        assert_eq!(ok.file_name().unwrap(), "file.txt");
        // 含 .. 的路径直接越界拒绝
        let err = resolve_for_write(&ws, "../evil.txt").unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    /// SH-085 的回归锁：canonicalize 在 Windows 上必然产出 `\\?\` verbatim 路径，
    /// 而 cmd.exe 拿它当 cwd 时 `dir /b > out.txt` 静默不产出文件、`cd /d "\\?\..."` 失败——
    /// 模型看到的 cwd 与它能用的 cwd 不是同一个东西，只能反复重试到轮数预算耗尽。
    #[cfg(windows)]
    #[test]
    fn resolvers_never_hand_verbatim_paths_to_shell_tools() {
        let ws = temp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        for resolved in [
            resolve_in_workspace(&ws, "a.txt").unwrap(),
            resolve_in_workspace(&ws, ".").unwrap(),
            resolve_for_write(&ws, "new/deep/file.txt").unwrap(),
        ] {
            let text = resolved.display().to_string();
            assert!(
                !text.contains(r"\\?\"),
                "交给 shell/模型的路径不该带 verbatim 前缀：{text}"
            );
        }
        // 还原不改变指向：已存在的目标仍能在还原后的路径上命中
        assert!(
            resolve_in_workspace(&ws, "a.txt").unwrap().is_file(),
            "还原后的路径不能再变成不可用"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[cfg(windows)]
    #[test]
    fn shell_usable_strips_verbatim_and_leaves_normal_paths() {
        assert_eq!(
            shell_usable(Path::new(r"\\?\C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
        assert_eq!(
            shell_usable(Path::new(r"\\?\UNC\fileserver\share\x")),
            PathBuf::from(r"\\fileserver\share\x")
        );
        // 非 verbatim 原样交出（drive 是 u8 ASCII 码，转错就会产出 "68:\" 这种废路径）
        assert_eq!(
            shell_usable(Path::new(r"C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
    }

    #[test]
    fn rejects_missing_paths_as_invalid_args() {
        let ws = temp_workspace();
        let err = resolve_in_workspace(&ws, "no-such-file.txt").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&ws).ok();
    }
}
