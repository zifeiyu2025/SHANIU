//! MCP server 进程监管（docs/02 §3.1.4 C-R4/C-R5 + X1 崩溃重启策略）。
//!
//! 职责只有三件：**按沙箱原语启动**、**双向管道收发**、**异常退出后的状态机**。
//! 协议关联（initialize / tools 请求）不在这里，见后续 `session` 单元。
//!
//! 重启策略（X1）：允许自动重启一次，且**每次重启前重算可执行文件哈希**并与批准记录比对；
//! 不一致 → `blocked`，绝不带着被替换的二进制复用同一份信任；第二次崩溃 → `degraded`，
//! 不无限重生。上游对比：codex 同样允许一次重启但未要求重验哈希（本项目更严）；
//! ZCode 由上层进程管理，未做此校验。

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::{McpServerConfig, SandboxMode};
use crate::sandbox::{self, StdioEvent};
use crate::sandbox::windows_impl::RestrictedStdioChild;

use super::command::{self, ResolveError};

/// 启动失败（每一种都带可诊断信息：缺什么、哪一步、怎么改）。
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error("读取可执行文件哈希失败（{path}）: {reason}")]
    Hash { path: String, reason: String },
    #[error("可执行文件哈希与批准记录不一致（批准 {approved}，实际 {actual}）：拒绝启动")]
    HashMismatch { approved: String, actual: String },
    #[error(
        "模块 `{module}` 的代码树与批准记录不一致（批准 {approved}，实际 {actual}）：拒绝启动，需重新审查"
    )]
    ModuleTreeMismatch {
        module: String,
        approved: String,
        actual: String,
    },
    #[error("无法复核模块代码树：{0}")]
    ModuleTree(String),
    #[error("沙箱启动失败: {0}")]
    Sandbox(String),
    #[error("写入 server stdin 失败: {0}")]
    Write(String),
    #[error("server 的 stdin 管道已关闭")]
    StdinClosed,
}

/// 崩溃后的处置结论（纯值，便于负向测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// 哈希一致且还有重启额度：允许重启（额度已扣）。
    Restart,
    /// 可执行文件已被替换：`blocked`，不再启动。
    Blocked { approved: String, actual: String },
    /// 重启额度已用完：`degraded`，工具移除，不无限重生。
    Degraded,
}

/// 崩溃重启状态机。**先比哈希再判额度**——被替换的二进制即使还有额度也必须 blocked。
#[derive(Debug, Clone)]
pub struct RestartGuard {
    max_restarts: u8,
    restarts_used: u8,
}

impl Default for RestartGuard {
    fn default() -> Self {
        Self::new(1)
    }
}

impl RestartGuard {
    pub fn new(max_restarts: u8) -> Self {
        Self { max_restarts, restarts_used: 0 }
    }

    pub fn restarts_used(&self) -> u8 {
        self.restarts_used
    }

    /// 一次非预期退出后调用。`approved` 来自批准记录，`actual` 是此刻重算的哈希。
    pub fn on_unexpected_exit(&mut self, approved: &str, actual: &str) -> RestartDecision {
        if !approved.eq_ignore_ascii_case(actual) {
            return RestartDecision::Blocked { approved: approved.to_string(), actual: actual.to_string() };
        }
        if self.restarts_used >= self.max_restarts {
            return RestartDecision::Degraded;
        }
        self.restarts_used += 1;
        RestartDecision::Restart
    }
}

/// 一个已启动的 MCP stdio server 及其管道。
pub struct McpProcess {
    server_id: String,
    exe: PathBuf,
    exe_sha256: String,
    child: RestrictedStdioChild,
    /// 会话层要独占读端做多路复用，因此允许被取走；未取走时 `next_event` 仍可用。
    stdout: Option<mpsc::Receiver<StdioEvent>>,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    _writer: std::thread::JoinHandle<()>,
}

