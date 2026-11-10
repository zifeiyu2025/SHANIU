//! 用户终端（SH-071）：右侧栏里**给人用**的独立终端。
//!
//! 边界（三条，写死在本模块，不靠调用方自觉）：
//!
//! 1. **不进沙箱**：裸进程 + ConPTY，不建受限令牌、不挂 AppContainer、不经审批。
//!    沙箱约束的是不可信执行者（模型），不是用户本人；把用户自己的 shell 关进
//!    容器只会让自敲命令被无名拒绝，安全收益为零、摩擦全是实打实的。
//! 2. **不进模型上下文**：输出只经本模块自己的 Tauri 事件通道
//!    （[`FRAME_EVENT`]）发给 WebView，不产生任何 `EventMsg`、不写会话日志、
//!    不进采样请求。结构上就没有通往模型的数据路径——不是靠调用方过滤。
//! 3. **不走 `unified_exec`**：独立进程、独立 Job，与 agent 的执行通道无关，
//!    也不出现在 agent 的工具调用记录里。
//!
//! 唯一的约束是 Job 的 `KILL_ON_JOB_CLOSE`：它不限制用户能跑什么，只保证应用
//! 退出/崩溃时整棵子进程树被内核回收，不留孤儿。

mod pty;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// 用户终端帧事件通道；与 `EventMsg` / 会话事件**不是**同一条通道。
pub const FRAME_EVENT: &str = "user-terminal://frame";

/// 关窗口（隐藏到托盘）时的一次性提示通道；只在仍有终端存活时发一次。
pub const TRAY_HINT_EVENT: &str = "user-terminal://tray-hint";

/// 托盘提示载荷。`count` 是当前存活的终端会话数；`first` 表示这是本进程内第一次
/// 点关闭，前端据此补一句“图标在托盘区哪里”。
#[derive(Debug, Clone, Serialize)]
pub struct TrayHint {
    pub count: usize,
    #[serde(default)]
    pub first: bool,
}

/// 单次写入上限：防止前端一次塞进巨量数据占住 IPC。
const MAX_INPUT_BYTES: usize = 1 << 20;
const MAX_ID_LEN: usize = 64;

/// 可选的 shell（`pwsh.exe` 不存在时不会出现在列表里）。
#[derive(Debug, Clone, Serialize)]
pub struct ShellOption {
    pub path: String,
    pub name: String,
    pub args: Vec<String>,
}

/// 终端对前端可见的状态快照。
#[derive(Debug, Clone, Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub title: String,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// `running` / `exited`。
    pub state: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// 本模块唯一的出口事件；`type` 区分输出与退出。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrameEvent {
    Output { id: String, data: String },
    Exit { id: String, code: i32 },
}

/// 事件出口抽象：生产环境写 Tauri 通道，测试用记录器钉住「只有这一条出口」。
pub trait FrameSink: Send + Sync {
    fn send(&self, event: FrameEvent);
}

struct TauriFrameSink(AppHandle);

impl FrameSink for TauriFrameSink {
    fn send(&self, event: FrameEvent) {
        let _ = self.0.emit(FRAME_EVENT, event);
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    /// 前端生成的稳定 id（同一标签重连沿用）。
    pub id: String,
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub title: Option<String>,
}

struct Session {
    child: pty::Child,
    writer: Mutex<Option<std::fs::File>>,
    closed: AtomicBool,
}

impl Session {
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // 先关输入端再杀进程：ConPTY 的写端不关，`ClosePseudoConsole` 可能等 reader。
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(writer);
        self.child.close();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// 终端注册表：只放**进程还活着**的会话。
///
/// 进程退出（读线程读到 EOF）时由读线程自己把条目摘掉：否则「存活计数」会把
/// 已退出的终端算成在跑（托盘提示说错话），而且同一 id 的重连会被
/// `终端 id 已存在` 挡住——标签切走再切回时 `refresh()` 用的是同一个 id。
/// 应用退出时随 Tauri 托管状态一起释放。
#[derive(Default)]
pub struct UserTerminalManager {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
}

impl UserTerminalManager {
    /// 启动一个终端；`sink` 接收输出与退出事件。
    ///
    /// `cwd` 由调用方（组合根的命令壳）解析好再传进来：起始目录要过工作区唯一判据，
    /// 而判据住在 `config`/`app_state` 层——本模块按 E57 保持零 crate 内依赖
    /// （与 `session_export` 同处理：越界判定放在组合根，模块只做终端本身）。
    fn open(
        &self,
        request: OpenRequest,
        sink: Arc<dyn FrameSink>,
        cwd: PathBuf,
    ) -> Result<TerminalInfo, String> {
        validate_id(&request.id)?;
        let shells = shells();
        let shell = request
            .shell
            .as_deref()
            .and_then(|path| shells.iter().find(|option| option.path.eq_ignore_ascii_case(path)))
            .unwrap_or_else(|| &shells[0]);
        let cols = request.cols.unwrap_or(80).clamp(20, 500);
        let rows = request.rows.unwrap_or(24).clamp(5, 500);
        let title = request
            .title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| shell.name.clone());

