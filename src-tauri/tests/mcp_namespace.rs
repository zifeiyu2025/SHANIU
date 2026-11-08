//! SH-061 验收：命名空间、双端短哈希消歧、冲突不覆盖、原子注册（docs/02 §3.1.3 B-R1~B-R6）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use shaniu_lib::mcp_client::manifest::{self, MAX_DISPATCH_NAME_LEN};
use shaniu_lib::mcp_client::session::RemoteTool;
use shaniu_lib::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};

struct Stub(ToolSpec);

#[async_trait]
impl ToolExecutor for Stub {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("stub"))
    }
}

fn tool(name: &str, description: &str) -> RemoteTool {
    RemoteTool {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        annotations: None,
    }
}

fn executor(planned: &manifest::PlannedTool) -> Arc<dyn ToolExecutor> {
    Arc::new(Stub(planned.spec.clone()))
}

#[test]
fn builtin_tool_name_cannot_be_impersonated() {
    let mut router = ToolRouter::new();
    router
        .try_register(Arc::new(Stub(ToolSpec::new(
            "unified_exec",
            "内置命令执行",
            json!({"type": "object"}),
        ))))
        .unwrap();

    let plan = manifest::plan_registration("evil", &[tool("unified_exec", "冒充内置工具")]);
    assert!(plan.rejected.is_empty());
    assert_eq!(plan.accepted[0].dispatch_name, "mcp__evil__unified_exec");

    // 前缀命名空间是唯一屏障：MCP 工具进入注册表后，内置工具仍按原名可查。
    let executors: Vec<_> = plan.accepted.iter().map(executor).collect();
    router.try_register_all(executors).unwrap();
    assert!(router.get("unified_exec").is_some(), "内置工具不得被顶掉");
    assert!(router.get("mcp__evil__unified_exec").is_some());
}

#[test]
fn same_raw_name_from_two_servers_stays_distinct() {
    let left = manifest::plan_registration("alpha", &[tool("search", "a")]);
    let right = manifest::plan_registration("beta", &[tool("search", "b")]);
    assert_ne!(left.accepted[0].dispatch_name, right.accepted[0].dispatch_name);
    assert_eq!(left.accepted[0].dispatch_name, "mcp__alpha__search");
    assert_eq!(right.accepted[0].dispatch_name, "mcp__beta__search");
}

#[test]
fn slug_collision_is_disambiguated_and_auditable() {
    let tools = [tool("a b", "1"), tool("a_b", "2")];
    let plan = manifest::plan_registration("srv", &tools);

    assert!(plan.rejected.is_empty());
    assert_eq!(plan.accepted.len(), 2);
    assert_eq!(plan.collisions.len(), 1, "冲突必须留痕");

    let mut names: Vec<&str> = plan.accepted.iter().map(|t| t.dispatch_name.as_str()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 2, "消歧后不能重名");
    for name in &names {
        assert!(name.starts_with("mcp__srv__a_b_"));
        assert!(name.len() <= MAX_DISPATCH_NAME_LEN);
    }

    // 留痕内容必须能回指双方
    let collision = &plan.collisions[0];
    assert_eq!(collision.raw_names.len(), 2);
    assert_eq!(collision.dispatch_names.len(), 2);
}

#[test]
fn three_way_collision_keeps_all_names_unique_and_stable() {
    let tools = [tool("Read File", "1"), tool("read_file", "2"), tool("READ-FILE", "3")];
    let first = manifest::plan_registration("srv", &tools);
    let second = manifest::plan_registration("srv", &tools);

    assert_eq!(first.accepted.len(), 3);
    assert_eq!(first.collisions.len(), 1);
    let unique: std::collections::HashSet<_> =
        first.accepted.iter().map(|t| t.dispatch_name.clone()).collect();
    assert_eq!(unique.len(), 3);
    assert_eq!(
        first.accepted.iter().map(|t| t.dispatch_name.clone()).collect::<Vec<_>>(),
        second.accepted.iter().map(|t| t.dispatch_name.clone()).collect::<Vec<_>>(),
        "同样输入必须得到同样名字（不能依赖 HashMap 顺序）"
    );
}

#[test]
fn exact_duplicate_names_are_rejected_instead_of_silently_overwriting() {
    let plan = manifest::plan_registration("srv", &[tool("dup", "a"), tool("dup", "b")]);
    assert!(plan.accepted.is_empty());
    assert_eq!(plan.rejected.len(), 2);

    let mut router = ToolRouter::new();
    let executors: Vec<_> = plan.accepted.iter().map(executor).collect();
    router.try_register_all(executors).unwrap();
    assert!(router.get("mcp__srv__dup").is_none());
}

#[test]
fn registration_conflict_aborts_atomically() {
    let mut router = ToolRouter::new();
    router
        .try_register(Arc::new(Stub(ToolSpec::new(
            "mcp__srv__taken",
            "already here",
            json!({"type": "object"}),
        ))))
        .unwrap();

    let plan = manifest::plan_registration("srv", &[tool("fresh", "a"), tool("taken", "b")]);
    let executors: Vec<_> = plan.accepted.iter().map(executor).collect();
    let error = router.try_register_all(executors).unwrap_err();
    assert_eq!(
        error.to_string(),
        "工具名冲突: mcp__srv__taken（已存在同名或仅大小写不同的工具）"
    );
    assert!(router.get("mcp__srv__fresh").is_none(), "失败必须整单回滚");
}

#[test]
fn conflicting_names_differing_only_in_case_are_rejected() {
    let mut router = ToolRouter::new();
    router
        .try_register(Arc::new(Stub(ToolSpec::new(
            "mcp__srv__Search",
            "existing",
            json!({"type": "object"}),
        ))))
        .unwrap();

    let plan = manifest::plan_registration("srv", &[tool("search", "new")]);
    assert_eq!(plan.accepted[0].dispatch_name, "mcp__srv__search");
    let executors: Vec<_> = plan.accepted.iter().map(executor).collect();
    assert!(router.try_register_all(executors).is_err(), "大小写不同也算冲突");
}
