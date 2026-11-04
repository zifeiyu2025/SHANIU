//! unified_exec（docs §8）：命令执行——tokio process、流式输出、超时、取消。
//!
//! 语义：
//! - 命令经系统 shell 执行（Windows `cmd /C`），工作目录限工作区内；
//! - stdout/stderr 逐行经 EventSink 发 `ToolCallOutputDelta`（call_id 关联），
//!   同时环形累积（总量上限保护上下文）；
//! - 超时/失败以 `ToolOutput::fail` 回填（模型可自我纠正），取消以 `ToolError::Cancelled`；
//! - 输出总量超限时截断并标注。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::events::EventMsg;
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
                        "timeout_ms": { "type": "integer", "description": "超时毫秒数，默认 120000，上限 600000" }
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
    channel: IoChannel,
    tx: mpsc::Sender<(IoChannel, String)>,
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

        // Windows：cmd /C 承载命令行；CREATE_NO_WINDOW 防止控制台弹窗
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/C", command])
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW，防控制台弹窗
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("命令启动失败: {e}")))?;

        // 读取任务：stdout/stderr 逐行送入通道（装箱统一读取类型）
        let (tx, mut rx) = mpsc::channel::<(IoChannel, String)>(256);
        if let Some(pipe) = child.stdout.take() {
            spawn_reader(Box::new(pipe), IoChannel::Stdout, tx.clone());
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_reader(Box::new(pipe), IoChannel::Stderr, tx.clone());
        }
        drop(tx);

        let mut collected: Vec<String> = Vec::new();
        let mut collected_bytes = 0usize;
        let mut truncated_note: Option<String> = None;

        let result = loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Err(ToolError::Cancelled);
                }
                _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                    let _ = child.kill().await;
                    break Ok(ToolOutput::fail(format!(
                        "命令超时（{timeout_ms}ms）被终止。已捕获输出：\n{}",
                        collected.join("\n")
                    )));
                }
                status = child.wait() => {
                    let status = status.map_err(|e| ToolError::Execution(format!("等待进程失败: {e}")))?;
                    // 排干管道余量（读取任务在 EOF 后结束）；排干限时防孤儿进程挂住
                    let drain_deadline = tokio::time::sleep(Duration::from_secs(1));
                    tokio::pin!(drain_deadline);
                    loop {
                        tokio::select! {
                            _ = &mut drain_deadline => break,
                            item = rx.recv() => match item {
                                Some((channel, line)) => {
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
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call-exec", sink)
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
