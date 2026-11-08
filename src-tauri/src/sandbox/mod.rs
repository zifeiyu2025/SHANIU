//! 沙箱策略入口（docs §8 sandbox/mod.rs，ADR-002 三级策略，fail-closed）。
//!
//! M2-c 交付（ADR-002 分阶段）：
//!  - read-only：纯策略拒绝（不经子进程，双保险——broker 已 Deny，此处再拦）；
//!  - workspace-write：**RestrictedToken（剥离全部特权）+ JobObject（kill-on-close）**
//!    启动子进程，CREATE_SUSPENDED 先挂起 → 入 Job → 再恢复；
//!  - danger-full-access：直接执行（用户明示，UI 全程标识）。
//!
//! M3 计划：deny-ACE 工作区外精细路径封锁。
//!
//! fail-closed（FF-4）：任何沙箱准备步骤失败 → Err，**绝不降级为无沙箱执行**。

pub mod windows_impl;

use std::path::Path;

use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::SandboxMode;

pub use windows_impl::{ExecLine, RestrictedStdioChild, StdioEvent};

/// 沙箱错误（文本回填模型/日志；不包含敏感内容）。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("策略拒绝: {0}")]
    PolicyDenied(String),
    #[error("沙箱创建失败: {0}")]
    CreateFailed(String),
}

/// 准备结果：
/// - Plain：未启动的命令（danger 路径，调用方 tokio spawn）；
/// - Restricted：**已启动**的受限子进程（纯 Win32 wait/kill + Job 守卫，读线程已接 tx）。
pub enum PreparedSpawn {
    Plain(Command),
    Restricted(windows_impl::RestrictedChild),
}

/// 按策略准备子进程（Windows 实现）。tx 供受限路径的读线程回传 stdio 行。
pub fn prepare_spawn(
    policy: SandboxMode,
    program: &str,
    args: &[&str],
    cwd: &Path,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    match policy {
        // read-only：纯策略拒绝（fail-closed 双保险）
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止命令执行".into(),
        )),
        // danger-full-access：直接执行
        SandboxMode::DangerFullAccess => {
            let mut command = Command::new(program);
            command.args(args).current_dir(cwd);
            Ok(PreparedSpawn::Plain(command))
        }
        // workspace-write：受限令牌 + JobObject（已启动）
        SandboxMode::WorkspaceWrite => {
            #[cfg(windows)]
            {
                let child = windows_impl::spawn_restricted_command(program, args, cwd, tx)
                    .map_err(SandboxError::CreateFailed)?;
                Ok(PreparedSpawn::Restricted(child))
            }
            #[cfg(not(windows))]
            {
                let _ = (program, args, cwd, tx);
                Err(SandboxError::CreateFailed(
                    "workspace-write 沙箱当前仅支持 Windows（fail-closed）".into(),
                ))
            }
        }
    }
}

/// 按策略准备 MCP stdio server（SH-060，X10：默认一律走沙箱）。
///
/// - read-only：纯策略拒绝（只读沙箱不允许任何进程执行）；
/// - workspace-write：受限令牌 + Job + 句柄白名单（默认路径）；
/// - danger-full-access：**调用方必须先确认该 server 的 `unsandboxed=true` 且原因已记录**
///   （逐服务器显式豁免），这里才走未沙箱启动。
pub fn prepare_stdio(
    policy: SandboxMode,
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    match policy {
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止启动 MCP server 进程".into(),
        )),
        SandboxMode::WorkspaceWrite => {
            windows_impl::spawn_restricted_stdio(program, args, cwd, env, tx)
                .map_err(SandboxError::CreateFailed)
        }
        SandboxMode::DangerFullAccess => {
            windows_impl::spawn_unsandboxed_stdio(program, args, cwd, env, tx)
                .map_err(SandboxError::CreateFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn ws() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-sb-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn readonly_policy_fails_closed() {
        let (tx, _rx) = mpsc::channel(4);
        let err = match prepare_spawn(SandboxMode::ReadOnly, "cmd", &["/C", "echo"], &ws(), tx) {
            Err(e) => e,
            Ok(_) => panic!("readonly 应拒绝命令执行"),
        };
        assert!(matches!(err, SandboxError::PolicyDenied(_)));
    }

    #[test]
    fn danger_policy_yields_plain_command() {
        let dir = ws();
        let (tx, _rx) = mpsc::channel(4);
        let spawn = match prepare_spawn(SandboxMode::DangerFullAccess, "cmd", &["/C", "echo"], &dir, tx) {
            Ok(PreparedSpawn::Plain(command)) => command,
            _ => panic!("danger 应为 Plain"),
        };
        drop(spawn);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn readonly_policy_denies_mcp_stdio() {
        let (tx, _rx) = mpsc::channel(4);
        let err = match prepare_stdio(
            SandboxMode::ReadOnly,
            "cmd",
            &["/C", "echo"],
            &ws(),
            &Default::default(),
            tx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("readonly 应拒绝启动 MCP server"),
        };
        assert!(matches!(err, SandboxError::PolicyDenied(_)));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn workspace_write_runs_sandboxed_and_succeeds() {
        // 真实 Win32 沙箱 smoke：受限令牌 + Job 下运行 echo，退出码必须为 0
        let dir = ws();
        let (tx, mut rx) = mpsc::channel(64);
        let mut child = match prepare_spawn(SandboxMode::WorkspaceWrite, "cmd", &["/C", "echo sb-ok"], &dir, tx)
        {
            Ok(PreparedSpawn::Restricted(child)) => child,
            _ => panic!("workspace-write 应为 Restricted"),
        };
        let code = child.wait().await.unwrap();
        assert_eq!(code, 0);
        // 排干读线程输出（echo sb-ok 应出现）
        let mut found = false;
        while let Ok(line) = rx.try_recv() {
            if line.1.contains("sb-ok") {
                found = true;
                break;
            }
        }
        assert!(found, "沙箱化子进程 stdout 应含输出");
        std::fs::remove_dir_all(&dir).ok();
    }
}