impl McpProcess {
    /// 启动 server。`approved_sha256` 是批准记录里的哈希——**没有它就没有信任**，
    /// 这里不接受「跳过校验」的入口（C-R1 指纹先于连接）。
    pub async fn start(
        config: &McpServerConfig,
        sandbox_mode: SandboxMode,
        cwd: &Path,
        approved_sha256: &str,
        approved_module_tree: Option<&str>,
    ) -> Result<Self, StartError> {
        let exe = command::resolve_executable(&config.command)?;
        let actual = command::file_sha256(&exe)
            .map_err(|e| StartError::Hash { path: exe.display().to_string(), reason: e.to_string() })?;
        if !actual.eq_ignore_ascii_case(approved_sha256) {
            return Err(StartError::HashMismatch { approved: approved_sha256.to_string(), actual });
        }
        // 解释器型启动：解释器只是外壳，真正执行的模块代码也必须与批准时一致。
        // 这里重算并比对，覆盖「审查之后、启动之前」被换掉的窗口（TOCTOU）。
        let current_tree = super::module_tree::digest_for(&exe, &config.args)
            .map_err(|error| StartError::ModuleTree(error.to_string()))?;
        match (approved_module_tree, current_tree) {
            (None, None) => {}
            (Some(approved), Some(tree)) if approved.eq_ignore_ascii_case(&tree.sha256) => {}
            (approved, current) => {
                return Err(StartError::ModuleTreeMismatch {
                    module: current
                        .as_ref()
                        .map(|tree| tree.module.clone())
                        .unwrap_or_else(|| "-".to_string()),
                    approved: approved.unwrap_or("<未覆盖>").to_string(),
                    actual: current
                        .map(|tree| tree.sha256)
                        .unwrap_or_else(|| "<无法定位模块>".to_string()),
                });
            }
        }

        // `unsandboxed` 是逐服务器显式豁免（X10），配置校验已要求同时填写原因。
        let mode = if config.unsandboxed { SandboxMode::DangerFullAccess } else { sandbox_mode };
        let (tx, stdout) = mpsc::channel(64);
        let program = exe.to_string_lossy().to_string();
        let args = config.args.clone();
        let env = config.env.clone();
        let cwd = cwd.to_path_buf();

        let mut child = tokio::task::spawn_blocking(move || {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            sandbox::prepare_stdio(mode, &program, &arg_refs, &cwd, &env, tx)
        })
        .await
        .map_err(|e| StartError::Sandbox(format!("启动任务失败: {e}")))?
        .map_err(|e| StartError::Sandbox(e.to_string()))?;

        let stdin = child
            .take_stdin()
            .ok_or_else(|| StartError::Sandbox("沙箱未提供 stdin 管道".into()))?;
        let (stdin_tx, stdin_rx) = mpsc::channel(32);
        let writer = spawn_stdin_writer(stdin, stdin_rx);

        Ok(Self {
            server_id: config.id.clone(),
            exe,
            exe_sha256: actual,
            child,
            stdout: Some(stdout),
            stdin_tx,
            _writer: writer,
        })
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// 解析后的绝对路径（指纹与 UI 展示用；不会随时间变化）。
    pub fn exe_path(&self) -> &Path {
        &self.exe
    }

    pub fn exe_sha256(&self) -> &str {
        &self.exe_sha256
    }

    pub fn pid(&self) -> u32 {
        self.child.pid()
    }

    /// 写一条 NDJSON（调用方保证是不含换行的单行 JSON）。
    pub async fn send_line(&self, line: &str) -> Result<(), StartError> {
        self.stdin_tx
            .send(line.as_bytes().to_vec())
            .await
            .map_err(|_| StartError::StdinClosed)
    }

    /// 下一条 stdout 事件（行或协议错误）。返回 `None` = 读端已关闭或已被取走。
    pub async fn next_event(&mut self) -> Option<StdioEvent> {
        self.stdout.as_mut()?.recv().await
    }

    /// 取走 stdout 读端（会话层接管多路复用后，本结构不再读）。
    pub fn take_stdout(&mut self) -> Option<mpsc::Receiver<StdioEvent>> {
        self.stdout.take()
    }

    /// stdin 发送端副本（会话层读线程应答 server→client 请求时要写）。
    pub fn stdin_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.stdin_tx.clone()
    }

    /// 等待进程退出，返回退出码（取不到时 -1）。
    pub async fn wait_exit(&mut self) -> i32 {
        self.child.wait().await.unwrap_or(-1)
    }

    /// 终止进程；Job Object 会连带收掉它派生的整棵树。
    pub fn kill(&self) {
        self.child.kill();
    }

    /// stderr 尾部快照（文本, 已丢弃字节数）——崩溃诊断用，不阻塞。
    pub fn stderr_tail(&self) -> (String, u64) {
        self.child.stderr_tail()
    }

    /// 崩溃诊断文本：退出码 + stderr 尾部（截断标注由 stderr 环自己保证）。
    pub fn crash_report(&self, exit_code: i32) -> String {
        let (stderr, dropped) = self.stderr_tail();
        let dropped_note =
            if dropped > 0 { format!("\n（stderr 已丢弃前 {dropped} 字节，仅保留尾部）") } else { String::new() };
        format!(
            "MCP server `{}` 异常退出（exit={exit_code}，pid={}）。stderr 尾部:\n{}{}",
            self.server_id,
            self.pid(),
            if stderr.trim().is_empty() { "<空>" } else { stderr.trim_end() },
            dropped_note
        )
    }

