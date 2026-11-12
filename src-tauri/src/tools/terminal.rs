//! 持久交互终端（SH-070，docs/02 ADR-106）。
//!
//! 与 `unified_exec` 的分工：一次性的「执行 → 收输出 → 退出」走 `unified_exec`；
//! 需要**同一个 shell 会话多轮交互**（起 dev server、等日志、再输命令、需要看提示）走
//! `terminal`。底层是 ConPTY，但仍走 `sandbox::prepare_terminal` 的三层边界
//! （受限令牌 + Job + AppContainer），不存在「终端 = 无沙箱旁路」。
//!
//! 四个动作：`open` / `write` / `read` / `close`。设计取舍：
//! - **read 是消费式**（读走即前进游标）：模型的上下文有限，重复读到旧输出只会让
//!   「有没有新输出」变得无法判断；需要保留的原文由 Agent 自己写进文件。
//! - **read 可以等**（`timeout_ms`）：dev server 启动需要时间，让模型用一次 read 等
//!   2–5 秒，比让模型忙轮询更省轮次。
//! - **会话有上限**（4 个）：终端是重资源（conhost + 进程树），不设上限会被一次任务
//!   开出一堆孤儿；达到上限直接报错并提示先 close，而不是静默回收别人的会话。
//! - **关闭即整树终止**（R106）：用户/模型 close 或进程退出时，Job 句柄释放 →
//!   内核回收整棵子树，不依赖 shell 自觉退出（实测见下方 `process_tree_is_reclaimed`）。
//!
//! 会话登记表是**进程级**的：工具实例每个任务重建，但终端必须跨轮次/跨任务存活，
//! 否则 `open` 之后下一次工具调用就找不到会话。因此登记表放在 `OnceLock` 里，
//! 整个进程一份（SH-071 的前端「接管」也直接复用这份）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Notify;

use crate::sandbox::{self, TerminalRequest};
use crate::tool_router::{
    resolve_in_workspace, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

/// 同时存活的终端会话上限。
const MAX_SESSIONS: usize = 4;
/// 每个会话保留的输出上限（字节）。超出即丢最早部分，并显式告知模型「有丢弃」。
const BUFFER_LIMIT: usize = 256 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BYTES: usize = 32 * 1024;
const MAX_READ_BYTES: usize = 64 * 1024;
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;

/// 进程级会话登记表（见模块注释：必须跨任务存活）。
static SESSIONS: OnceLock<Arc<TerminalSessions>> = OnceLock::new();

/// 当前进程的终端会话表（SH-071 的前端接管也从这里取）。
pub fn sessions() -> Arc<TerminalSessions> {
    SESSIONS
        .get_or_init(|| Arc::new(TerminalSessions::new()))
        .clone()
}

// ============================== 输出缓冲 ==============================

#[derive(Default)]
struct BufferState {
    text: String,
    /// 已「读走」的字节位置（UTF-8 字节偏移，始终落在字符边界上）。
    cursor: usize,
    /// 因超上限被丢弃的字节数（累计）。
    dropped: usize,
    /// 已向调用方报告过的丢弃字节数。
    dropped_reported: usize,
    /// 读线程已结束（shell 退出或伪控制台关闭）。
    closed: bool,
}

/// 有界输出缓冲 + 新输出通知。
struct OutputBuffer {
    state: Mutex<BufferState>,
    notify: Notify,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            state: Mutex::new(BufferState::default()),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BufferState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push(&self, chunk: &str) {
        {
            let mut state = self.lock();
            state.text.push_str(chunk);
            if state.text.len() > BUFFER_LIMIT {
                let excess = state.text.len() - BUFFER_LIMIT;
                let mut cut = excess.min(state.text.len());
                while cut < state.text.len() && !state.text.is_char_boundary(cut) {
                    cut += 1;
                }
                state.text.drain(..cut);
                state.cursor = state.cursor.saturating_sub(cut);
                state.dropped += cut;
                state.dropped_reported = state.dropped_reported.min(state.dropped);
            }
        }
        // `notify_one` 会留一个许可：push 与 read 之间即使没人在等，下一次 wait 立即返回。
        self.notify.notify_one();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }
}

/// 增量 UTF-8 解码：ConPTY 输出是 UTF-8，但一次 read 可能切在多字节字符中间。
#[derive(Default)]
struct Utf8Tail {
    pending: Vec<u8>,
}

impl Utf8Tail {
    fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut out = String::new();
        let mut offset = 0;
        while offset < buf.len() {
            match std::str::from_utf8(&buf[offset..]) {
                Ok(text) => {
                    out.push_str(text);
                    offset = buf.len();
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // valid_up_to 保证这一段是完整 UTF-8。
                        out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[offset..offset + valid]) });
                        offset += valid;
                    }
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            offset += len;
                        }
                        None => break, // 不完整尾部，留到下次
                    }
                }
            }
        }
        self.pending = buf[offset..].to_vec();
        out
    }
}

