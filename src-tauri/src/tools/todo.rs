//! todo_write（SH-108）：会话内任务清单工具，契约对齐上游 `packages/todo/tool-todo`。
//!
//! 三条刻意的设计选择：
//!
//! 1. **零协议面变更**：工具不新增 `EventMsg` 变体、不写 rollout 新条目。上游的持久化
//!    事实源是 journal `todo/write` 事件，而 SHANIU 的持久化事实源是模型历史——工具调用
//!    记录本身就在档案里。适配层从「调用参数 + 执行结果」在实时与回放两条路径上推导出
//!    同一个 journal 事件（`src/transport/adapter.ts` 的 `parseTodos` 与本模块
//!    `validate` 是同一组纯谓词的两面，等价性由两侧测试与真机回放钉住）。
//! 2. **内容 trim 后入库**（对齐上游 `toTodoList`）：trim 是上游既定的输入归一，不套用
//!    「拒绝不截断」先例——那条先例针对的是有长度上限的输入（标题、框架字节），静默截断
//!    会让用户以为存的是原文；这里 trim 只是外观归一，拒绝 " fix " 反而比上游更严。
//!    适配层回放推导做同一 trim，两条路径逐项相等。
//! 3. **风险 Read 档**（permission/policy.rs）：清单是会话内状态，不触碰工作区，上游
//!    同名工具从不触发审批。Read 档同时让它进计划态白名单——计划态列 todo 正是
//!    上游规划工作流的形态。
//!
//! 部署口径：`ALLOW_PARALLEL_IN_PROGRESS = true`，对齐上游 base bundle
//! （`cordis.patch.yml` 的 `allowParallelInProgress: true`）。SHANIU 单会话串行执行工具，
//! 并行语义留给 M5 子代理；若未来要单活跃纪律，把常量改 false 即可（校验分支已备）。

use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::Value;

use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

/// 是否允许多条 todo 同时 `in_progress`（上游 base bundle 默认 true）。
const ALLOW_PARALLEL_IN_PROGRESS: bool = true;

/// 三态枚举（上游 TodoItem['status']）。
const STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];

/// 经校验的一条 todo：trim 后的 content + 原样 status。
type ValidatedTodo = (String, String);

/// 参数校验（模型侧唯一的失败来源）：形状 + 值约束一并在此，
/// 错误文案对齐上游（模型据此自我纠正）。
fn validate(args: &Value, allow_parallel: bool) -> Result<Vec<ValidatedTodo>, ToolError> {
    let items = args
        .get("todos")
        .ok_or_else(|| ToolError::InvalidArgs("缺少 todos".into()))?
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgs("todos 必须是数组".into()))?;

    let mut todos: Vec<ValidatedTodo> = Vec::with_capacity(items.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut active = 0usize;
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| ToolError::InvalidArgs("todo 条目必须是对象".into()))?;
        // 上游 schema 的 additionalProperties: false：多余键拒绝而不是静默展平——
        // 适配层回放推导按同一规则镜像，两边不一致就等于给模型两套语义。
        if obj.keys().any(|key| key != "content" && key != "status") {
            return Err(ToolError::InvalidArgs(
                "todo 条目只接受 content 与 status".into(),
            ));
        }
        let raw_content = obj
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("invalid todo: `content` must be a non-empty string".into()))?;
        let content = raw_content.trim();
        if content.is_empty() {
            return Err(ToolError::InvalidArgs(
                "invalid todo: `content` must be a non-empty string".into(),
            ));
        }
        let status = obj
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("invalid todo: `status` must be one of pending | in_progress | completed".into()))?;
        if !STATUSES.contains(&status) {
            return Err(ToolError::InvalidArgs(
                "invalid todo: `status` must be one of pending | in_progress | completed".into(),
            ));
        }
        if !seen.insert(content.to_string()) {
            return Err(ToolError::InvalidArgs(format!(
                "invalid todos: duplicate content {content:?}"
            )));
        }
        if status == "in_progress" {
            active += 1;
        }
        todos.push((content.to_string(), status.to_string()));
    }
    if !allow_parallel && active > 1 {
        return Err(ToolError::InvalidArgs(format!(
            "invalid todos: at most one task may be in_progress (got {active})"
        )));
    }
    Ok(todos)
}

pub struct TodoWrite {
    spec: ToolSpec,
    allow_parallel: bool,
}

impl Default for TodoWrite {
    fn default() -> Self {
        Self::new(ALLOW_PARALLEL_IN_PROGRESS)
    }
}

