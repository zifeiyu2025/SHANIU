//! unified_exec（docs §8）：命令执行——tokio process、流式输出、超时、取消。
//!
//! 语义：
//! - 命令经系统 shell 执行（Windows `cmd /C`），工作目录限工作区内；
//! - stdout/stderr 逐行经 EventSink 发 `ToolCallOutputDelta`（call_id 关联），
//!   同时环形累积（总量上限保护上下文）；
//! - 超时/失败以 `ToolOutput::fail` 回填（模型可自我纠正），取消以 `ToolError::Cancelled`；
//! - 输出总量超限时截断并标注。

use std::io;
use std::os::windows::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::events::EventMsg;
use crate::sandbox;
use crate::tool_router::{
    resolve_in_workspace, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_COLLECTED_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoChannel {
    Stdout,
    Stderr,
}

pub struct UnifiedExec {
    spec: ToolSpec,
}

impl Default for UnifiedExec {
    fn default() -> Self {
        Self::new()
    }
}

impl UnifiedExec {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "unified_exec",
                "在工作区内执行 shell 命令（Windows cmd），实时回传 stdout/stderr。超时默认 120s。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令行" },
                        "cwd": { "type": "string", "description": "工作目录（工作区相对路径，默认根）" },
                        "timeout_ms": { "type": "integer", "description": "超时毫秒数，默认 120000，上限 600000" },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "在后台运行：立即返回作业 id，输出改用 job_output 读取，可用 job_kill 终止"
                        }
                    },
                    "required": ["command"]
                }),
            ),
        }
    }

    fn emit_delta(&self, ctx: &ToolContext, channel: IoChannel, line: &str) {
        let _ = ctx.sink.emit(&EventMsg::ToolCallOutputDelta {
            call_id: ctx.call_id.clone(),
            channel: if channel == IoChannel::Stdout {
                crate::events::IoChannel::Stdout
            } else {
                crate::events::IoChannel::Stderr
            },
            delta: format!("{line}\n"),
        });
    }
}

/// 逐行读取子进程管道送入通道；EOF（管道关闭）后任务自然结束。
fn spawn_reader(
    pipe: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    channel: u8,
    tx: mpsc::Sender<sandbox::ExecLine>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if tx.send((channel, trimmed)).await.is_err() {
                break;
            }
            line.clear();
        }
    })
}

/// 后台作业 worker：脱离工具调用独立运行，输出进注册表环形缓冲，
/// 由 `job_kill` 的终止信号或超时结束；退出码落定终态。
fn spawn_background_worker(
    jobs: std::sync::Arc<crate::jobs::JobRegistry>,
    job_id: String,
    mut waiter: Waiter,
    mut rx: mpsc::Receiver<sandbox::ExecLine>,
    mut kill_rx: tokio::sync::oneshot::Receiver<()>,
    timeout_ms: u64,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut kill_rx => {
                    waiter.kill();
                    jobs.finish(&job_id, "killed", Some("kill requested".into()));
                    return;
                }
                _ = &mut deadline => {
                    waiter.kill();
                    jobs.finish(
                        &job_id,
                        "failed",
                        Some(format!("命令超时（{timeout_ms}ms）被终止")),
                    );
                    return;
                }
                status = waiter.status() => {
                    // 排干管道余量；限时防孤儿进程挂住。
                    let drain_deadline = tokio::time::sleep(Duration::from_secs(1));
                    tokio::pin!(drain_deadline);
                    loop {
                        tokio::select! {
                            _ = &mut drain_deadline => break,
                            item = rx.recv() => match item {
                                Some((channel, line)) => append_line(&jobs, &job_id, channel, &line),
                                None => break,
                            }
                        }
                    }
                    match status {
                        Ok(status) => {
                            let code = status.code().unwrap_or(-1);
                            let state = if code == 0 { "completed" } else { "failed" };
                            jobs.finish(&job_id, state, Some(format!("exit code: {code}")));
                        }
                        Err(error) => {
                            jobs.finish(&job_id, "failed", Some(format!("等待进程失败: {error}")));
                        }
                    }
                    return;
                }
                Some((channel, line)) = rx.recv() => append_line(&jobs, &job_id, channel, &line),
            }
        }
    });
}

fn append_line(jobs: &crate::jobs::JobRegistry, job_id: &str, channel: u8, line: &str) {
    let channel = if channel == 0 { Some("stdout") } else { Some("stderr") };
    jobs.append(job_id, &format!("{line}\n"), channel);
}

