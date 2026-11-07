//! 后台作业的模型侧工具（SH-089）：`job_output` / `job_list` / `job_kill`。
//!
//! 与上游 `packages/jobs/tool-jobs` 同义：后台作业启动后，模型用这三个工具
//! 读取增量、查看整表、终止作业。它们只读写 `ToolContext.jobs`（进程内注册表），
//! 没有会话作用域时 fail-closed 返回可纠正错误。

use async_trait::async_trait;
use serde_json::Value;

use crate::jobs::JobKillValue;
use crate::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec};

pub struct JobOutput {
    spec: ToolSpec,
}

impl Default for JobOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl JobOutput {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "job_output",
                "读取后台作业的输出（非消费；from 为 UTF-8 字节偏移，缺省从保留区最早处读）。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "run_in_background 返回的作业 id" },
                        "from": { "type": "integer", "description": "起始偏移（字节），缺省为最早可读输出" }
                    },
                    "required": ["job_id"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for JobOutput {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let job_id = args
            .get("job_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 job_id".into()))?;
        let from = args.get("from").and_then(|v| v.as_u64());
        let (jobs, session) = runtime(ctx)?;
        let read = jobs.read(job_id, session, from).map_err(|error| ToolError::Execution(error.message))?;

        let mut text = String::new();
        text.push_str(&format!(
            "[job {} {} · output {}..{}]\n",
            read.job.id, read.job.status, read.job.output.earliest, read.job.output.total
        ));
        if read.lossy {
            text.push_str("[更早的输出已丢弃]\n");
        }
        for chunk in &read.chunks {
            if chunk.gap_before == Some(true) {
                text.push_str("[...]\n");
            }
            text.push_str(&chunk.text);
        }
        if read.chunks.is_empty() {
            text.push_str("（暂无新输出）\n");
        }
        text.push_str(&format!("\n[next={}]", read.next));
        Ok(ToolOutput::ok(text))
    }
}

pub struct JobList {
    spec: ToolSpec,
}

impl Default for JobList {
    fn default() -> Self {
        Self::new()
    }
}

impl JobList {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "job_list",
                "列出当前会话可见的后台作业（id/状态/命令）。",
                serde_json::json!({ "type": "object", "properties": {} }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for JobList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (jobs, session) = runtime(ctx)?;
        let rows = jobs.list_for(session);
        if rows.is_empty() {
            return Ok(ToolOutput::ok("当前没有后台作业。"));
        }
        let text = rows
            .iter()
            .map(|job| {
                let detail = job.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
                format!("{} [{}] {}{}", job.id, job.status, job.label, detail)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::ok(text))
    }
}

pub struct JobKill {
    spec: ToolSpec,
}

impl Default for JobKill {
    fn default() -> Self {
        Self::new()
    }
}

impl JobKill {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "job_kill",
                "请求终止一个后台作业；已结束的作业会如实返回 already-finished。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "要终止的作业 id" }
                    },
                    "required": ["job_id"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for JobKill {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let job_id = args
            .get("job_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 job_id".into()))?;
        let (jobs, session) = runtime(ctx)?;
        let JobKillValue { outcome } = jobs.kill(job_id, session).map_err(|error| ToolError::Execution(error.message))?;
        let text = match outcome.as_str() {
            "requested" => format!("已请求终止 {job_id}，终态将以作业状态为准。"),
            _ => format!("{job_id} 已经结束，无需终止。"),
        };
        Ok(ToolOutput::ok(text))
    }
}

fn runtime(ctx: &ToolContext) -> Result<(&crate::jobs::JobRegistry, &str), ToolError> {
    let jobs = ctx
        .jobs
        .as_deref()
        .ok_or_else(|| ToolError::Execution("后台作业不可用：缺少作业注册表".into()))?;
    let session = ctx
        .session_id
        .as_deref()
        .ok_or_else(|| ToolError::Execution("后台作业不可用：缺少会话作用域".into()))?;
    Ok((jobs, session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn context(registry: Arc<crate::jobs::JobRegistry>) -> ToolContext {
        ToolContext::new(
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            "call-jobs",
            Arc::new(crate::events::NullEventSink),
            crate::config::SandboxMode::WorkspaceWrite,
        )
        .with_jobs(Some("s1".into()), Some(registry))
    }

    #[tokio::test]
    async fn output_reads_ring_and_reports_status() {
        let registry = Arc::new(crate::jobs::JobRegistry::new());
        let (id, _rx) = registry.create("bash", "echo hi", Some("s1".into()));
        registry.append(&id, "hi\n", Some("stdout"));
        let out = JobOutput::new()
            .execute(serde_json::json!({ "job_id": id }), &context(registry))
            .await
            .unwrap();
        assert!(out.output.contains("hi"));
        assert!(out.output.contains("running"));
    }

    #[tokio::test]
    async fn list_and_kill_reflect_lifecycle() {
        let registry = Arc::new(crate::jobs::JobRegistry::new());
        let (id, mut rx) = registry.create("bash", "sleep", Some("s1".into()));
        let listed = JobList::new()
            .execute(serde_json::json!({}), &context(registry.clone()))
            .await
            .unwrap();
        assert!(listed.output.contains(&id));

        let killed = JobKill::new()
            .execute(serde_json::json!({ "job_id": id }), &context(registry.clone()))
            .await
            .unwrap();
        assert!(killed.output.contains("已请求终止"));
        assert!(rx.try_recv().is_ok());

        registry.finish(&id, "killed", None);
        let again = JobKill::new()
            .execute(serde_json::json!({ "job_id": id }), &context(registry))
            .await
            .unwrap();
        assert!(again.output.contains("已经结束"));
    }

    #[tokio::test]
    async fn missing_scope_fails_closed() {
        let ctx = ToolContext::new(
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            "call-jobs",
            Arc::new(crate::events::NullEventSink),
            crate::config::SandboxMode::WorkspaceWrite,
        );
        let error = JobList::new().execute(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(matches!(error, ToolError::Execution(_)));
    }
}