impl TodoWrite {
    /// `allow_parallel` 独立于部署常量：测试用它走「多活跃拒绝」分支。
    pub fn new(allow_parallel: bool) -> Self {
        // 描述按 docs/04 §6.2 收敛为 ≤200 字符的紧凑英文，语义保留上游 tool-todo
        // 三段式指引（做什么/跳过琐碎/活跃档与收尾）。不逐字抄上游 350 字符文案的
        // 理由是实测的：那份文案让每请求多扛 ~220 token 的声明开销，评估门禁按
        // +27% token 阻断；schema/校验/输出文案仍与上游逐字对齐（偏差见 docs/04 §2）。
        Self {
            spec: ToolSpec::new(
                "todo_write",
                "Record and update a task list to plan multi-step work; skip it for trivial tasks. \
                 One todo per concrete step before starting; keep actively worked todos \
                 in_progress; mark each completed when done.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "The COMPLETE task list, replacing any previous list.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "content": { "type": "string", "description": "What the task is — a short imperative line." },
                                    "status": {
                                        "type": "string",
                                        "enum": STATUSES,
                                        "description": "pending (not started) | in_progress (now) | completed (done)."
                                    }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }),
            ),
            allow_parallel,
        }
    }

    /// 模型可见输出：上游 output.render 的同一句式，计数是模型跟进改状态的抓手。
    fn render(todos: &[ValidatedTodo]) -> String {
        let count = |status: &str| todos.iter().filter(|(_, s)| s == status).count();
        format!(
            "Updated todo list: {} pending, {} in progress, {} completed.",
            count("pending"),
            count("in_progress"),
            count("completed")
        )
    }
}

#[async_trait]
impl ToolExecutor for TodoWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let todos = validate(&args, self.allow_parallel)?;
        Ok(ToolOutput::ok(Self::render(&todos)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NullEventSink;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            "call-todo",
            sink,
            crate::config::SandboxMode::ReadOnly,
        )
    }

    fn todos_json(items: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "todos": items
                .iter()
                .map(|(content, status)| serde_json::json!({"content": content, "status": status}))
                .collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn valid_list_renders_counts_and_trims_content() {
        let tool = TodoWrite::default();
        let out = tool
            .execute(
                todos_json(&[(" 调研现状 ", "completed"), ("写实现", "in_progress"), ("补测试", "pending")]),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.output, "Updated todo list: 1 pending, 1 in progress, 1 completed.");
    }

    #[tokio::test]
    async fn empty_list_is_valid_and_renders_zeroes() {
        let tool = TodoWrite::default();
        let out = tool.execute(todos_json(&[]), &ctx()).await.unwrap();
        assert_eq!(out.output, "Updated todo list: 0 pending, 0 in progress, 0 completed.");
    }

    #[tokio::test]
    async fn blank_content_rejected() {
        let tool = TodoWrite::default();
        let err = tool
            .execute(todos_json(&[("   ", "pending")]), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn duplicate_content_rejected_after_trim() {
        let tool = TodoWrite::default();
        let err = tool
            .execute(todos_json(&[("同一件事", "pending"), (" 同一件事 ", "in_progress")]), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("duplicate")));
    }

    #[tokio::test]
    async fn invalid_status_rejected() {
        let tool = TodoWrite::default();
        let err = tool
            .execute(todos_json(&[("x", "done")]), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("status")));
    }

    #[tokio::test]
    async fn extra_item_key_rejected() {
        // additionalProperties: false：嵌套/扩展形状必须在这里炸响，而不是被静默展平
        // 让适配层回放出模型没写过的清单。
        let tool = TodoWrite::default();
        let err = tool
            .execute(
                serde_json::json!({"todos": [{"content": "x", "status": "pending", "priority": "high"}]}),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn second_in_progress_rejected_when_parallel_disallowed() {
        let tool = TodoWrite::new(false);
        let err = tool
            .execute(todos_json(&[("a", "in_progress"), ("b", "in_progress")]), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("in_progress")));
        // 同一份清单在部署口径（true）下合法
        let ok = TodoWrite::default()
            .execute(todos_json(&[("a", "in_progress"), ("b", "in_progress")]), &ctx())
            .await
            .unwrap();
        assert!(ok.success);
    }

    #[tokio::test]
    async fn missing_or_malformed_todos_rejected() {
        let tool = TodoWrite::default();
        for args in [serde_json::json!({}), serde_json::json!({"todos": "x"}), serde_json::json!({"todos": [1]})] {
            let err = tool.execute(args, &ctx()).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidArgs(_)));
        }
    }

    /// 工具契约（AGENTS §1 新增内置工具的证据要求）：名称/风险档/schema 必填项。
    #[test]
    fn spec_contract_matches_upstream_name_and_required_params() {
        let spec = TodoWrite::default().spec;
        assert_eq!(spec.name, "todo_write");
        assert_eq!(spec.parameters["required"][0], "todos");
        assert_eq!(spec.parameters["properties"]["todos"]["items"]["required"][0], "content");
        assert_eq!(
            spec.parameters["properties"]["todos"]["items"]["properties"]["status"]["enum"][0],
            "pending"
        );
        assert_eq!(
            spec.parameters["properties"]["todos"]["items"]["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        // 描述必须含上游三段式的使用指引（何时用 / 活跃档 / 收尾）
        assert!(spec.description.contains("skip it for trivial"));
        assert!(spec.description.contains("in_progress"));
    }
}
