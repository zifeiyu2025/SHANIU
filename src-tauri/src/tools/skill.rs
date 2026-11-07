//! `skill` 工具（SH-091）：按名加载技能完整正文。
//!
//! 对齐上游 `packages/skill/tool-skill` 的模型侧语义：
//! - 参数 `{name}`，只接受 kebab-case；
//! - 只加载 **model-invocable** 技能；
//! - 结果文本与 `/name` 用户注入共用 `skills::render_skill_content` 的
//!   `<skill_content>` 形状。
//!
//! 风险分级为 Read：技能正文是本地可信内容，但读取本身不产生写入/执行副作用；
//! 真正的写/执行仍由后续工具逐次审批。

use async_trait::async_trait;
use serde_json::Value;

use crate::skills;
use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

pub struct SkillTool {
    spec: ToolSpec,
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "skill",
                "Load the full instructions for a skill. Call it before acting on a task that names or clearly matches a skill in the session skill catalog.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The exact skill name from the available skills list."
                        }
                    },
                    "required": ["name"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for SkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("missing required string `name`".into()))?;
        let Some(skill) = skills::load(&ctx.workspace_root, name) else {
            return Err(ToolError::Execution(format!(
                "skill \"{name}\" is unknown or no longer available"
            )));
        };
        if !skill.summary.model_invocable {
            return Err(ToolError::Execution(format!(
                "skill \"{name}\" is not available for model invocation"
            )));
        }
        Ok(ToolOutput::ok(skills::render_skill_content(&skill)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shaniu-skill-tool-{tag}-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn write_skill(root: &std::path::Path, name: &str, extra: &str, body: &str) {
        let path = root.join(".dsh").join("skills").join(name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: test skill\n{extra}\n---\n{body}"),
        )
        .unwrap();
    }

    fn context(root: &PathBuf) -> ToolContext {
        ToolContext::new(
            root,
            tokio_util::sync::CancellationToken::new(),
            "call-skill",
            std::sync::Arc::new(crate::events::NullEventSink),
            crate::config::SandboxMode::WorkspaceWrite,
        )
    }

    #[tokio::test]
    async fn loads_model_invocable_skill_body() {
        let root = temp_root("ok");
        write_skill(&root, "pdf", "", "PDF BODY");
        let tool = SkillTool::new();
        let out = tool
            .execute(serde_json::json!({ "name": "pdf" }), &context(&root))
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.output.contains("<skill_content name=\"pdf\">"));
        assert!(out.output.contains("PDF BODY"));
    }

    #[tokio::test]
    async fn rejects_unknown_and_non_model_skills() {
        let root = temp_root("reject");
        write_skill(&root, "hidden", "disable-model-invocation: true", "SECRET");
        let tool = SkillTool::new();
        let unknown = tool
            .execute(serde_json::json!({ "name": "missing" }), &context(&root))
            .await
            .unwrap_err();
        assert!(matches!(unknown, ToolError::Execution(_)));
        let hidden = tool
            .execute(serde_json::json!({ "name": "hidden" }), &context(&root))
            .await
            .unwrap_err();
        assert!(matches!(hidden, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn cancelled_context_short_circuits() {
        let root = temp_root("cancel");
        let ctx = context(&root);
        ctx.cancel.cancel();
        let tool = SkillTool::new();
        let error = tool
            .execute(serde_json::json!({ "name": "pdf" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled));
    }
}