#[async_trait]
impl ToolExecutor for UnifiedExec {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 command".into()))?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidArgs("command 为空".into()));
        }
        let cwd_arg = args.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        let cwd = resolve_in_workspace(&ctx.workspace_root, cwd_arg)?;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidArgs(format!("工作目录不存在: {cwd_arg}")));
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(100, MAX_TIMEOUT_MS);
        let run_in_background = args
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // stdio 行通道（Plain=tokio 读任务 / Restricted=std 读线程，统一回传）
        let (tx, mut rx) = mpsc::channel::<sandbox::ExecLine>(256);

        // 按沙箱策略启动（fail-closed：沙箱准备失败 → Execution，执行数为 0）
        let mut waiter = match sandbox::prepare_spawn(
            ctx.sandbox,
            "cmd",
            &["/C", command],
            &cwd,
            tx.clone(),
        ) {
            Ok(sandbox::PreparedSpawn::Plain(mut cmd)) => {
                cmd.stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null());
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW，防控制台弹窗
                let mut child = cmd
                    .spawn()
                    .map_err(|e| ToolError::Execution(format!("命令启动失败: {e}")))?;
                if let Some(pipe) = child.stdout.take() {
                    spawn_reader(Box::new(pipe), 0, tx.clone());
                }
                if let Some(pipe) = child.stderr.take() {
                    spawn_reader(Box::new(pipe), 1, tx.clone());
                }
                Waiter::Tokio(Box::new(child))
            }
            Ok(sandbox::PreparedSpawn::Restricted(child)) => {
                // 受限子进程已启动（读线程已在 windows_impl 内接 tx）；
                // wait/kill 经 RestrictedChild 纯 Win32 封装，Job 守卫随结构存活
                Waiter::Restricted(Box::new(child))
            }
            Err(e) => return Err(ToolError::SandboxDenied(e.to_string())),
        };
        drop(tx);

        if run_in_background {
            let (Some(jobs), Some(session_id)) = (ctx.jobs.clone(), ctx.session_id.clone()) else {
                waiter.kill();
                return Err(ToolError::InvalidArgs(
                    "run_in_background 需要会话作用域（请从会话内调用）".into(),
                ));
            };
            let (job_id, kill_rx) = jobs.create("bash", command, Some(session_id));
            jobs.append(&job_id, &format!("$ {command}\n"), None);
            spawn_background_worker(jobs, job_id.clone(), waiter, rx, kill_rx, timeout_ms);
            return Ok(ToolOutput::ok(format!(
                "后台作业已启动: {job_id}\n命令: {command}\n用 job_output 读取增量输出，job_list 查看作业列表，job_kill 终止。"
            )));
        }

        let mut collected: Vec<String> = Vec::new();
        let mut collected_bytes = 0usize;
        let mut truncated_note: Option<String> = None;

        // 超时必须在循环外固定截止点：在 select! 里每次迭代重建 sleep 会让
        // 持续输出的长命令永远等不到超时。
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);

        let result = loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    waiter.kill();
                    return Err(ToolError::Cancelled);
                }
                _ = &mut deadline => {
                    waiter.kill();
                    break Ok(ToolOutput::fail(format!(
                        "命令超时（{timeout_ms}ms）被终止。已捕获输出：\n{}",
                        collected.join("\n")
                    )));
                }
                status = waiter.status() => {
                    let status = status.map_err(|e| ToolError::Execution(format!("等待进程失败: {e}")))?;
                    // 排干管道余量（读取任务在 EOF 后结束）；排干限时防孤儿进程挂住
                    let drain_deadline = tokio::time::sleep(Duration::from_secs(1));
                    tokio::pin!(drain_deadline);
                    loop {
                        tokio::select! {
                            _ = &mut drain_deadline => break,
                            item = rx.recv() => match item {
                                Some((channel, line)) => {
                                    let channel = to_io_channel(channel);
                                    if collected_bytes < MAX_COLLECTED_BYTES {
                                        self.emit_delta(ctx, channel, &line);
                                        collected_bytes += line.len() + 1;
                                        collected.push(line);
                                    } else if truncated_note.is_none() {
                                        truncated_note = Some("[输出超限，已截断]".into());
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                    let code = status.code().unwrap_or(-1);
                    let mut text = collected.join("\n");
                    if let Some(note) = truncated_note {
                        text.push_str(note.as_str());
                    }
                    text.push_str(&format!("\n[exit code: {code}]"));
                    break if code == 0 {
                        Ok(ToolOutput::ok(text))
                    } else {
                        Ok(ToolOutput::fail(text))
                    };
                }
                Some((channel, line)) = rx.recv() => {
                    let channel = to_io_channel(channel);
                    if collected_bytes < MAX_COLLECTED_BYTES {
                        self.emit_delta(ctx, channel, &line);
                        collected_bytes += line.len() + 1;
                        collected.push(line);
                    } else if truncated_note.is_none() {
                        truncated_note = Some("[输出超限，已截断]".into());
                    }
                }
            }
        };
        result
    }
}

fn to_io_channel(code: u8) -> IoChannel {
    if code == 0 { IoChannel::Stdout } else { IoChannel::Stderr }
}

/// 子进程等待/终止的统一抽象（Plain=tokio Child / Restricted=spawn_blocking + Job）。
enum Waiter {
    Tokio(Box<tokio::process::Child>),
    Restricted(Box<sandbox::windows_impl::RestrictedChild>),
}

impl Waiter {
    async fn status(&mut self) -> io::Result<ExitStatus> {
        match self {
            Waiter::Tokio(child) => child.wait().await,
            Waiter::Restricted(child) => {
                // 约定：退出码 i32（Windows exit code u32 的有符号视图）
                let code = child.wait().await?;
                Ok(ExitStatus::from_raw(code as u32))
            }
        }
    }

    fn kill(&mut self) {
        match self {
            Waiter::Tokio(child) => {
                let _ = child.start_kill();
            }
            Waiter::Restricted(child) => child.kill(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_router::ToolRouter;
    use std::path::Path;
    use std::sync::Arc;
    
    fn ctx(ws: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(crate::events::NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call-exec", sink, crate::config::SandboxMode::WorkspaceWrite)
    }

    fn workspace() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-exec-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn echo_command_streams_output() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "echo hello-exec"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.output.contains("hello-exec"));
        assert!(out.output.contains("[exit code: 0]"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn non_zero_exit_marks_failure() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "cmd /C exit 7"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("[exit code: 7]"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": "ping -n 10 127.0.0.1", "timeout_ms": 300}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("超时"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn cancel_returns_cancelled() {
        let ws = workspace();
        let context = ctx(&ws);
        let context = std::sync::Arc::new(context);
        let executor = std::sync::Arc::new(UnifiedExec::new());
        let handle = tokio::spawn({
            let ctx = context.clone();
            let executor = executor.clone();
            async move { executor.execute(serde_json::json!({"command": "ping -n 10 127.0.0.1"}), &ctx).await }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        context.cancel.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn cwd_outside_workspace_rejected() {
        let ws = workspace();
        let err = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": "echo x", "cwd": ".."}),
                &ctx(&ws),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn missing_command_is_invalid_args() {
        let ws = workspace();
        let err = UnifiedExec::new()
            .execute(serde_json::json!({}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn background_job_collects_output_and_settles() {
        let ws = workspace();
        let registry = std::sync::Arc::new(crate::jobs::JobRegistry::new());
        let context = ctx(&ws).with_jobs(Some("s1".into()), Some(registry.clone()));
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": "echo bg-hello", "run_in_background": true}),
                &context,
            )
            .await
            .unwrap();
        assert!(out.success);
        let id = out
            .output
            .split_whitespace()
            .nth(1)
            .expect("输出应含作业 id")
            .to_string();
        for _ in 0..100 {
            if registry.read(&id, "s1", None).unwrap().job.status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let read = registry.read(&id, "s1", None).unwrap();
        assert_eq!(read.job.status, "completed");
        assert_eq!(read.job.detail.as_deref(), Some("exit code: 0"));
        assert!(read.chunks.iter().any(|chunk| chunk.text.contains("bg-hello")));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn background_job_can_be_killed() {
        let ws = workspace();
        let registry = std::sync::Arc::new(crate::jobs::JobRegistry::new());
        let context = ctx(&ws).with_jobs(Some("s1".into()), Some(registry.clone()));
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": "ping -n 30 127.0.0.1", "run_in_background": true}),
                &context,
            )
            .await
            .unwrap();
        let id = out.output.split_whitespace().nth(1).unwrap().to_string();
        let killed = registry.kill(&id, "s1").unwrap();
        assert_eq!(killed.outcome, "requested");
        for _ in 0..100 {
            if registry.read(&id, "s1", None).unwrap().job.status == "killed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(registry.read(&id, "s1", None).unwrap().job.status, "killed");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn router_dispatches_unified_exec() {
        let ws = workspace();
        let mut router = ToolRouter::new();
        router.register(std::sync::Arc::new(UnifiedExec::new()));
        let out = router
            .execute("unified_exec", serde_json::json!({"command": "echo routed"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.output.contains("routed"));
        std::fs::remove_dir_all(&ws).ok();
    }
}