// ============================== 会话 ==============================

struct Session {
    id: String,
    /// 属主会话（`ToolContext.session_id`）：跨会话拿不到别人的终端。
    owner: Option<String>,
    buffer: Arc<OutputBuffer>,
    writer: Mutex<Option<std::fs::File>>,
    /// 最近一次 `write` 的命令原文（去掉尾部换行）。写后验证的「通过计入」靠它
    /// 与验证命令做**逐字**比对：终端里的命令是模型手输的，不能靠模糊匹配猜。
    last_command: Mutex<Option<String>>,
    /// `None` = 已被 close 取走（随后立即 Drop → 整树回收）。
    child: Mutex<Option<sandbox::windows_impl::TerminalChild>>,
}

impl Session {
    fn write(&self, data: &str) -> Result<usize, String> {
        // 先记原文再写：即使写入失败，也不会把「上一条命令」错当成这一条。
        // 尾部换行是「按回车」的输入语法，不属于命令本身。
        *self
            .last_command
            .lock()
            .unwrap_or_else(|p| p.into_inner()) =
            Some(data.trim_end_matches(['\r', '\n']).to_string());
        let mut guard = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let writer = guard.as_mut().ok_or_else(|| "终端会话已关闭".to_string())?;
        let payload = normalize_input(data);
        writer
            .write_all(payload.as_bytes())
            .map_err(|e| format!("写入终端失败: {e}"))?;
        writer.flush().map_err(|e| format!("刷新终端输入失败: {e}"))?;
        Ok(payload.len())
    }

    /// 把「这条命令跑过且成功」记进宿主侧的通过观测。
    ///
    /// 只认三件事同时成立：会话已关闭（shell 退出，短时间内不会再改代码）、
    /// 退出码为 0、并且有最近一次命令原文。非 0 一律不记——本期只做「通过计入」。
    fn observe_pass(&self, ctx: &ToolContext, exit: Option<u32>, closed: bool) {
        if !closed || exit != Some(0) {
            return;
        }
        let Some(log) = &ctx.verify_passes else {
            return;
        };
        if let Some(command) = self
            .last_command
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_deref()
        {
            log.record_pass(command);
        }
    }

    fn exit_code(&self) -> Option<u32> {
        let guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
        guard.as_ref().and_then(|child| child.exit_code())
    }

    fn close(&self) {
        let mut guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
        // Drop 会 TerminateProcess + ClosePseudoConsole + 释放 Job（整树回收）。
        drop(guard.take());
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }
        self.buffer.close();
    }
}

/// 模型输入里的换行在真实终端里必须变成回车（Enter）。`\r\n` 只算一次回车。
fn normalize_input(data: &str) -> String {
    data.replace("\r\n", "\r").replace('\n', "\r")
}

/// 会话表。所有操作都校验属主，避免一个会话读到另一个会话的终端。
pub struct TerminalSessions {
    inner: Mutex<BTreeMap<String, Arc<Session>>>,
}