        {
            let guard = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.contains_key(&request.id) {
                return Err(format!("终端 id 已存在: {}", request.id));
            }
        }

        let spawn = pty::spawn(&shell.path, &shell.args, &cwd, cols, rows)?;
        let exit_probe = spawn.child.clone_process().ok();
        let watcher = spawn.child.clone_process().ok();
        let info = TerminalInfo {
            id: request.id.clone(),
            title,
            shell: shell.path.clone(),
            cwd: cwd.display().to_string(),
            cols,
            rows,
            state: String::from("running"),
            exit_code: None,
            error: None,
        };
        let session = Arc::new(Session {
            child: spawn.child,
            writer: Mutex::new(Some(spawn.writer)),
            closed: AtomicBool::new(false),
        });
        // 先登记再起读线程：进程秒退时读线程会立刻摘条目，顺序反了会留下幽灵会话。
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request.id.clone(), session);
        if let Err(error) = start_reader(
            spawn.reader,
            exit_probe,
            request.id.clone(),
            Arc::clone(&self.inner),
            sink,
        ) {
            self.inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&request.id);
            return Err(error);
        }
        if let Some(process) = watcher {
            watch_exit(process, request.id.clone(), Arc::clone(&self.inner));
        }
        Ok(info)
    }

    fn write(&self, id: &str, data: &str) -> Result<(), String> {
        if data.len() > MAX_INPUT_BYTES {
            return Err(format!("单次输入超过 {MAX_INPUT_BYTES} 字节"));
        }
        let session = self.session(id)?;
        let mut guard = session
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let writer = guard.as_mut().ok_or_else(|| String::from("终端已关闭"))?;
        writer
            .write_all(data.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| format!("写入终端失败: {error}"))
    }

    fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let session = self.session(id)?;
        session.child.resize(cols.clamp(20, 500), rows.clamp(5, 500))
    }

    fn close(&self, id: &str) {
        let session = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        if let Some(session) = session {
            session.close();
        }
    }

    fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions {
            session.close();
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, String> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| format!("终端不存在: {id}"))
    }

    /// 存活会话数。唯一用途是给"隐藏到托盘时仍有终端在跑"提供状态，
    /// 不推断控制权、不代表任何接管语义。表里只留活着的进程（见类型注释），
    /// 所以这个数就是"真在跑的终端数"，用户已 `exit` 的终端不算。
    pub(crate) fn active_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Drop for UserTerminalManager {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn start_reader(
    mut reader: std::fs::File,
    exit_probe: Option<std::os::windows::io::OwnedHandle>,
    id: String,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    sink: Arc<dyn FrameSink>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name(format!("user-terminal-{id}"))
        .spawn(move || {
            let mut decoder = Utf8Stream::default();
            let mut buffer = vec![0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let text = decoder.push(&buffer[..count]);
                        if !text.is_empty() {
                            sink.send(FrameEvent::Output {
                                id: id.clone(),
                                data: text,
                            });
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let tail = decoder.finish();
            if !tail.is_empty() {
                sink.send(FrameEvent::Output {
                    id: id.clone(),
                    data: tail,
                });
            }
            let code = exit_probe
                .as_ref()
                .and_then(pty::exit_code_of)
                .map(|value| value as i32)
                .unwrap_or(1);
            sink.send(FrameEvent::Exit {
                id: id.clone(),
                code,
            });
            // 进程已经没了：立刻摘掉注册表条目。留在这里会让存活计数虚高，
            // 也会挡住同 id 的重连（`open` 对已存在 id 直接拒绝）。
            retire(&sessions, &id);
        })
        .map(|_| ())
        .map_err(|error| format!("启动终端读线程失败: {error}"))
}

/// 盯进程句柄，退出即摘条目并收起伪控制台。
///
/// 读线程只在管道 EOF 时才知道结束；用户 `exit` 后管道可能一直不 EOF，
/// 所以"进程没了"由这个等待线程负责，读线程随伪控制台关闭而 EOF。
fn watch_exit(
    process: std::os::windows::io::OwnedHandle,
    id: String,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("user-terminal-watch-{id}"))
        .spawn(move || {
            pty::wait_until_exit(&process);
            retire(&sessions, &id);
        });
}

/// 从注册表摘掉会话并关闭伪控制台；重复调用是幂等的（第一个摘到的负责关）。
fn retire(sessions: &Mutex<HashMap<String, Arc<Session>>>, id: &str) {
    let exited = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
    if let Some(session) = exited {
        session.close();
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return Err(String::from("终端 id 长度非法"));
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(String::from("终端 id 只允许字母、数字、- 与 _"));
    }
    Ok(())
}

/// 起始目录：请求里的目录存在且**过工作区唯一判据**就用它，否则退回默认工作区。
///
/// 终端本身是"给人用"的裸进程（能自己 cd 到任何地方），但**起始目录**不能是数据根
/// 或它的祖先/兄弟目录——否则右键打开终端时默认 cwd 恰好是档案与凭据所在处。
/// 判据由组合根注入（`is_allowed`），本模块不引用 `config`/`app_state`。
pub fn resolve_cwd(
    requested: Option<&str>,
    default_root: &Path,
    is_allowed: impl Fn(&Path) -> Result<(), String>,
) -> PathBuf {
    if let Some(value) = requested {
        let path = Path::new(value);
        if path.is_dir() && is_allowed(path).is_ok() {
            return path.to_path_buf();
        }
    }
    default_root.to_path_buf()
}

/// 组合根的命令壳入口：解析会话工作区（默认工作区）并绑上 Tauri 事件 sink。
pub fn open_with_cwd(
    app: &AppHandle,
    manager: &UserTerminalManager,
    request: OpenRequest,
    cwd: PathBuf,
) -> Result<TerminalInfo, String> {
    manager.open(request, Arc::new(TauriFrameSink(app.clone())), cwd)
}

/// 探测可用 shell：cmd 恒有；`pwsh.exe` 找不到就不提供（不假装支持）。
pub fn shells() -> Vec<ShellOption> {
    let mut options = vec![ShellOption {
        path: cmd_path().display().to_string(),
        name: String::from("命令提示符"),
        args: Vec::new(),
    }];
    if let Some(pwsh) = find_pwsh() {
        options.push(ShellOption {
            path: pwsh.display().to_string(),
            name: String::from("PowerShell 7"),
            args: Vec::new(),
        });
    }
    options
}

fn cmd_path() -> PathBuf {
    if let Some(value) = std::env::var_os("COMSPEC") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(r"C:\Windows\System32\cmd.exe")
}

fn find_pwsh() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path_var) {
            let candidate = entry.join("pwsh.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let candidate = program_files.join("PowerShell").join("7").join("pwsh.exe");
    candidate.is_file().then_some(candidate)
}

/// 增量 UTF-8 解码：ConPTY 输出的多字节字符可能跨读边界。
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        out.push_str(
                            std::str::from_utf8(&self.pending[..valid]).expect("valid_up_to 前缀"),
                        );
                        self.pending.drain(..valid);
                    }
                    match error.error_len() {
                        Some(length) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..length);
                        }
                        // 尾部是不完整字符：留到下一块再解。
                        None => break,
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

