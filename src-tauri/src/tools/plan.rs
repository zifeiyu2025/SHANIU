//! Plan Mode（docs §8 tools/plan.rs，ADR-003）：
//! 进入免确认进入只读计划态；产出计划后 ExitPlanMode 必须用户评审
//! （复用审批通道）——批准后计划落盘 `.shaniu/plan.md` 并恢复完整能力。
//!
//! 只读强制为双层：①agent_loop 按白名单收窄工具声明（模型看不见写工具）；
//! ②执行守卫拒绝非白名单调用（防御）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

/// 计划态共享标志。
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

// ============================== enter_plan_mode ==============================

pub struct EnterPlanMode {
    spec: ToolSpec,
    state: Arc<PlanState>,
}

impl EnterPlanMode {
    pub fn new(state: Arc<PlanState>) -> Self {
        Self {
            spec: ToolSpec::new(
                "enter_plan_mode",
                "进入计划模式：先以只读工具调研工作区并产出实施计划，完成后调用 exit_plan_mode 请求用户评审。计划获得批准前无法写入或执行命令。",
                serde_json::json!({"type": "object", "properties": {}}),
            ),
            state,
        }
    }
}

#[async_trait]
impl ToolExecutor for EnterPlanMode {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        // 免确认（ADR-003）；重复进入幂等
        self.state.activate();
        Ok(ToolOutput::ok(
            "已进入计划模式：仅只读工具（read_file/list_dir/search）可用。请完成调研后调用 exit_plan_mode 提交计划。",
        ))
    }
}

// ============================== exit_plan_mode ==============================

pub struct ExitPlanMode {
    spec: ToolSpec,
    state: Arc<PlanState>,
}

impl ExitPlanMode {
    pub fn new(state: Arc<PlanState>) -> Self {
        Self {
            spec: ToolSpec::new(
                "exit_plan_mode",
                "提交计划请求用户评审。计划获批准后将保存到工作区 .shaniu/plan.md 并恢复完整工具能力；被拒绝则继续留在计划模式。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "plan": { "type": "string", "description": "完整的实施计划（Markdown）" }
                    },
                    "required": ["plan"]
                }),
            ),
            state,
        }
    }
}

#[async_trait]
impl ToolExecutor for ExitPlanMode {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let plan = args
            .get("plan")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 plan".into()))?;
        if plan.trim().is_empty() {
            return Err(ToolError::InvalidArgs("计划为空".into()));
        }
        // 落盘 .shaniu/plan.md（docs §9.1 工作区级数据）
        let plan_dir = ctx.workspace_root.join(".shaniu");
        std::fs::create_dir_all(&plan_dir).map_err(|e| ToolError::Execution(format!("创建 .shaniu 失败: {e}")))?;
        std::fs::write(plan_dir.join("plan.md"), plan)
            .map_err(|e| ToolError::Execution(format!("写入 plan.md 失败: {e}")))?;
        // 恢复完整能力
        self.state.deactivate();
        Ok(ToolOutput::ok(
            "计划已获用户批准，保存至 .shaniu/plan.md。计划模式已退出，全部工具恢复可用。",
        ))
    }
}

/// 注册 Plan Mode 工具对。
pub fn register(router: &mut crate::tool_router::ToolRouter, state: Arc<PlanState>) {
    router.register(Arc::new(EnterPlanMode::new(state.clone())));
    router.register(Arc::new(ExitPlanMode::new(state)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NullEventSink;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx(ws: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(
            ws,
            tokio_util::sync::CancellationToken::new(),
            "call-plan",
            sink,
            crate::config::SandboxMode::ReadOnly, // 计划态沙箱即只读
        )
    }

    fn workspace() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-plan-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn enter_activates_plan_mode() {
        let state = Arc::new(PlanState::new());
        assert!(!state.is_active());
        let out = EnterPlanMode::new(state.clone())
            .execute(serde_json::json!({}), &ctx(&workspace()))
            .await
            .unwrap();
        assert!(out.success);
        assert!(state.is_active(), "进入后计划态必须激活");
        // 幂等
        EnterPlanMode::new(state.clone())
            .execute(serde_json::json!({}), &ctx(&workspace()))
            .await
            .unwrap();
        assert!(state.is_active());
    }

    #[tokio::test]
    async fn exit_writes_plan_and_deactivates() {
        let state = Arc::new(PlanState::new());
        state.activate();
        let ws = workspace();
        let out = ExitPlanMode::new(state.clone())
            .execute(
                serde_json::json!({"plan": "## 步骤\n1. 先读\n2. 再写"}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(out.success);
        assert!(!state.is_active(), "批准后必须退出计划态");
        let saved = std::fs::read_to_string(ws.join(".shaniu").join("plan.md")).unwrap();
        assert!(saved.contains("## 步骤"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn exit_requires_nonempty_plan() {
        let state = Arc::new(PlanState::new());
        state.activate();
        let ws = workspace();
        let err = ExitPlanMode::new(state.clone())
            .execute(serde_json::json!({"plan": "  "}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        assert!(state.is_active(), "拒绝后保持计划态");
        std::fs::remove_dir_all(&ws).ok();
    }
}