impl Default for TerminalSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalSessions {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Session>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn get(&self, session_id: &str, owner: Option<&str>) -> Result<Arc<Session>, String> {
        let session = self
            .map()
            .get(session_id)
            .cloned()
            .ok_or_else(|| format!("终端会话不存在: {session_id}（可能已 close 或属于其它会话）"))?;
        if session.owner.as_deref() != owner {
            return Err(format!("终端会话 {session_id} 不属于当前会话"));
        }
        Ok(session)
    }

    fn remove(&self, session_id: &str, owner: Option<&str>) -> Result<Arc<Session>, String> {
        let session = self.get(session_id, owner)?;
        self.map().remove(session_id);
        Ok(session)
    }

    /// 当前存活会话数（SH-071 面板与测试用）。
    pub fn len(&self) -> usize {
        self.map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[cfg(test)]
    fn peek(&self, session_id: &str) -> Option<Arc<Session>> {
        self.map().get(session_id).cloned()
    }
}

// ============================== 工具 ==============================

pub struct Terminal {
    spec: ToolSpec,
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "terminal",
                "持久交互终端（ConPTY）。适合「需要同一个 shell 多轮交互」的场景：\
起 dev server 并读启动日志、看提示符后继续输入、需要保留环境变量/当前目录。\
一次性命令请用 unified_exec，不要用本工具。\
动作：open（打开会话，返回 session_id）、write（写输入；\\n 会按终端语义转成回车）、\
read（读取**自上次 read 之后**的新输出，可等待）、close（关闭会话并终止整棵进程树）。\
建议节奏：open → write \"<命令>\" → read；命令没跑完就再 read，不要重复 write。",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["open", "write", "read", "close"],
                            "description": "要执行的动作"
                        },
                        "session_id": {
                            "type": "string",
                            "description": "open 返回的会话 id；除 open 外必填"
                        },
                        "data": {
                            "type": "string",
                            "description": "write 的输入原文。换行会被当作回车（Enter）"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "open 的工作目录（相对工作区；缺省工作区根）"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "read 的最长等待毫秒数，默认 5000，上限 60000"
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "read 单次最多返回的字节数，默认 32768，上限 65536"
                        }
                    },
                    "required": ["action"]
                }),
            ),
        }
    }

    fn sessions(&self) -> Arc<TerminalSessions> {
        sessions()
    }

    fn open(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let registry = self.sessions();
        if registry.len() >= MAX_SESSIONS {
            return Err(ToolError::Execution(format!(
                "终端会话已达上限 {MAX_SESSIONS} 个，请先 close 不用的会话再 open"
            )));
        }
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(path) if !path.trim().is_empty() => resolve_in_workspace(&ctx.workspace_root, path)?,
            _ => ctx.workspace_root.clone(),
        };
        let cols = args
            .get("cols")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_COLS as u64)
            .clamp(20, 500) as u16;
        let rows = args
            .get("rows")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_ROWS as u64)
            .clamp(5, 200) as u16;

        // v1 固定 cmd.exe：Windows 上唯一保证存在的 shell，且与 unified_exec 口径一致。
        let program = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let spawned = sandbox::prepare_terminal(&TerminalRequest {
            policy: ctx.sandbox,
            os: sandbox::current_os_mode(),
            workspace_root: &ctx.workspace_root,
            program: &program,
            args: &[],
            cwd: &cwd,
            cols,
            rows,
        })
        .map_err(|error| match error {
            sandbox::SandboxError::PolicyDenied(message) => ToolError::SandboxDenied(message),
            sandbox::SandboxError::CreateFailed(message) => ToolError::SandboxDenied(message),
        })?;

        let id = format!("term-{}", ulid::Ulid::generate());
        let buffer = Arc::new(OutputBuffer::new());
        let session = Arc::new(Session {
            id: id.clone(),
            owner: ctx.session_id.clone(),
            buffer: buffer.clone(),
            writer: Mutex::new(Some(spawned.writer)),
            last_command: Mutex::new(None),
            child: Mutex::new(Some(spawned.child)),
        });

        // 读线程只持有 reader + buffer：**不持有 Session**，否则 close 后 Arc 不归零、
        // Job 不释放、整树回收失效。线程随输出管道 EOF 自然结束。
        let reader = spawned.reader;
        let thread_buffer = buffer.clone();
        let spawned_thread = std::thread::Builder::new()
            .name(format!("pty-{id}"))
            .spawn(move || {
                let mut reader = reader;
                let mut decoder = Utf8Tail::default();
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let text = decoder.push(&chunk[..read]);
                            if !text.is_empty() {
                                thread_buffer.push(&text);
                            }
                        }
                    }
                }
                thread_buffer.close();
            });
        if let Err(error) = spawned_thread {
            session.close();
            return Err(ToolError::Execution(format!("启动终端读线程失败: {error}")));
        }

        registry.map().insert(id.clone(), session);
        Ok(ToolOutput::ok(format!(
            "终端已打开。\n[terminal {id} · 运行中 · cwd={}]\n用 write 输入命令、read 读取输出、close 关闭。",
            shell_usable_display(&cwd)
        )))
    }

    async fn write(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let session_id = required_str(args, "session_id")?;
        let data = required_str(args, "data")?;
        let session = self
            .sessions()
            .get(session_id, ctx.session_id.as_deref())
            .map_err(ToolError::Execution)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let written = session.write(data).map_err(ToolError::Execution)?;
        Ok(ToolOutput::ok(format!("已写入 {written} 字节到终端 {session_id}。")))
    }

    async fn read(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let session_id = required_str(args, "session_id")?;
        let session = self
            .sessions()
            .get(session_id, ctx.session_id.as_deref())
            .map_err(ToolError::Execution)?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_BYTES as u64)
            .clamp(1, MAX_READ_BYTES as u64) as usize;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let (text, truncated, dropped) = drain(&session.buffer, max_bytes);
            let closed = session.buffer.lock().closed;
            if !text.is_empty() || closed {
                let exit = session.exit_code();
                session.observe_pass(ctx, exit, closed);
                return Ok(ToolOutput::ok(render_read(
                    &session,
                    &text,
                    truncated,
                    dropped,
                    exit,
                    closed,
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                _ = session.buffer.notify.notified() => {}
                _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
                _ = tokio::time::sleep(remaining) => break,
            }
        }
        let exit = session.exit_code();
        let closed = session.buffer.lock().closed;
        session.observe_pass(ctx, exit, closed);
        Ok(ToolOutput::ok(render_read(
            &session, "", false, 0, exit, closed,
        )))
    }

    fn close(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let session_id = required_str(args, "session_id")?;
        let registry = self.sessions();
        let session = registry
            .remove(session_id, ctx.session_id.as_deref())
            .map_err(ToolError::Execution)?;
        let exit = session.exit_code();
        session.close();
        Ok(ToolOutput::ok(format!(
            "终端会话 {session_id} 已关闭{}（整棵进程树已回收）。",
            match exit {
                Some(code) => format!("，退出码 {code}"),
                None => "，已终止仍在运行的进程".to_string(),
            }
        )))
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidArgs(format!("缺少 {key}")))
}

