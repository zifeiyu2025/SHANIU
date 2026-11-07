//! 编排四工具（SH-052，docs/02 §2.2）。
//!
//! 本文件只做三件事：解析参数、调用 `ToolContext::orchestrator`、把结果序列化回模型。
//! 不直接依赖 `agent_loop`/`subagent_registry`——能力由组合根注入，避免模块环（FF-1）。
//! 深度门控在两层生效：声明层（子代理看不到这些工具）与分发层（伪造名称也会被拒）。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::tool_router::orchestration::AgentScope;
use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

fn orchestrator(ctx: &ToolContext) -> Result<&std::sync::Arc<dyn crate::tool_router::orchestration::AgentOrchestrator>, ToolError> {
    ctx.orchestrator
        .as_ref()
        .ok_or_else(|| ToolError::Execution("当前代理没有编排能力（只有 root 可以编排子代理）".into()))
}

fn scope(ctx: &ToolContext) -> Result<&AgentScope, ToolError> {
    ctx.scope
        .as_ref()
        .ok_or_else(|| ToolError::Execution("缺少代理上下文，无法编排子代理".into()))
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs(error.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

pub struct SpawnAgent {
    spec: ToolSpec,
}

impl Default for SpawnAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl SpawnAgent {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "spawn_agent",
                "派生一个子代理并行完成子任务，立即返回 agent_id（不阻塞）。子代理上下文独立，只看到 task 描述；用 wait_agent 收集结果。只有 root 可用。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "子任务描述；子代理不继承当前会话历史，背景必须写全" },
                        "profile": { "type": "string", "description": "可选预设；当前仅支持 worker（完整工具集）" },
                        "budget_tokens": { "type": "integer", "description": "该子树自己的 token 上限（须 > 0）；不影响全树预算，只能在全树之内进一步收窄" },
                        "allowed_tools": { "type": "array", "items": { "type": "string" }, "description": "预留给 SH-054 权限合成；当前不支持，传入会失败" }
                    },
                    "required": ["task"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for SpawnAgent {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SpawnArgs = parse(args)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if args.task.trim().is_empty() {
            return Err(ToolError::InvalidArgs("task 不能为空".into()));
        }
        // 权限只降不升（ADR-103）：尚未实现的收窄参数必须拒绝，不能静默给出完整权限。
        if args.allowed_tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
            return Err(ToolError::InvalidArgs(
                "allowed_tools 需要权限合成（SH-054），当前不支持；请在 task 中限定范围或在子代理内自行约束".into(),
            ));
        }
        if args.budget_tokens == Some(0) {
            return Err(ToolError::InvalidArgs(
                "budget_tokens 必须大于 0；不限额就省略该字段".into(),
            ));
        }
        if let Some(profile) = args.profile.as_deref() {
            if profile != "worker" {
                return Err(ToolError::InvalidArgs(format!(
                    "profile {profile} 尚未实现：explorer 需要按 profile 收窄工具集（后续卡片）；当前只支持 worker"
                )));
            }
        }
        let agent_id = orchestrator(ctx)?
            .spawn_agent(scope(ctx)?, args.task.trim(), args.budget_tokens)
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::ok(
            serde_json::json!({ "agent_id": agent_id, "status": "running" }).to_string(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    agent_ids: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct WaitAgent {
    spec: ToolSpec,
}

impl Default for WaitAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitAgent {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "wait_agent",
                "等待一组子代理结束，返回先完成者的结构化结果。超时只返回当前状态，不终止子代理。只有 root 可用。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_ids": { "type": "array", "items": { "type": "string" }, "description": "spawn_agent 返回的 agent_id 列表" },
                        "timeout_ms": { "type": "integer", "description": "总超时毫秒，默认 30000，上限 600000" }
                    },
                    "required": ["agent_ids"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for WaitAgent {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: WaitArgs = parse(args)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let ids: Vec<String> = args
            .agent_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(ToolError::InvalidArgs("agent_ids 不能为空".into()));
        }
        if let Some(duplicate) = ids.iter().enumerate().find_map(|(index, id)| {
            ids[..index].contains(id).then_some(id.clone())
        }) {
            return Err(ToolError::InvalidArgs(format!("agent_ids 含重复项: {duplicate}")));
        }
        let timeout = Duration::from_millis(
            args.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).clamp(1, MAX_WAIT_MS),
        );
        let outcomes = orchestrator(ctx)?.wait_agents(&ids, timeout).await;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        Ok(ToolOutput::ok(serde_json::json!({ "results": outcomes }).to_string()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    agent_id: String,
    message: String,
}

pub struct SendAgentMessage {
    spec: ToolSpec,
}

impl Default for SendAgentMessage {
    fn default() -> Self {
        Self::new()
    }
}

impl SendAgentMessage {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "send_agent_message",
                "向运行中的子代理注入一条引导消息，在下一次模型请求前进入其上下文。不中断当前请求。只有 root 可用。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "目标子代理 id" },
                        "message": { "type": "string", "description": "要注入的引导内容" }
                    },
                    "required": ["agent_id", "message"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for SendAgentMessage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SendArgs = parse(args)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if args.message.trim().is_empty() {
            return Err(ToolError::InvalidArgs("message 不能为空".into()));
        }
        orchestrator(ctx)?
            .send_message(args.agent_id.trim(), args.message.trim())
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::ok(
            serde_json::json!({ "agent_id": args.agent_id.trim(), "delivered": true }).to_string(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeArgs {
    agent_id: String,
    #[serde(default)]
    task: Option<String>,
}

pub struct ResumeAgent {
    spec: ToolSpec,
}

impl Default for ResumeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl ResumeAgent {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "resume_agent",
                "从一个已结束的子代理恢复，沿用它已有的对话历史继续工作，返回新的 agent_id。只有 root 可用。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "已结束的子代理 id" },
                        "task": { "type": "string", "description": "续跑指令；省略则要求它接着完成原任务" }
                    },
                    "required": ["agent_id"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for ResumeAgent {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ResumeArgs = parse(args)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let task = args.task.as_deref().map(str::trim).filter(|text| !text.is_empty());
        if args.task.is_some() && task.is_none() {
            return Err(ToolError::InvalidArgs("task 为空；省略该字段表示默认续跑".into()));
        }
        let agent_id = orchestrator(ctx)?
            .resume_agent(scope(ctx)?, args.agent_id.trim(), task)
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::ok(
            serde_json::json!({ "agent_id": agent_id, "status": "running" }).to_string(),
        ))
    }
}

/// 注册四个编排工具（路由共享；可见性由 agent_loop 的声明层按深度过滤）。
pub fn register(router: &mut crate::tool_router::ToolRouter) {
    router.register(std::sync::Arc::new(SpawnAgent::new()));
    router.register(std::sync::Arc::new(WaitAgent::new()));
    router.register(std::sync::Arc::new(SendAgentMessage::new()));
    router.register(std::sync::Arc::new(ResumeAgent::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::config::SandboxMode;
    use crate::events::{EventMsg, EventSink};
    use crate::tool_router::orchestration::{AgentOutcome, AgentOutcomeState, AgentOrchestrator};

    struct DiscardSink;

    impl EventSink for DiscardSink {
        fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubOrchestrator {
        spawned: Mutex<Vec<String>>,
        budgets: Mutex<Vec<Option<u64>>>,
        resumed: Mutex<Vec<(String, Option<String>)>>,
        messages: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl AgentOrchestrator for StubOrchestrator {
        async fn spawn_agent(
            &self,
            _parent: &AgentScope,
            task: &str,
            budget_tokens: Option<u64>,
        ) -> Result<String, String> {
            self.spawned.lock().unwrap().push(task.to_string());
            self.budgets.lock().unwrap().push(budget_tokens);
            Ok("child-1".into())
        }

        async fn resume_agent(
            &self,
            _parent: &AgentScope,
            agent_id: &str,
            task: Option<&str>,
        ) -> Result<String, String> {
            self.resumed
                .lock()
                .unwrap()
                .push((agent_id.to_string(), task.map(str::to_string)));
            Ok("child-2".into())
        }

        async fn wait_agents(&self, agent_ids: &[String], _timeout: Duration) -> Vec<AgentOutcome> {
            agent_ids
                .iter()
                .map(|agent_id| AgentOutcome {
                    agent_id: agent_id.clone(),
                    state: AgentOutcomeState::Done,
                    summary: "完成".into(),
                    artifacts: Vec::new(),
                    token_usage: None,
                })
                .collect()
        }

        fn send_message(&self, agent_id: &str, message: &str) -> Result<(), String> {
            self.messages
                .lock()
                .unwrap()
                .push((agent_id.to_string(), message.to_string()));
            Ok(())
        }
    }

    fn context(stub: Arc<StubOrchestrator>) -> ToolContext {
        let scope = AgentScope {
            agent_id: "root".into(),
            parent_id: None,
            depth: 0,
            cancel: tokio_util::sync::CancellationToken::new(),
            sink: Arc::new(DiscardSink),
        };
        ToolContext::new(
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            "call-1",
            Arc::new(DiscardSink),
            SandboxMode::WorkspaceWrite,
        )
        .with_orchestration(scope, stub)
    }

    #[tokio::test]
    async fn spawn_rejects_unknown_capabilities_instead_of_silently_downgrading() {
        let stub = Arc::new(StubOrchestrator::default());
        let ctx = context(stub.clone());
        let tool = SpawnAgent::new();

        for args in [
            serde_json::json!({ "task": "   " }),
            serde_json::json!({ "task": "x", "allowed_tools": ["read_file"] }),
            serde_json::json!({ "task": "x", "budget_tokens": 0 }),
            serde_json::json!({ "task": "x", "profile": "explorer" }),
            serde_json::json!({ "task": "x", "unknown": 1 }),
        ] {
            assert!(
                matches!(tool.execute(args.clone(), &ctx).await, Err(ToolError::InvalidArgs(_))),
                "应拒绝 {args}"
            );
        }
        assert!(stub.spawned.lock().unwrap().is_empty(), "校验失败不得真的派生子代理");

        let output = tool
            .execute(serde_json::json!({ "task": "  探索仓库  ", "budget_tokens": 2000 }), &ctx)
            .await
            .expect("合法参数应成功");
        assert!(output.output.contains("child-1"));
        assert_eq!(*stub.spawned.lock().unwrap(), vec!["探索仓库".to_string()]);
        // 预算必须原样交给编排层，不能在工具层被吞掉。
        assert_eq!(*stub.budgets.lock().unwrap(), vec![Some(2000)]);
    }

    #[tokio::test]
    async fn wait_rejects_empty_and_duplicate_ids_then_serializes_outcomes() {
        let stub = Arc::new(StubOrchestrator::default());
        let ctx = context(stub);
        let tool = WaitAgent::new();

        for args in [
            serde_json::json!({ "agent_ids": [] }),
            serde_json::json!({ "agent_ids": ["a", "a"] }),
        ] {
            assert!(matches!(tool.execute(args, &ctx).await, Err(ToolError::InvalidArgs(_))));
        }

        let output = tool
            .execute(serde_json::json!({ "agent_ids": ["a", "b"], "timeout_ms": 5 }), &ctx)
            .await
            .expect("应成功");
        let json: Value = serde_json::from_str(&output.output).expect("输出必须是 JSON");
        assert_eq!(json["results"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["results"][0]["state"], "done");
    }

    #[tokio::test]
    async fn send_and_resume_validate_then_delegate() {
        let stub = Arc::new(StubOrchestrator::default());
        let ctx = context(stub.clone());

        assert!(matches!(
            SendAgentMessage::new()
                .execute(serde_json::json!({ "agent_id": "a", "message": "  " }), &ctx)
                .await,
            Err(ToolError::InvalidArgs(_))
        ));
        let output = SendAgentMessage::new()
            .execute(serde_json::json!({ "agent_id": " child-1 ", "message": " 看边界 " }), &ctx)
            .await
            .expect("应成功");
        assert!(output.output.contains("delivered"));
        assert_eq!(
            *stub.messages.lock().unwrap(),
            vec![("child-1".to_string(), "看边界".to_string())]
        );

        // 显式传空 task 是用户错误，不能悄悄变成默认续跑。
        assert!(matches!(
            ResumeAgent::new()
                .execute(serde_json::json!({ "agent_id": "a", "task": "" }), &ctx)
                .await,
            Err(ToolError::InvalidArgs(_))
        ));
        let output = ResumeAgent::new()
            .execute(serde_json::json!({ "agent_id": "child-1" }), &ctx)
            .await
            .expect("应成功");
        assert!(output.output.contains("child-2"));
        assert_eq!(
            *stub.resumed.lock().unwrap(),
            vec![("child-1".to_string(), None)]
        );
    }

    #[tokio::test]
    async fn cancelled_context_short_circuits_every_tool() {
        let stub = Arc::new(StubOrchestrator::default());
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext::new(
            std::env::temp_dir(),
            cancel,
            "call-1",
            Arc::new(DiscardSink),
            SandboxMode::WorkspaceWrite,
        )
        .with_orchestration(
            AgentScope {
                agent_id: "root".into(),
                parent_id: None,
                depth: 0,
                cancel: tokio_util::sync::CancellationToken::new(),
                sink: Arc::new(DiscardSink),
            },
            stub,
        );

        assert!(matches!(
            SpawnAgent::new().execute(serde_json::json!({ "task": "x" }), &ctx).await,
            Err(ToolError::Cancelled)
        ));
        assert!(matches!(
            WaitAgent::new().execute(serde_json::json!({ "agent_ids": ["a"] }), &ctx).await,
            Err(ToolError::Cancelled)
        ));
        assert!(matches!(
            SendAgentMessage::new()
                .execute(serde_json::json!({ "agent_id": "a", "message": "x" }), &ctx)
                .await,
            Err(ToolError::Cancelled)
        ));
        assert!(matches!(
            ResumeAgent::new().execute(serde_json::json!({ "agent_id": "a" }), &ctx).await,
            Err(ToolError::Cancelled)
        ));
    }
}
