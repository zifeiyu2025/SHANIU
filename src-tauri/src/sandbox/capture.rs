//! 捕获式命令执行（写后验证闸门 + 后台作业共用）。
//!
//! 为什么在 `sandbox` 而不是 `tools`：调用方有两层——`tools`（命令工具）与
//! `agent_loop`（收尾验证）。放在工具层会让 `agent_loop → tools` 成为反向依赖
//! （docs/01 §2.3.1）。派生、管道、整树回收本来就属于沙箱职责，下沉到这里之后
//! 两条调用路径共用同一套语义，不会在沙箱行为上分叉。
//!
//! 与 `tools/unified_exec.rs` 的关系：那边负责「流式回填 + 事件」，这里只负责
//! 「起进程 → 收输出 → 等退出/超时 → 杀整树」。

use std::io;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{current_os_mode, prepare_spawn_with, ExecLine, PreparedSpawn, SpawnRequest};

/// 单行原始字节上限（`read_until` 本身无界）。
///
/// 一条永远不换行的输出（`printf` 几百 MB、`yes` 被截断在半行）会把行缓冲撑到 OOM，
/// 而总量上限是**收集之后**才生效，救不了读取阶段。超限按一行先发出去、继续读下一段：
/// 内容不丢、行序不变，只是超长行会被切成多条。
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;
/// 捕获路径的输出总量上限（超出即丢弃后续行，不中断进程）。
const MAX_CAPTURED_BYTES: usize = 128 * 1024;

/// 子进程等待/终止的统一抽象（Plain=tokio Child / Restricted=spawn_blocking + Job）。
pub enum Waiter {
    Tokio(Box<tokio::process::Child>),
    Restricted(Box<super::windows_impl::RestrictedChild>),
}

impl Waiter {
    pub async fn status(&mut self) -> io::Result<std::process::ExitStatus> {
        match self {
            Waiter::Tokio(child) => child.wait().await,
            Waiter::Restricted(child) => {
                // 约定：退出码 i32（Windows exit code u32 的有符号视图）
                let code = child.wait().await?;
                Ok(std::process::ExitStatus::from_raw(code as u32))
            }
        }
    }

    pub fn kill(&mut self) {
        match self {
            Waiter::Tokio(child) => {
                let _ = child.start_kill();
            }
            Waiter::Restricted(child) => child.kill(),
        }
    }
}

/// 逐行读管道并解码：调用方按 `(channel, line)` 消费。
pub fn spawn_reader(
    pipe: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    channel: u8,
    tx: mpsc::Sender<ExecLine>,
    encodings: crate::text_encoding::EncodingLedger,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut decoder = crate::text_encoding::StreamDecoder::with_system_oem();
        let mut raw: Vec<u8> = Vec::new();
        'read: loop {
            raw.clear();
            // 有界读取：`take` 封顶后 `read_until` 到上限即返回（不带换行），下一轮接着读。
            let read = {
                let mut limited = (&mut reader).take(MAX_LINE_BYTES as u64);
                limited.read_until(b'\n', &mut raw).await
            };
            match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            while matches!(raw.last(), Some(b'\n') | Some(b'\r')) {
                raw.pop();
            }
            for decoded in decoder.push_line(&raw) {
                if tx.send((channel, decoded)).await.is_err() {
                    break 'read;
                }
            }
            if let Some(encoding) = decoder.decision() {
                encodings.record(channel, encoding);
            }
        }
        for decoded in decoder.finish() {
            if tx.send((channel, decoded)).await.is_err() {
                break;
            }
        }
        if let Some(encoding) = decoder.decision() {
            encodings.record(channel, encoding);
        }
    })
}

/// 宿主侧验证命令的捕获结果。
pub struct CapturedRun {
    pub exit_code: i32,
    /// 超时被杀：退出码不可信，必须与「正常非零」区分开。
    pub timed_out: bool,
    pub output: String,
    pub duration_ms: u64,
}

/// 捕获式执行的失败形态（与工具回填的成功/失败分开）。
#[derive(Debug)]
pub enum CaptureError {
    /// 进程起不来（命令不存在、shell 缺失）。
    Spawn(String),
    /// 沙箱拒绝派生。
    Denied(String),
    Cancelled,
    /// 等待进程失败。
    Wait(String),
}

/// 执行一条命令并**捕获**输出（不发流式 delta、不经过审批）。
///
/// 与 `tools` 的流式路径共用 [`prepare_spawn_with`] 与同一套
/// 「启动 → 排干管道 → kill 整树」路径：验证不能另开执行引擎，否则沙箱语义
/// 会在两条路径上分叉。
pub async fn run_capture(
    policy: crate::config::SandboxMode,
    workspace_root: &Path,
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<CapturedRun, CaptureError> {
    let started = std::time::Instant::now();
    let (tx, mut rx) = mpsc::channel::<ExecLine>(256);
    let encodings = crate::text_encoding::EncodingLedger::new();
    let mut waiter = match prepare_spawn_with(
        SpawnRequest {
            policy,
            workspace_root,
            program: "cmd",
            args: &["/C", command],
            cwd,
            os: current_os_mode(),
        },
        tx.clone(),
    ) {
        Ok(PreparedSpawn::Plain(mut cmd)) => {
            cmd.stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::null());
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000);
            let mut child = cmd
                .spawn()
                .map_err(|e| CaptureError::Spawn(format!("命令启动失败: {e}")))?;
            if let Some(pipe) = child.stdout.take() {
                spawn_reader(Box::new(pipe), 0, tx.clone(), encodings.clone());
            }
            if let Some(pipe) = child.stderr.take() {
                spawn_reader(Box::new(pipe), 1, tx.clone(), encodings.clone());
            }
            Waiter::Tokio(Box::new(child))
        }
        Ok(PreparedSpawn::Restricted(child)) => Waiter::Restricted(Box::new(child)),
        Err(e) => return Err(CaptureError::Denied(e.to_string())),
    };
    drop(tx);

    let mut collected: Vec<String> = Vec::new();
    let mut collected_bytes = 0usize;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                waiter.kill();
                return Err(CaptureError::Cancelled);
            }
            _ = &mut deadline => {
                waiter.kill();
                return Ok(CapturedRun {
                    exit_code: -1,
                    timed_out: true,
                    output: collected.join("\n"),
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
            status = waiter.status() => {
                let status = status.map_err(|e| CaptureError::Wait(format!("等待进程失败: {e}")))?;
                // 排干管道余量，限时防孤儿进程挂住（与工具路径同口径）。
                let drain_deadline = tokio::time::sleep(Duration::from_secs(1));
                tokio::pin!(drain_deadline);
                loop {
                    tokio::select! {
                        _ = &mut drain_deadline => break,
                        item = rx.recv() => match item {
                            Some((_, line)) => {
                                if collected_bytes < MAX_CAPTURED_BYTES {
                                    collected_bytes += line.len() + 1;
                                    collected.push(line);
                                }
                            }
                            None => break,
                        }
                    }
                }
                return Ok(CapturedRun {
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                    output: collected.join("\n"),
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
            Some((_, line)) = rx.recv() => {
                if collected_bytes < MAX_CAPTURED_BYTES {
                    collected_bytes += line.len() + 1;
                    collected.push(line);
                }
            }
        }
    }
}