#[tauri::command]
pub fn user_terminal_shells() -> Vec<ShellOption> {
    shells()
}

#[tauri::command]
pub fn user_terminal_write(
    state: State<'_, UserTerminalManager>,
    id: String,
    data: String,
) -> Result<(), String> {
    state.write(&id, &data)
}

#[tauri::command]
pub fn user_terminal_resize(
    state: State<'_, UserTerminalManager>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    state.resize(&id, cols, rows)
}

#[tauri::command]
pub fn user_terminal_close(state: State<'_, UserTerminalManager>, id: String) {
    state.close(&id);
}

#[tauri::command]
pub fn user_terminal_close_all(state: State<'_, UserTerminalManager>) {
    state.close_all();
}

/// 存活会话数：给"关窗口只是隐藏到托盘，但仍有终端在跑"这条提示提供状态。
#[tauri::command]
pub fn user_terminal_active_count(state: State<'_, UserTerminalManager>) -> usize {
    state.active_count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<FrameEvent>>,
    }

    impl FrameSink for RecordingSink {
        fn send(&self, event: FrameEvent) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    impl RecordingSink {
        fn snapshot(&self) -> Vec<FrameEvent> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    /// 测试的起始目录：当前目录真实存在即可；起始目录判据另有专门用例。
    fn test_cwd() -> PathBuf {
        std::env::current_dir().expect("当前目录可读")
    }

    /// 起始目录的唯一判据由调用方注入：被拒时退回默认根，不阻断终端启动。
    /// 反向对照：去掉 `is_allowed` 后"存在但被拒"那条会返回请求目录，用例红。
    #[test]
    fn resolve_cwd_falls_back_when_the_candidate_is_rejected() {
        let existing = std::env::temp_dir();
        let here = existing.to_str().expect("临时目录可转字符串");
        let fallback = Path::new("C:\\fallback");
        assert_eq!(
            resolve_cwd(Some(here), fallback, |_: &Path| Ok(())),
            existing,
            "存在且放行的目录直接用作起始目录"
        );
        assert_eq!(
            resolve_cwd(Some(here), fallback, |_: &Path| Err(String::from("数据根内部"))),
            fallback.to_path_buf(),
            "存在但被拒的目录必须回落，不能把数据根内部当起始目录"
        );
        assert_eq!(
            resolve_cwd(Some(r"C:\definitely-missing-cwd"), fallback, |_: &Path| Ok(())),
            fallback.to_path_buf(),
            "不存在的目录不能被当成起始目录"
        );
    }

    /// 用户终端唯一的出口是本模块的 `FrameEvent`：输出/退出都只能是这两种形态。
    #[test]
    fn terminal_output_only_leaves_through_user_terminal_frames() {
        let manager = UserTerminalManager::default();
        let sink = Arc::new(RecordingSink::default());
        let id = String::from("test-terminal");
        manager
            .open(
                OpenRequest {
                    id: id.clone(),
                    shell: None,
                    cwd: Some(std::env::temp_dir().display().to_string()),
                    cols: Some(80),
                    rows: Some(24),
                    title: None,
                },
                sink.clone(),
                test_cwd(),
            )
            .expect("启动用户终端");
        manager
            .write(&id, "echo SHANIU_USER_TERMINAL\r\n")
            .expect("写入终端");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut echoed = false;
        while Instant::now() < deadline {
            echoed = sink.snapshot().iter().any(|event| match event {
                FrameEvent::Output { data, .. } => data.contains("SHANIU_USER_TERMINAL"),
                FrameEvent::Exit { .. } => false,
            });
            if echoed {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(echoed, "未收到终端回显，事件: {:?}", sink.snapshot());
        manager.close(&id);

        // 退出事件由读线程在管道 EOF 后发出，和 close 不是同一个时刻：等它落地再断言。
        let exit_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < exit_deadline
            && !sink.snapshot().iter().any(|event| matches!(event, FrameEvent::Exit { .. }))
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let events = sink.snapshot();
        assert!(
            events.iter().all(|event| match event {
                FrameEvent::Output { id: event_id, .. } | FrameEvent::Exit { id: event_id, .. } =>
                    event_id == &id,
            }),
            "出现非本终端的出口事件: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(event, FrameEvent::Exit { .. })),
            "关闭后应收到退出事件: {events:?}"
        );
    }

    /// 关闭后整棵进程树要归零（Job 的 KILL_ON_JOB_CLOSE + 显式 Terminate）。
    #[test]
    fn closing_terminal_reclaims_the_whole_process_tree() {
        let cwd = std::env::temp_dir();
        let spawn = pty::spawn(
            &cmd_path().display().to_string(),
            &[
                String::from("/c"),
                String::from("ping -n 60 127.0.0.1"),
            ],
            &cwd,
            80,
            24,
        )
        .expect("启动带子进程的终端");
        assert!(
            spawn.child.active_processes() >= 1,
            "Job 内应有活动进程（cmd + ping）"
        );
        spawn.child.close();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if spawn.child.active_processes() == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "关闭后 Job 内仍有 {} 个活动进程",
            spawn.child.active_processes()
        );
    }

    /// 存活计数只反映会话表长度：开一个 +1，关掉归零（托盘提示的唯一数据源）。
    #[test]
    fn active_count_tracks_open_and_close() {
        let manager = UserTerminalManager::default();
        assert_eq!(manager.active_count(), 0, "初始应为 0");
        let sink = Arc::new(RecordingSink::default());
        let id = String::from("count-terminal");
        manager
            .open(
                OpenRequest {
                    id: id.clone(),
                    shell: None,
                    cwd: Some(std::env::temp_dir().display().to_string()),
                    cols: Some(80),
                    rows: Some(24),
                    title: None,
                },
                sink,
                test_cwd(),
            )
            .expect("启动用户终端");
        assert_eq!(manager.active_count(), 1, "开一个终端后应为 1");
        manager.close(&id);
        assert_eq!(manager.active_count(), 0, "关闭后应归零");
    }

    /// 多标签 + 只关一个标签：计数必须落在剩下的真实终端数上。
    ///
    /// 托盘提示直接读这个数，它把已关的标签也算进去就会说错话。
    #[test]
    fn closing_one_tab_keeps_the_other_counted() {
        let manager = UserTerminalManager::default();
        let sink = Arc::new(RecordingSink::default());
        for id in ["tab-a", "tab-b"] {
            manager
                .open(
                    OpenRequest {
                        id: String::from(id),
                        shell: None,
                        cwd: Some(std::env::temp_dir().display().to_string()),
                        cols: Some(80),
                        rows: Some(24),
                        title: None,
                    },
                    sink.clone(),
                    test_cwd(),
                )
                .expect("启动用户终端");
        }
        assert_eq!(manager.active_count(), 2, "两个标签应计 2");
        manager.close("tab-a");
        assert_eq!(
            manager.active_count(),
            1,
            "只关一个标签后应计 1（剩下那个仍在跑）"
        );
    }

    /// 用户自己 `exit`（进程自然结束）：计数立刻回落，不等标签关掉。
    ///
    /// 这是托盘提示的另一半真话来源：已退出的终端不该被算成"仍在运行"。
    #[test]
    fn exited_terminal_leaves_the_active_count() {
        let manager = UserTerminalManager::default();
        let sink = Arc::new(RecordingSink::default());
        let id = String::from("exit-terminal");
        manager
            .open(
                OpenRequest {
                    id: id.clone(),
                    shell: None,
                    cwd: Some(std::env::temp_dir().display().to_string()),
                    cols: Some(80),
                    rows: Some(24),
                    title: None,
                },
                sink.clone(),
                test_cwd(),
            )
            .expect("启动用户终端");
        assert_eq!(manager.active_count(), 1);
        manager.write(&id, "exit\r\n").expect("写入 exit");

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && manager.active_count() != 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            manager.active_count(),
            0,
            "进程退出后应立刻从注册表摘除，事件: {:?}",
            sink.snapshot()
        );
    }

    #[test]
    fn shells_always_offer_cmd_and_only_real_pwsh() {
        let options = shells();
        assert_eq!(options[0].name, "命令提示符");
        assert!(Path::new(&options[0].path).is_file());
        for option in &options[1..] {
            // 列出什么就必须真的存在：不假装支持未安装的 shell。
            assert!(Path::new(&option.path).is_file());
        }
    }

    /// 架构护栏：生产代码里不得出现通往模型上下文的标识符。
    ///
    /// 只扫生产代码（注释与测试块先剔除），防止以后有人把用户终端接进
    /// `EventMsg` / 会话日志 / `unified_exec` 时测试还绿着。
    #[test]
    fn module_never_touches_model_context() {
        let forbidden = [
            concat!("Event", "Msg"),
            concat!("roll", "out"),
            concat!("event_", "journal"),
            concat!("unified", "_exec"),
            concat!("Tauri", "EventSink"),
            concat!("sandbox", "::"),
        ];
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("pty.rs", include_str!("pty.rs")),
        ] {
            let production = strip_production(source);
            for needle in forbidden {
                assert!(
                    !production.contains(needle),
                    "{name} 的生产代码出现 `{needle}`：用户终端不得接入模型上下文"
                );
            }
        }
    }

    fn strip_production(source: &str) -> String {
        let without_tests = source
            .split("#[cfg(test)]")
            .next()
            .expect("源文件至少有一个片段");
        without_tests
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !(trimmed.starts_with("//") || trimmed.starts_with("/*"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