    /// 重启前复核：重算可执行文件哈希**与模块代码树**，任一不一致即 blocked。
    pub fn restart_decision(
        &self,
        guard: &mut RestartGuard,
        approved_sha256: &str,
        approved_module_tree: Option<&str>,
        args: &[String],
    ) -> RestartDecision {
        let actual = match command::file_sha256(&self.exe) {
            Ok(actual) => actual,
            // 文件读不到 = 无法确认它还是批准的那个文件——按不一致处理（fail-closed）。
            Err(_) => {
                return RestartDecision::Blocked {
                    approved: approved_sha256.to_string(),
                    actual: "<无法读取>".to_string(),
                }
            }
        };
        if !actual.eq_ignore_ascii_case(approved_sha256) {
            return RestartDecision::Blocked { approved: approved_sha256.to_string(), actual };
        }
        let current_tree = match super::module_tree::digest_for(&self.exe, args) {
            Ok(tree) => tree.map(|tree| tree.sha256),
            Err(_) => None,
        };
        match (approved_module_tree, current_tree.as_deref()) {
            (None, None) => {}
            (Some(approved), Some(current)) if approved.eq_ignore_ascii_case(current) => {}
            (approved, current) => {
                return RestartDecision::Blocked {
                    approved: approved.unwrap_or("<未覆盖>").to_string(),
                    actual: current.unwrap_or("<无法定位模块>").to_string(),
                }
            }
        }
        guard.on_unexpected_exit(approved_sha256, &actual)
    }
}

/// 单写者线程：stdin 只有它写，避免多任务交叉写坏一行。
fn spawn_stdin_writer(stdin: std::fs::File, mut rx: mpsc::Receiver<Vec<u8>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        let mut stdin = stdin;
        while let Some(mut line) = rx.blocking_recv() {
            line.push(b'\n');
            if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                break;
            }
        }
    })
}

/// 等待一条 stdout 事件的便捷包装（超时返回 `None`，调用方据此判定握手超时）。
pub async fn next_event_within(process: &mut McpProcess, timeout: Duration) -> Option<StdioEvent> {
    tokio::time::timeout(timeout, process.next_event()).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_crash_restarts_then_second_degrades() {
        let mut guard = RestartGuard::default();
        assert_eq!(guard.on_unexpected_exit("abc", "abc"), RestartDecision::Restart);
        assert_eq!(guard.restarts_used(), 1);
        assert_eq!(guard.on_unexpected_exit("abc", "abc"), RestartDecision::Degraded);
        assert_eq!(guard.restarts_used(), 1, "degraded 后不再扣额度、不再重启");
    }

    #[test]
    fn hash_mismatch_blocks_before_restart_attempt() {
        let mut guard = RestartGuard::default();
        assert_eq!(
            guard.on_unexpected_exit("approved", "replaced"),
            RestartDecision::Blocked { approved: "approved".into(), actual: "replaced".into() }
        );
        assert_eq!(guard.restarts_used(), 0, "blocked 不应消耗重启额度");
    }

    #[test]
    fn hash_mismatch_on_second_crash_is_blocked_not_degraded() {
        let mut guard = RestartGuard::default();
        assert_eq!(guard.on_unexpected_exit("same", "same"), RestartDecision::Restart);
        // 第二次崩溃且二进制被换：必须是 blocked（更强的处置优先），不能只是 degraded。
        assert!(matches!(guard.on_unexpected_exit("same", "other"), RestartDecision::Blocked { .. }));
    }

    #[test]
    fn zero_restarts_degrades_immediately() {
        let mut guard = RestartGuard::new(0);
        assert_eq!(guard.on_unexpected_exit("h", "h"), RestartDecision::Degraded);
    }

    #[tokio::test]
    async fn hash_mismatch_rejects_start_without_spawning() {
        let config = McpServerConfig { id: "probe".into(), command: "cmd".into(), ..Default::default() };
        let err = match McpProcess::start(
            &config,
            SandboxMode::WorkspaceWrite,
            &std::env::temp_dir(),
            "deadbeef",
            None,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("哈希不一致必须拒绝启动"),
        };
        assert!(matches!(err, StartError::HashMismatch { .. }), "实际: {err:?}");
    }

    #[tokio::test]
    async fn restricted_stdio_streams_one_line_and_exits() {
        let exe = command::resolve_executable("cmd").expect("cmd 应可解析");
        let sha = command::file_sha256(&exe).unwrap();
        let config = McpServerConfig {
            id: "probe".into(),
            command: exe.display().to_string(),
            args: vec!["/C".into(), "echo".into(), "hello-mcp".into()],
            ..Default::default()
        };
        let mut process =
            McpProcess::start(&config, SandboxMode::WorkspaceWrite, &std::env::temp_dir(), &sha, None)
                .await
                .expect("受限沙箱应能启动 cmd");

        let event = tokio::time::timeout(Duration::from_secs(15), process.next_event())
            .await
            .expect("等待 stdout 超时")
            .expect("stdout 读端提前关闭");
        assert_eq!(event, StdioEvent::Line("hello-mcp".into()));

        let code = tokio::time::timeout(Duration::from_secs(15), process.wait_exit())
            .await
            .expect("等待退出超时");
        assert_eq!(code, 0);
    }
}
