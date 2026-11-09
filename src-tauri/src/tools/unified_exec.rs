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
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::events::EventMsg;
use crate::sandbox;
use crate::tool_router::{
    resolve_in_workspace, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolSpec,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_COLLECTED_BYTES: usize = 128 * 1024;
/// 单行原始字节上限（`read_until` 本身无界）。
///
/// 一条永远不换行的输出（`printf` 几百 MB、`yes` 被截断在半行）会把行缓冲撑到 OOM，
/// 而总量上限 `MAX_COLLECTED_BYTES` 是**收集之后**才生效，救不了读取阶段。
/// 超限按一行先发出去、继续读下一段：内容不丢、行序不变，只是超长行会被切成多条。
const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoChannel {
    Stdout,
    Stderr,
}

pub struct UnifiedExec {
    spec: ToolSpec,
    /// 已经提示过"改用 read_file"的会话（SH-126①）：同一会话只提示一次，
    /// 连续 `type` 不会把提示刷满上下文。
    hinted_read: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 已经提示过"改用 apply_patch"的会话（SH-126①写入侧），口径同 `hinted_read`。
    hinted_write: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// 用 shell 读文件的命令（SH-126①）。它们能跑通，但绕过了 `read_file` 的截断、
/// 行号与读状态登记——后续写同一文件时会被"未读先写"拦下，所以值得提示一次。
const SHELL_READERS: &[&str] = &["cat", "type", "get-content", "gc"];

/// 取被执行的**第一条命令**的程序名（去掉外层引号与 `.exe`）。
///
/// 允许 `cmd /c ...`、`powershell -Command ...` 这类外壳。读误用、写误用与
/// 卷信息探测共用这一个口径，否则同一句命令在三个探测里会得到三种答案。
fn first_program<'a>(words: &'a [&'a str]) -> Option<&'a str> {
    let mut index = 0;
    if matches!(
        words.first().copied(),
        Some("cmd" | "cmd.exe" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe")
    ) {
        index = words
            .iter()
            .position(|word| matches!(*word, "/c" | "/k" | "-c" | "-command"))
            .map(|at| at + 1)
            .unwrap_or(0);
    }
    words.get(index).map(|word| word.trim_matches('"').trim_end_matches(".exe"))
}

/// 认出"用 shell 读文件"的形态，返回命中的程序名（None = 不是这类命令）。
///
/// 只看被执行的**第一条命令**的程序名：`cmd /c type x`、`powershell -c Get-Content x`
/// 都算；参数里顺带出现 `cat` 不算（否则 `cargo run -- cat` 也会被误提示）。
fn shell_reader_program(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let program = first_program(&words)?;
    SHELL_READERS.iter().copied().find(|known| *known == program)
}

/// 认出"用 shell 原地改文件"的形态（SH-126①写入侧），返回命中的写法。
///
/// 只覆盖显式的原地编辑：`sed -i`、`perl -i/-pi`、`Set-Content`/`Add-Content`/`Out-File`。
/// **不把 `>` 重定向算进来**——`cargo test > out.log` 是正常用法，误提示会盖过收益。
fn shell_writer_program(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let program = first_program(&words)?;
    let in_place = words
        .iter()
        .any(|word| word.starts_with("-i") || *word == "-pi" || word.starts_with("--in-place"));
    match program {
        "sed" if in_place => Some("sed -i"),
        "perl" if in_place => Some("perl -i"),
        "set-content" => Some("Set-Content"),
        "add-content" => Some("Add-Content"),
        "out-file" => Some("Out-File"),
        _ => None,
    }
}

/// 依赖卷信息、在 AppContainer 下会静默退出的 cmd 命令（SH-021 实测：退出码 1、零输出）。
const VOLUME_DEPENDENT: &[&str] = &["dir", "vol", "tree"];

/// 认出「依赖卷信息」的命令程序名（None = 不是这类命令）。
///
/// 与 `shell_reader_program` 同一解析口径：只看被执行的**第一条命令**，
/// 允许 `cmd /c ...` / `cmd /k ...` 外壳。
fn volume_dependent_program(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let program = first_program(&words)?;
    VOLUME_DEPENDENT.iter().copied().find(|known| *known == program)
}

/// 一次「纯 `dir` 列举」请求。
#[derive(Debug, PartialEq, Eq)]
struct DirListing {
    target: String,
    /// `/b`：只输出名字，一行一个。
    bare: bool,
    /// `/a`：包含隐藏/系统项（cmd 默认不显示）。
    show_all: bool,
}

const DIR_FALLBACK_LIMIT: usize = 500;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

/// 按引号切词；引号不配对返回 None（无法安全判定形态）。
fn split_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if quoted {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

/// 识别可以安全等价替代的 `dir` 形态（SH-021 降级处理）。
///
/// 只接单条、无管道/重定向/串接的 `dir`：`dir`、`dir /b`、`dir /a sub`、`dir "a b"`。
/// `/s` 递归、多目标、其他开关一律返回 None（交回真实 shell；失败时仍有空输出提示兜底），
/// 避免自造一套与 cmd 不一致的语义。
fn dir_listing(command: &str) -> Option<DirListing> {
    let trimmed = command.trim();
    if trimmed.contains(['&', '|', '>', '<', '^']) {
        return None;
    }
    let mut words = split_words(trimmed)?;
    if words.is_empty() {
        return None;
    }
    if words[0].eq_ignore_ascii_case("cmd") || words[0].eq_ignore_ascii_case("cmd.exe") {
        if words.len() < 3 || !matches!(words[1].to_ascii_lowercase().as_str(), "/c" | "/k") {
            return None;
        }
        words.drain(0..2);
    }
    let program = words[0].trim_end_matches(".exe");
    if !program.eq_ignore_ascii_case("dir") {
        return None;
    }
    let mut bare = false;
    let mut show_all = false;
    let mut target: Option<String> = None;
    for word in &words[1..] {
        if word.starts_with('/') || word.starts_with('-') {
            let switch = word[1..].to_ascii_lowercase();
            // 带限定符的开关（/a:d、/o:s…）语义不同，别假装等价。
            if switch.contains(':') {
                return None;
            }
            match switch.as_str() {
                "a" => show_all = true,
                "b" => bare = true,
                "w" | "q" | "x" | "4" | "n" | "l" | "d" | "o" | "t" => {}
                _ => return None,
            }
            continue;
        }
        // `dir a b` 是多目标列举，语义不同，不替代。
        if target.is_some() {
            return None;
        }
        target = Some(word.clone());
    }
    Some(DirListing {
        target: target.unwrap_or_else(|| ".".to_string()),
        bare,
        show_all,
    })
}

fn is_hidden(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

/// 宿主侧等价列举（只读；作用域仍由 `resolve_in_workspace` 限制在工作区内）。
///
/// 输出**明确标注是替代实现**，不假装是 cmd 的原样输出；空目录也给计数行，
/// 保证模型不会拿到一片空白。
fn render_dir_listing(full: &Path, request: &DirListing) -> ToolOutput {
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    if full.is_file() {
        let name = full
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| full.display().to_string());
        let size = std::fs::metadata(full).map(|meta| meta.len()).unwrap_or(0);
        entries.push((name, false, size));
    } else {
        let read = match std::fs::read_dir(full) {
            Ok(read) => read,
            Err(error) => {
                return ToolOutput::fail(format!("列举目录失败: {}: {error}", full.display()));
            }
        };
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    return ToolOutput::fail(format!(
                        "列举目录失败: {}: {error}",
                        full.display()
                    ));
                }
            };
            let file_type = entry.file_type().ok();
            let meta = entry.metadata().ok();
            if !request.show_all {
                if let Some(meta) = meta.as_ref() {
                    if is_hidden(meta) {
                        continue;
                    }
                }
            }
            let is_dir = file_type.map(|kind| kind.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|meta| meta.len()).unwrap_or(0);
            entries.push((entry.file_name().to_string_lossy().to_string(), is_dir, size));
        }
    }
    entries.sort_by_key(|(name, _, _)| name.to_ascii_lowercase());

    let dirs = entries.iter().filter(|(_, is_dir, _)| *is_dir).count();
    let files = entries.len() - dirs;
    let total: u64 = entries.iter().map(|(_, _, size)| *size).sum();
    let shown = entries.len().min(DIR_FALLBACK_LIMIT);

    let mut text = String::new();
    if request.bare {
        for (name, _, _) in entries.iter().take(shown) {
            text.push_str(name);
            text.push('\n');
        }
    } else {
        text.push_str(&format!(
            "{} 的目录（shaniu 等价列举：容器内 cmd 的 dir 依赖卷信息，AppContainer 下不可用）\n",
            full.display()
        ));
        for (name, is_dir, size) in entries.iter().take(shown) {
            if *is_dir {
                text.push_str(&format!("<DIR>  {name}\n"));
            } else {
                text.push_str(&format!("{size:>10} B  {name}\n"));
            }
        }
    }
    text.push_str(&format!(
        "[共 {dirs} 个目录，{files} 个文件，{total} 字节{}]",
        if entries.len() > shown {
            format!("；其余 {} 项未显示", entries.len() - shown)
        } else {
            String::new()
        }
    ));
    ToolOutput::ok(text)
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
            hinted_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            hinted_write: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// SH-126①：读文件走 shell 时，在结果尾部追加一条改用 `read_file` 的提示。
    /// **同一会话只提示一次**（`cat`/`type`/`Get-Content` 视为同一类误用，不各提示一遍）；
    /// 只改文本、不改 `success`，因此不计入工具失败率。
    fn read_file_hint(&self, ctx: &ToolContext, command: &str) -> Option<String> {
        let program = shell_reader_program(command)?;
        let key = ctx
            .session_id
            .clone()
            .unwrap_or_else(|| "__sessionless__".to_string());
        let mut hinted = self.hinted_read.lock().expect("hinted_read 锁中毒");
        if !hinted.insert(key) {
            return None;
        }
        Some(format!(
            "\n[提示] `{program}` 读文件绕过了 read_file 的截断、行号与读状态登记，\
             后续写同一文件可能被判为「未读先写」；改用 read_file 更稳（本会话只提示这一次）。"
        ))
    }

    /// SH-126①（写入侧）：用 `sed -i` / `perl -pi` / `Set-Content` 改文件时，
    /// 在结果尾部追加一条改用编辑工具的提示。口径同读侧：**同一会话只提示一次**，
    /// 只改文本、不改 `success`，因此不计入工具失败率。
    fn write_file_hint(&self, ctx: &ToolContext, command: &str) -> Option<String> {
        let program = shell_writer_program(command)?;
        let key = ctx
            .session_id
            .clone()
            .unwrap_or_else(|| "__sessionless__".to_string());
        let mut hinted = self.hinted_write.lock().expect("hinted_write 锁中毒");
        if !hinted.insert(key) {
            return None;
        }
        Some(format!(
            "\n[提示] `{program}` 直接改文件绕过了 apply_patch 的 diff 展示与读状态登记：\
             改错无法按补丁撤回，之后再用 apply_patch 改同一文件容易被判「未读先写」。\
             改用 apply_patch 更稳（本会话只提示这一次）。"
        ))
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
///
/// 按**原始字节**读行再解码：`read_line(&mut String)` 遇到中文 Windows 的 GBK 输出
/// 会返回 `InvalidData` 并让整行连同其后内容一起丢失（模型只看到 `�\u{73e}...`）。
/// 编码由 [`crate::text_encoding::StreamDecoder`] 按整条流判定一次，结果回写进
/// `encodings`，供调用方写进工具输出元数据。
fn spawn_reader(
    pipe: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    channel: u8,
    tx: mpsc::Sender<sandbox::ExecLine>,
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

        // SH-021 降级处理：AppContainer 内 cmd 的 `dir` 依赖卷信息，会静默退出（退出码 1、零输出）。
        // `dir` 是模型最常用的列举方式，静默空结果会被误判成"沙箱坏了"再绕路，
        // 所以在容器模式下把纯 `dir` 列举翻成宿主侧只读列举（仍受工作区根约束）。
        if !run_in_background
            && ctx.sandbox == crate::config::SandboxMode::WorkspaceWrite
            && sandbox::current_os_mode() == crate::config::OsSandbox::AppContainer
        {
            if let Some(request) = dir_listing(command) {
                if let Ok(full) = resolve_in_workspace(&ctx.workspace_root, &request.target) {
                    return Ok(render_dir_listing(&full, &request));
                }
                // 目标在工作区外：交回真实 shell，交给沙箱策略裁决（不在这里替它放行）。
            }
        }

        // stdio 行通道（Plain=tokio 读任务 / Restricted=std 读线程，统一回传）
        let (tx, mut rx) = mpsc::channel::<sandbox::ExecLine>(256);
        // 输出编码判定槽：两条流各自回写最终采用的编码，收尾时写进工具输出元数据。
        let encodings = crate::text_encoding::EncodingLedger::new();

        // 按沙箱策略启动（fail-closed：沙箱准备失败 → Execution，执行数为 0）
        // 网络不是策略维度（2026-11-10）：容器一律带 internetClient，不再按会话存授权。
        let mut waiter = match sandbox::prepare_spawn_with(
            sandbox::SpawnRequest {
                policy: ctx.sandbox,
                workspace_root: &ctx.workspace_root,
                program: "cmd",
                args: &["/C", command],
                cwd: &cwd,
                os: sandbox::current_os_mode(),
            },
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
                    spawn_reader(Box::new(pipe), 0, tx.clone(), encodings.clone());
                }
                if let Some(pipe) = child.stderr.take() {
                    spawn_reader(Box::new(pipe), 1, tx.clone(), encodings.clone());
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
                    // 输出编码元数据：只在回退到 OEM 代码页（中文 Windows = 936）时写。
                    // UTF-8/ASCII 是常态，不占位、不加噪；回退则必须明示，不许无痕猜测。
                    let encoding_note = match &waiter {
                        Waiter::Tokio(_) => encodings.note(),
                        Waiter::Restricted(child) => child.encoding_note(),
                    };
                    if let Some(note) = encoding_note {
                        text.push_str(&format!("\n{note}"));
                    }
                    // SH-021 降级处理：卷信息类命令在 AppContainer 里会失败（典型报错
                    // 「拒绝访问。」）——这是容器拿不到卷信息，不是目录 ACL 故障，也不是
                    // 沙箱损坏。中文报错现在能正常解码（见 text_encoding），所以触发条件
                    // 从「零输出」改为「失败 + 容器模式」：既覆盖旧的静默形态，也避免模型
                    // 把 `拒绝访问。` 误读成授权配置错了。
                    if code != 0
                        && sandbox::current_os_mode() == crate::config::OsSandbox::AppContainer
                    {
                        if let Some(program) = volume_dependent_program(command) {
                            text.push_str(&format!(
                                "\n[提示] `{program}` 在 AppContainer 内会因拿不到卷信息而失败（典型报错「拒绝访问。」）——\
                                 这不是沙箱损坏，也不是目录 ACL 故障。列目录请改用 list_dir 工具，或在容器内用 `for %f in (*) do @echo %f`。"
                            ));
                        }
                    }
                    // SH-126①：`type`/`cat`/`Get-Content` 这类读文件形态，提示一次改用 read_file。
                    if let Some(hint) = self.read_file_hint(ctx, command) {
                        text.push_str(&hint);
                    }
                    // SH-126①（写入侧）：`sed -i`/`perl -pi`/`Set-Content` 绕过 apply_patch。
                    if let Some(hint) = self.write_file_hint(ctx, command) {
                        text.push_str(&hint);
                    }
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
    use tokio::io::AsyncWriteExt;
    
    fn ctx(ws: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn crate::events::EventSink>> = std::sync::OnceLock::new();
        let sink = SINK
            .get_or_init(|| Arc::new(crate::events::NullEventSink) as Arc<dyn crate::events::EventSink>)
            .clone();
        ToolContext::new(ws, tokio_util::sync::CancellationToken::new(), "call-exec", sink, crate::config::SandboxMode::WorkspaceWrite)
    }

    fn workspace() -> crate::test_support::TestTempDir {
        // 自删临时目录：panic 也会清（2026-11-09 夹具规矩）。
        crate::test_support::TestTempDir::new("exec")
    }

    #[tokio::test]
    async fn overlong_line_is_split_instead_of_unbounded_buffering() {
        let (mut client, server) = tokio::io::duplex(1024);
        let (tx, mut rx) = mpsc::channel::<sandbox::ExecLine>(16);
        let reader = spawn_reader(
            Box::new(server),
            0,
            tx,
            crate::text_encoding::EncodingLedger::new(),
        );
        let total = MAX_LINE_BYTES * 3 + 7;
        let payload = vec![b'a'; total];
        let writer = tokio::spawn(async move {
            AsyncWriteExt::write_all(&mut client, &payload).await.unwrap();
            AsyncWriteExt::shutdown(&mut client).await.unwrap();
        });

        let mut received = 0usize;
        let mut lines = 0usize;
        while let Some((_channel, line)) = rx.recv().await {
            lines += 1;
            assert!(line.len() <= MAX_LINE_BYTES, "单条输出不得超过单行上限");
            received += line.len();
        }
        writer.await.unwrap();
        reader.await.unwrap();
        assert_eq!(received, total, "超长行被切成多条，但字节一个不能丢");
        assert!(lines >= 3, "超过上限的行必须分段送出而不是缓冲到 OOM，实际 {lines} 段");
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
    }

    /// 真机用例（仅中文 Windows 生效）：cmd 的报错按 OEM 代码页（936/GBK）写管道，
    /// 旧实现会 `InvalidData` 丢整行。这里要求模型侧拿到**可读中文** + 编码元数据，
    /// 且不得出现替换字符。
    #[tokio::test]
    async fn gbk_error_message_is_readable_on_chinese_windows() {
        if crate::text_encoding::system_oem_codepage() != 936 {
            return;
        }
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": "type __shaniu_missing_file_9f3c__.txt"}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(!out.success, "输出: {}", out.output);
        assert!(!out.output.contains('\u{FFFD}'), "输出: {}", out.output);
        assert!(
            out.output
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "应拿到可读中文，实际: {}",
            out.output
        );
        assert!(
            out.output.contains("oem-936"),
            "缺少编码元数据: {}",
            out.output
        );
    }

    /// 不碰网络、不碰文件系统的「长命令」：超时 / 取消 / 后台终止用例的样本。
    /// 之所以不用 `ping` 之类：这类外部命令的可用性依赖网络与 PATH，样本一早就退出时，
    /// 超时与取消用例会变成"进程早退了"的假绿/假红。纯 shell 循环不受环境影响。
    const SLOW_COMMAND: &str = "for /L %i in (1,1,400000000) do @rem";

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": SLOW_COMMAND, "timeout_ms": 300}),
                &ctx(&ws),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("超时"), "超时用例拿到的是：{}", out.output);
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
            async move { executor.execute(serde_json::json!({"command": SLOW_COMMAND}), &ctx).await }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        context.cancel.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
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
    }

    #[tokio::test]
    async fn missing_command_is_invalid_args() {
        let ws = workspace();
        let err = UnifiedExec::new()
            .execute(serde_json::json!({}), &ctx(&ws))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
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
    }

    #[tokio::test]
    async fn background_job_can_be_killed() {
        let ws = workspace();
        let registry = std::sync::Arc::new(crate::jobs::JobRegistry::new());
        let context = ctx(&ws).with_jobs(Some("s1".into()), Some(registry.clone()));
        let out = UnifiedExec::new()
            .execute(
                serde_json::json!({"command": SLOW_COMMAND, "run_in_background": true}),
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
    }

    #[test]
    fn dir_listing_parser_accepts_only_simple_forms() {
        assert_eq!(
            dir_listing("dir"),
            Some(DirListing { target: ".".into(), bare: false, show_all: false })
        );
        assert_eq!(
            dir_listing("dir /b"),
            Some(DirListing { target: ".".into(), bare: true, show_all: false })
        );
        assert_eq!(
            dir_listing("DIR /A \"my dir\""),
            Some(DirListing { target: "my dir".into(), bare: false, show_all: true })
        );
        assert_eq!(
            dir_listing("cmd /c dir src /w"),
            Some(DirListing { target: "src".into(), bare: false, show_all: false })
        );
        // 语义不同或无法安全等价的一律交回真实 shell（失败时还有空输出提示兜底）
        assert_eq!(dir_listing("dir /s"), None);
        assert_eq!(dir_listing("dir /a:d"), None);
        assert_eq!(dir_listing("dir | findstr x"), None);
        assert_eq!(dir_listing("dir a b"), None);
        assert_eq!(dir_listing("echo dir"), None);
        assert_eq!(dir_listing("dir \"unbalanced"), None);
    }

    #[test]
    fn volume_dependent_commands_are_recognized() {
        assert_eq!(volume_dependent_program("dir"), Some("dir"));
        assert_eq!(volume_dependent_program("cmd /C vol"), Some("vol"));
        assert_eq!(volume_dependent_program("TREE.EXE"), Some("tree"));
        assert_eq!(volume_dependent_program("echo dir"), None);
        assert_eq!(volume_dependent_program("cargo run -- dir"), None);
    }

    /// 回归锁（SH-021 降级）：容器内 `dir` 不能静默返回空——必须有真实列举。
    #[tokio::test]
    async fn dir_lists_real_entries_instead_of_silent_empty() {
        let ws = workspace();
        std::fs::write(ws.join("alpha.txt"), "12345").unwrap();
        std::fs::create_dir_all(ws.join("nested")).unwrap();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "dir"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.success, "dir 替代列举应成功：{}", out.output);
        assert!(out.output.contains("alpha.txt"), "{}", out.output);
        assert!(out.output.contains("nested"), "{}", out.output);
        assert!(out.output.contains("等价列举"), "应明确标注是替代实现：{}", out.output);
        assert!(out.output.contains("1 个目录，1 个文件"), "应给出计数：{}", out.output);
    }

    #[tokio::test]
    async fn dir_on_empty_dir_reports_counts_not_nothing() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "dir"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.success, "{}", out.output);
        assert!(!out.output.trim().is_empty(), "空目录也不许返回零字节");
        assert!(out.output.contains("0 个目录，0 个文件"), "{}", out.output);
    }

    #[tokio::test]
    async fn dir_bare_lists_names_without_detail_columns() {
        let ws = workspace();
        std::fs::write(ws.join("alpha.txt"), "x").unwrap();
        std::fs::create_dir_all(ws.join("nested")).unwrap();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "dir /b"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("alpha.txt"), "{}", out.output);
        assert!(out.output.contains("nested"), "{}", out.output);
        assert!(!out.output.contains("<DIR>"), "/b 不应输出明细列：{}", out.output);
    }

    /// 未被替代的卷信息形态（如 `/s`）也不许静默：要么真实输出，要么失败并解释原因。
    #[tokio::test]
    async fn unintercepted_dir_never_fails_silently() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "dir /s"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(
            !out.success && out.output.contains("这不是沙箱损坏"),
            "dir /s 不允许失败且零解释：{:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn shell_read_hint_appears_once_per_session() {
        let ws = workspace();
        std::fs::write(ws.join("note.txt"), "hello-note").unwrap();
        let exec = UnifiedExec::new();
        let first = exec
            .execute(serde_json::json!({"command": "type note.txt"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(first.success, "提示不阻断执行: {}", first.output);
        assert!(first.output.contains("hello-note"));
        assert!(first.output.contains("read_file"), "应提示改用 read_file: {}", first.output);
        // 同会话第二次（换个别名也算同一类误用）：不再提示，避免刷屏
        let second = exec
            .execute(serde_json::json!({"command": "type note.txt"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(!second.output.contains("[提示]"), "同会话只提示一次: {}", second.output);
    }

    #[tokio::test]
    async fn shell_read_hint_ignores_unrelated_commands() {
        let ws = workspace();
        let out = UnifiedExec::new()
            .execute(serde_json::json!({"command": "echo hi"}), &ctx(&ws))
            .await
            .unwrap();
        assert!(!out.output.contains("[提示]"));
        // 只在参数里出现 `cat` 不算误用
        assert_eq!(shell_reader_program("cargo run -- cat x"), None);
        assert_eq!(shell_reader_program("cmd /c type x"), Some("type"));
        assert_eq!(shell_reader_program("powershell -Command Get-Content x"), Some("get-content"));
        assert_eq!(shell_reader_program("TYPE.EXE x"), Some("type"));
    }

    /// SH-126①（写入侧）：只认显式原地编辑，别把正常重定向也当成误用。
    #[test]
    fn shell_writer_program_recognizes_in_place_editors_only() {
        assert_eq!(shell_writer_program("sed -i s/a/b/ src/x.rs"), Some("sed -i"));
        assert_eq!(shell_writer_program("sed --in-place s/a/b/ src/x.rs"), Some("sed -i"));
        assert_eq!(shell_writer_program("perl -pi -e 's/a/b/' src/x.rs"), Some("perl -i"));
        assert_eq!(
            shell_writer_program("powershell -Command Set-Content -Path a.txt -Value b"),
            Some("Set-Content")
        );
        assert_eq!(shell_writer_program("pwsh -c Add-Content a.txt b"), Some("Add-Content"));
        // 只读 / 只打印，不是写入：不提示
        assert_eq!(shell_writer_program("sed -n 1,10p src/x.rs"), None);
        assert_eq!(shell_writer_program("perl -e 'print 1'"), None);
        // 重定向是正常用法（跑测试存日志），刻意不提示
        assert_eq!(shell_writer_program("cargo test > out.log"), None);
        assert_eq!(shell_writer_program("echo hi"), None);
    }

    /// 提示只加文本、不改 success，且同一会话只出现一次。
    #[test]
    fn shell_write_hint_fires_once_per_session_and_keeps_success() {
        let ws = workspace();
        let exec = UnifiedExec::new();
        let first = exec
            .write_file_hint(&ctx(&ws), "sed -i s/a/b/ src/x.rs")
            .expect("sed -i 应提示改用 apply_patch");
        assert!(first.contains("apply_patch"), "{first}");
        assert!(
            exec.write_file_hint(&ctx(&ws), "perl -pi -e 's/a/b/' src/y.rs").is_none(),
            "同会话第二次不再提示"
        );
        // 读误用与写误用是两类，互不吞掉对方的提示额度
        assert!(exec.read_file_hint(&ctx(&ws), "type note.txt").is_some());
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
    }
}