/// 取走游标之后的新输出（最多 `max_bytes`，不切断 UTF-8 字符）。
fn drain(buffer: &OutputBuffer, max_bytes: usize) -> (String, bool, usize) {
    let mut state = buffer.lock();
    let start = state.cursor.min(state.text.len());
    let rest = &state.text[start..];
    let mut end = rest.len().min(max_bytes);
    while end < rest.len() && !rest.is_char_boundary(end) {
        end += 1;
    }
    let text = rest[..end].to_string();
    state.cursor = start + end;
    let truncated = state.cursor < state.text.len();
    let dropped = state.dropped.saturating_sub(state.dropped_reported);
    state.dropped_reported = state.dropped;
    (text, truncated, dropped)
}

fn render_read(
    session: &Session,
    text: &str,
    truncated: bool,
    dropped: usize,
    exit: Option<u32>,
    closed: bool,
) -> String {
    let status = match (exit, closed) {
        (Some(code), _) => format!("已退出 · exit={code}"),
        (None, true) => "运行中 · 输出流已关闭".to_string(),
        (None, false) => "运行中".to_string(),
    };
    let mut out = format!("[terminal {} · {status}]\n", session.id);
    if dropped > 0 {
        out.push_str(&format!("[已丢弃 {dropped} 字节更早的输出]\n"));
    }
    if text.is_empty() {
        out.push_str("（等待超时，暂无新输出）\n");
    } else {
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    if truncated {
        out.push_str("[还有未读输出，继续 read 取回]\n");
    }
    out
}

fn shell_usable_display(path: &Path) -> String {
    path.display().to_string()
}

#[async_trait]
impl ToolExecutor for Terminal {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        match required_str(&args, "action")? {
            "open" => self.open(&args, ctx),
            "write" => self.write(&args, ctx).await,
            "read" => self.read(&args, ctx).await,
            "close" => self.close(&args, ctx),
            other => Err(ToolError::InvalidArgs(format!(
                "未知 action: {other}（可选 open/write/read/close）"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxMode;
    use crate::test_support::TestTempDir;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn workspace(tag: &str) -> TestTempDir {
        TestTempDir::new(&format!("term-{tag}"))
    }

    fn context(ws: &Path, session: Option<&str>, sandbox: SandboxMode) -> ToolContext {
        let mut ctx = ToolContext::new(
            ws.to_path_buf(),
            CancellationToken::new(),
            format!("call-{}", ulid::Ulid::generate()),
            Arc::new(crate::events::NullEventSink),
            sandbox,
        );
        ctx.session_id = session.map(str::to_string);
        ctx
    }

    fn extract_id(text: &str) -> String {
        let start = text.find("term-").expect("输出里应带会话 id");
        text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect()
    }

    async fn read_until(terminal: &Terminal, ctx: &ToolContext, id: &str, needle: &str) -> String {
        let mut seen = String::new();
        for _ in 0..12 {
            let out = terminal
                .execute(json!({"action":"read","session_id":id,"timeout_ms":2000}), ctx)
                .await
                .unwrap();
            seen.push_str(&out.output);
            if seen.contains(needle) {
                return seen;
            }
        }
        panic!("等待输出超时，未出现 {needle:?}；已收到：\n{seen}");
    }

    /// 数一下命令行里带 marker 的进程（用系统 WMI，不依赖被测代码自证）。
    /// 必须排除 powershell 自身：查询脚本的命令行里就含 marker。
    fn marker_processes(marker: &str) -> u32 {
        let script = format!(
            "(Get-CimInstance Win32_Process -Filter \"CommandLine like '%{marker}%'\" \
| Where-Object {{ $_.Name -ne 'powershell.exe' }} | Measure-Object).Count"
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("powershell 可启动");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    #[test]
    fn newline_becomes_enter() {
        assert_eq!(normalize_input("echo hi\n"), "echo hi\r");
        assert_eq!(normalize_input("a\r\nb\nc"), "a\rb\rc");
    }

    #[test]
    fn utf8_split_across_reads_is_reassembled() {
        let mut tail = Utf8Tail::default();
        let bytes = "中文".as_bytes();
        // 故意把「中」的 3 字节切成 1 + 2，模拟管道边界。
        assert_eq!(tail.push(&bytes[..1]), "");
        assert_eq!(tail.push(&bytes[1..]), "中文");
    }

    #[tokio::test]
    async fn read_only_policy_denies_open() {
        let ws = workspace("readonly");
        let terminal = Terminal::new();
        let ctx = context(&ws, Some("sess-ro"), SandboxMode::ReadOnly);
        let before = sessions().len();
        let error = terminal
            .execute(json!({"action":"open"}), &ctx)
            .await
            .expect_err("只读沙箱必须拒绝");
        assert!(
            matches!(error, ToolError::SandboxDenied(_)),
            "应是沙箱拒绝，实际: {error}"
        );
        assert_eq!(sessions().len(), before, "被拒的 open 不得留下会话");
    }

    #[tokio::test]
    async fn echo_roundtrip_then_close() {
        let ws = workspace("echo");
        let terminal = Terminal::new();
        let ctx = context(&ws, Some("sess-echo"), SandboxMode::WorkspaceWrite);
        let id = extract_id(
            &terminal
                .execute(json!({"action":"open"}), &ctx)
                .await
                .expect("open 应成功")
                .output,
        );

        terminal
            .execute(
                json!({"action":"write","session_id":id,"data":"set /a 123*456\r"}),
                &ctx,
            )
            .await
            .expect("write 应成功");
        // 56088 只可能来自命令**执行结果**，命令原文里没有这个数字。
        read_until(&terminal, &ctx, &id, "56088").await;

        let closed = terminal
            .execute(json!({"action":"close","session_id":id}), &ctx)
            .await
            .expect("close 应成功");
        assert!(closed.output.contains("已关闭"), "close 输出: {}", closed.output);
        assert!(sessions().peek(&id).is_none(), "close 后会话必须移出登记表");
    }

    #[tokio::test]
    async fn other_session_cannot_touch_terminal() {
        let ws = workspace("owner");
        let terminal = Terminal::new();
        let owner = context(&ws, Some("sess-owner"), SandboxMode::WorkspaceWrite);
        let id = extract_id(
            &terminal
                .execute(json!({"action":"open"}), &owner)
                .await
                .expect("open 应成功")
                .output,
        );

        let other = context(&ws, Some("sess-other"), SandboxMode::WorkspaceWrite);
        let error = terminal
            .execute(json!({"action":"write","session_id":id,"data":"echo x\r"}), &other)
            .await
            .expect_err("跨会话必须被拒");
        assert!(
            format!("{error}").contains("不属于当前会话"),
            "错误应说明归属，实际: {error}"
        );

        terminal
            .execute(json!({"action":"close","session_id":id}), &owner)
            .await
            .expect("清理会话");
    }

    /// R106：close 必须回收**整棵**进程树，而不只是 shell 本身。
    #[tokio::test]
    async fn close_reclaims_whole_process_tree() {
        let ws = workspace("tree");
        let terminal = Terminal::new();
        let ctx = context(&ws, Some("sess-tree"), SandboxMode::WorkspaceWrite);
        let id = extract_id(
            &terminal
                .execute(json!({"action":"open"}), &ctx)
                .await
                .expect("open 应成功")
                .output,
        );

        let marker = format!("SHANIU_TERM_{}", ulid::Ulid::generate());
        // `start /b` 派生一个长跑子进程；命令行里带 marker，供外部观察。
        let command = format!(
            "start /b cmd /c for /l %i in (1,1,1000000000) do @rem {marker}\r"
        );
        terminal
            .execute(json!({"action":"write","session_id":id,"data":command}), &ctx)
            .await
            .expect("write 应成功");

        // 前提：子树真的起来了（否则 0 残留的断言毫无意义）。
        let mut saw_child = 0;
        for _ in 0..20 {
            saw_child = marker_processes(&marker);
            if saw_child > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        assert!(saw_child > 0, "未观察到子进程，测试前提不成立");

        terminal
            .execute(json!({"action":"close","session_id":id}), &ctx)
            .await
            .expect("close 应成功");

        let mut left = saw_child;
        for _ in 0..20 {
            left = marker_processes(&marker);
            if left == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        assert_eq!(left, 0, "关闭会话后仍有 {left} 个进程残留（整树未回收）");
    }
}
