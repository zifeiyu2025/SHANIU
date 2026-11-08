//! Windows 沙箱实现（ADR-002 M2）：受限令牌（剥离全部特权）+ JobObject。
//!
//! 全部 Win32 unsafe 隔离在本文件。流程（蒸馏自 codex windows-sandbox 的 M2 子集）：
//! 1. OpenProcessToken 取自身主令牌（TOKEN_DUPLICATE/QUERY/ASSIGN_PRIMARY/ADJUST_*）；
//! 2. GetTokenInformation(TokenPrivileges) 枚举全部特权；
//! 3. CreateRestrictedToken 以 SE_PRIVILEGE_REMOVED **删除全部特权**
//!    （删除而非禁用：子进程拿不到 SeDebug/SeBackup 等任何提权能力）；
//! 4. CreateJobObject + KILL_ON_JOB_CLOSE（应用崩溃/退出兜底回收子进程树）；
//! 5. 手工匿名管道（写端可继承）+ STARTF_USESTDHANDLES 接管子进程 stdio；
//! 6. CreateProcessAsUserW 以 CREATE_SUSPENDED 挂起启动 → AssignProcessToJobObject
//!    → ResumeThread（子进程任何代码执行前已入 Job）；
//! 7. std::thread 读线程 + blocking_send 桥接 tokio 通道；句柄 RAII（CloseHandle）。
//!
//! 失败语义：任一步 Err → 上层 refuse-to-spawn（FF-4：沙箱创建失败执行数为 0）。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows::Win32::Security::{
    CreateRestrictedToken, GetTokenInformation, LUID_AND_ATTRIBUTES,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SE_PRIVILEGE_REMOVED, SID_AND_ATTRIBUTES,
    TOKEN_ACCESS_MASK, TOKEN_PRIVILEGES, TokenPrivileges, PSID,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken,
    UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_CREATION_FLAGS, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, STARTUPINFOW, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT,
};

// ============================== RAII 句柄守卫 ==============================

/// 内核句柄 RAII 包装。HANDLE 本身 !Send，但内核对象句柄无线程亲和性，
/// 跨线程等待/关闭是安全且常规的用法（同 std OwnedHandle: Send）。
struct HandleGuard(HANDLE);
unsafe impl Send for HandleGuard {}

impl HandleGuard {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }
    fn handle(&self) -> HANDLE {
        self.0
    }
    /// 转移所有权到 std File（用于 stdio 管道端），避免双重 CloseHandle。
    fn into_file(self) -> std::fs::File {
        let raw: RawHandle = self.0.0.cast();
        std::mem::forget(self);
        unsafe { std::fs::File::from_raw_handle(raw) }
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Job 对象守卫：drop 关闭 Job 句柄（KILL_ON_JOB_CLOSE 兜底回收子进程树）。
pub struct JobGuard(HandleGuard);
unsafe impl Send for JobGuard {}

impl JobGuard {
    fn handle(&self) -> HANDLE {
        self.0.handle()
    }
}

// ============================== 宽字符助手 ==============================

pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

// ============================== 令牌 ==============================

/// 打开自身进程主令牌。
unsafe fn open_current_token() -> Result<HandleGuard, String> {
    let desired: TOKEN_ACCESS_MASK = windows::Win32::Security::TOKEN_DUPLICATE
        | windows::Win32::Security::TOKEN_QUERY
        | windows::Win32::Security::TOKEN_ASSIGN_PRIMARY
        | windows::Win32::Security::TOKEN_ADJUST_DEFAULT
        | windows::Win32::Security::TOKEN_ADJUST_SESSIONID;
    let mut token = HANDLE::default();
    OpenProcessToken(GetCurrentProcess(), desired, &mut token)
        .map_err(|e| format!("OpenProcessToken 失败: {e}"))?;
    Ok(HandleGuard::new(token))
}

/// 枚举令牌全部特权 → SE_PRIVILEGE_REMOVED 删除清单。
///
/// 5b 现场核对过：祖先目录的遍历不依赖 `SeChangeNotifyPrivilege`——
/// 叶子目录授权后，删掉全部特权也能在容器内列出/读取/写入（见
/// `appcontainer_leaf_grant_allows_list_read_write_then_revoke_denies`）。
unsafe fn privileges_to_remove(token: HANDLE) -> Result<Vec<LUID_AND_ATTRIBUTES>, String> {
    let mut needed = 0u32;
    let _ = GetTokenInformation(
        token,
        TokenPrivileges,
        None,
        0,
        &mut needed,
    );
    if needed == 0 {
        return Err("GetTokenInformation(TokenPrivileges) 返回长度为 0".into());
    }
    let mut buffer = vec![0u8; needed as usize];
    GetTokenInformation(
        token,
        TokenPrivileges,
        Some(buffer.as_mut_ptr().cast()),
        needed,
        &mut needed,
    )
    .map_err(|e| format!("GetTokenInformation(TokenPrivileges) 失败: {e}"))?;
    let privileges = *(buffer.as_ptr() as *const TOKEN_PRIVILEGES);
    let count = privileges.PrivilegeCount as usize;
    let items = std::slice::from_raw_parts(privileges.Privileges.as_ptr(), count);
    Ok(items
        .iter()
        .map(|luid_attr| LUID_AND_ATTRIBUTES {
            Luid: luid_attr.Luid,
            Attributes: SE_PRIVILEGE_REMOVED,
        })
        .collect())
}

/// 创建「删除全部特权」的受限令牌。
unsafe fn create_restricted_token() -> Result<HandleGuard, String> {
    let base = open_current_token()?;
    let to_remove = privileges_to_remove(base.handle())?;
    let mut new_token = HANDLE::default();
    CreateRestrictedToken(
        base.handle(),
        windows::Win32::Security::CREATE_RESTRICTED_TOKEN_FLAGS(0),
        None,
        Some(&to_remove),
        None,
        &mut new_token,
    )
    .map_err(|e| format!("CreateRestrictedToken 失败: {e}"))?;
    Ok(HandleGuard::new(new_token))
}

// 实测记录（2026-11-08）：AppContainer 走 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
// （官方「Launch an AppContainer」样例路径），不要改成 `SetTokenInformation(TokenAppContainerSid)`：
// 后者对普通令牌与受限令牌都返回 ERROR_INVALID_PARAMETER（0x80070057），不是可用的标记方式。

// ============================== 匿名管道 ==============================
/// 创建一对匿名管道：读端转 std::fs::File（Send，父进程读）；
/// 写端 HandleGuard（子进程继承后由父进程关闭）。
unsafe fn create_inheritable_pipe() -> Result<(std::fs::File, HandleGuard), String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read_end = HANDLE::default();
    let mut write_end = HANDLE::default();
    CreatePipe(&mut read_end, &mut write_end, Some(&sa), 0)
        .map_err(|e| format!("CreatePipe 失败: {e}"))?;
    let read_raw: RawHandle = read_end.0.cast();
    let read_file = std::fs::File::from_raw_handle(read_raw);
    // read_end 是 Copy 的 HANDLE 值，无需 forget；File 独占该句柄的所有权
    Ok((read_file, HandleGuard::new(write_end)))
}

// ============================== 受限启动 ==============================

/// 已启动的受限子进程：纯 Win32 wait/kill（std Child 不支持裸句柄构造）。
pub struct RestrictedChild {
    process: OwnedHandle, // Send ✓
    _job: JobGuard,
}

unsafe impl Send for RestrictedChild {}

/// stdio 行载荷（0 = stdout，1 = stderr）。
pub type ExecLine = (u8, String);

impl RestrictedChild {
    /// 阻塞等待退出（经 spawn_blocking 在阻塞线程池运行），返回退出码。
    pub async fn wait(&mut self) -> io::Result<i32> {
        let process = self
            .process
            .try_clone()
            .map_err(|e| io::Error::other(format!("复制进程句柄失败: {e}")))?;
        use std::os::windows::io::AsRawHandle as _;
        tokio::task::spawn_blocking(move || unsafe {
            WaitForSingleObject(HANDLE(process.as_raw_handle()), INFINITE);
            let mut code: u32 = 0;
            let _ = GetExitCodeProcess(HANDLE(process.as_raw_handle()), &mut code);
            Ok(code as i32)
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    /// 终止子进程（超时/取消时调用）。
    pub fn kill(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(
                HANDLE(self.process.as_raw_handle()),
                1,
            );
        }
    }
}

/// 以「删除全部特权的受限令牌 + JobObject」启动子进程。
/// stdout/stderr 由 std::thread 读线程逐行送入 tx（blocking_send 桥接 tokio）。
pub fn spawn_restricted_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    tx: mpsc::Sender<ExecLine>,
) -> Result<RestrictedChild, String> {
    let mut command_line = program.to_string();
    for arg in args {
        command_line.push(' ');
        command_line.push_str(arg);
    }
    let mut cmdline_wide = wide(&command_line);
    let cwd_wide = wide(cwd.as_os_str());

    unsafe {
        // 1-3) 受限令牌（删除全部特权）
        let token_guard = create_restricted_token()?;

        // 4) Job 对象：KILL_ON_JOB_CLOSE
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("CreateJobObject 失败: {e}"))?;
        let job_guard = JobGuard(HandleGuard::new(job));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job_guard.handle(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject 失败: {e}"))?;

        // 5) stdio 管道（写端可继承）
        let (out_read, out_write) = create_inheritable_pipe()?;
        let (err_read, err_write) = create_inheritable_pipe()?;

        // 6) CREATE_SUSPENDED 启动
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: HANDLE::default(),
            hStdOutput: out_write.handle(),
            hStdError: err_write.handle(),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token_guard.handle()),
            PCWSTR::null(),
            Some(PWSTR(cmdline_wide.as_mut_ptr())),
            None,
            None,
            true,
            PROCESS_CREATION_FLAGS(CREATE_SUSPENDED.0),
            None,
            PCWSTR::from_raw(cwd_wide.as_ptr()),
            &si,
            &mut pi,
        )
        .map_err(|e| format!("CreateProcessAsUserW 失败: {e}"))?;

        // 7) 入 Job（子进程任何代码执行前）→ 恢复运行
        AssignProcessToJobObject(job_guard.handle(), pi.hProcess)
            .map_err(|e| format!("AssignProcessToJobObject 失败: {e}"))?;
        ResumeThread(pi.hThread);

        // 8) 父侧：关闭继承写端；读端 File 交 std::thread 读线程
        drop(out_write);
        drop(err_write);
        spawn_pipe_reader(out_read, 0, tx.clone());
        spawn_pipe_reader(err_read, 1, tx);

        // 进程句柄 → OwnedHandle（wait/kill 用）
        let raw: RawHandle = pi.hProcess.0.cast();
        let process = OwnedHandle::from_raw_handle(raw);
        // 线程主句柄立即关闭（线程已 Resume；si 为 Copy 无需 drop）
        let _ = CloseHandle(pi.hThread);
        Ok(RestrictedChild { process, _job: job_guard })
    }
}

/// 以「删除全部特权的受限令牌 + JobObject + **AppContainer**」启动子进程（SH-021）。
///
/// 与 `spawn_restricted_command` 的区别：
///  - 进程在 AppContainer 里运行，默认只能读写被授予容器 SID 的目录（ACE 由 `acl` 管理）；
///  - `capabilities` 为空 = 没有 `internetClient`，出站网络被系统拒绝（v1 默认断网）；
///  - 句柄白名单只放两根管道，避免把父进程其它可继承句柄漏给沙箱子进程。
pub fn spawn_appcontainer_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    app_container_sid: PSID,
    capabilities: Vec<PSID>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<RestrictedChild, String> {
    let mut command_line = program.to_string();
    for arg in args {
        command_line.push(' ');
        command_line.push_str(arg);
    }
    let mut cmdline_wide = wide(&command_line);
    let cwd_wide = wide(cwd.as_os_str());

    unsafe {
        // 纵深第 1 层：删除全部特权的受限令牌（提权面）
        let token_guard = create_restricted_token()?;

        // 纵深第 2 层：Job（进程树回收 / 资源边界）
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("CreateJobObject 失败: {e}"))?;
        let job_guard = JobGuard(HandleGuard::new(job));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job_guard.handle(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject 失败: {e}"))?;

        let (out_read, out_write) = create_inheritable_pipe()?;
        let (err_read, err_write) = create_inheritable_pipe()?;

        // 纵深第 3 层：容器（路径/读隔离 + 网络默认拒绝）；
        // 属性表：句柄白名单 + Job + 容器 SID/能力，创建即入容器、入 Job。
        let mut attributes = AttrList::new(3)?;
        attributes.set_handle_list(vec![out_write.handle(), err_write.handle()])?;
        attributes.set_job(job_guard.handle())?;
        attributes.set_security_capabilities(app_container_sid, &capabilities)?;

        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = HANDLE::default();
        startup.StartupInfo.hStdOutput = out_write.handle();
        startup.StartupInfo.hStdError = err_write.handle();
        startup.lpAttributeList = attributes.as_mut_ptr();

        let flags = PROCESS_CREATION_FLAGS(
            CREATE_SUSPENDED.0
                | CREATE_NO_WINDOW.0
                | EXTENDED_STARTUPINFO_PRESENT.0
                | CREATE_UNICODE_ENVIRONMENT.0,
        );
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token_guard.handle()),
            PCWSTR::null(),
            Some(PWSTR(cmdline_wide.as_mut_ptr())),
            None,
            None,
            true,
            flags,
            None,
            PCWSTR::from_raw(cwd_wide.as_ptr()),
            &startup.StartupInfo,
            &mut pi,
        )
        .map_err(|e| {
            format!(
                "CreateProcessAsUserW(AppContainer) 失败（program={program}，cwd={}）: {e}。\
                 容器内无法启动该命令时不会自动降级；如确认是工具链不兼容，\
                 请显式设置 sandbox.os = \"logical\"",
                cwd.display()
            )
        })?;

        ResumeThread(pi.hThread);

        // 父侧：关闭继承写端；读端交读线程
        drop(out_write);
        drop(err_write);
        spawn_pipe_reader(out_read, 0, tx.clone());
        spawn_pipe_reader(err_read, 1, tx);

        let raw: RawHandle = pi.hProcess.0.cast();
        let process = OwnedHandle::from_raw_handle(raw);
        let _ = CloseHandle(pi.hThread);
        Ok(RestrictedChild { process, _job: job_guard })
    }
}


/// std::thread 逐行读管道 → blocking_send 桥接 tokio 通道（codex read_handle_loop 同思路）。
fn spawn_pipe_reader(read: std::fs::File, channel: u8, tx: mpsc::Sender<ExecLine>) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        // BufReader 必须活在循环外。每轮新建的话，它会把一次 read 拿到的整块（默认 8KB）
        // 读进自己的缓冲区、只交出一行，然后随本轮结束被 drop——缓冲区里剩下的行**全部丢弃**。
        // 实测症状：受限沙箱里 `dir /b` 输出四行，模型只收到第一行，于是反复重跑同一命令
        // 却永远看不到完整清单。
        let mut reader = std::io::BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if tx.blocking_send((channel, trimmed)).is_err() {
                break;
            }
        }
    });
}

// ============================== MCP stdio 受限启动（SH-060） ==============================
//
// 与 `spawn_restricted_command` 的差异（**纯新增**，旧路径一行未改）：
//  - stdin 真实接线（MCP 是双向 NDJSON，不是一次性命令）；
//  - **句柄继承白名单**：`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 只放三根管道。若只靠
//    「可继承」标志，`bInheritHandles=TRUE` 会把父进程里所有可继承句柄漏给第三方
//    server——这是真实的信息泄漏面；
//  - `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在创建时原子入 Job，没有「先跑起来再入 Job」
//    的窗口，也避免 Assign 失败后留下挂起孤儿进程；
//  - env 白名单：默认只保留 node 启动必需的系统变量，其余键值来自配置；缺关键变量
//    时报出**具体缺哪个**，而不是笼统的 "spawn failed"；
//  - stdout 单行硬上限 + stderr 有界环（不落盘，防刷日志的 server 写满磁盘）。
//
// 参考实现取舍（ADR/提交信息同口径）：
//  - codex：`rmcp-client` 用命名管道 + `bInheritHandle=0`（管道安全描述符只允许
//    owner/SYSTEM），env 走显式白名单 `WINDOWS_CORE_ENV_VARS`，stdout 有
//    `MAX_MCP_STDIO_LINE_BYTES = 8MB` 硬上限，Windows 侧用 Job Object 收树。
//  - ZCode：MCP 由 SDK/上层进程管理，Windows 侧未做句柄白名单；我们不复用它。
//  - 本实现选 codex 的「命名管道之外的另一条正确路」：匿名管道 + HANDLE_LIST。
//    理由：SHANIU 已有匿名管道沙箱原语，HANDLE_LIST 与命名管道在防泄漏上等价，
//    但少一套命名空间/安全描述符逻辑；同时吸收 codex 的 8MB 行上限与 env 白名单。

pub const MAX_MCP_STDIO_LINE_BYTES: usize = 8 * 1024 * 1024;
/// stderr 只保留最近 64 KiB 供诊断；不写盘，避免刷日志的 server 吃满磁盘。
const STDERR_TAIL_CAPACITY: usize = 64 * 1024;

/// Windows 上启动 node 类 MCP server 的基础环境（codex `WINDOWS_CORE_ENV_VARS` 同口径）。
/// **只按名字从父进程复制**，不在此列出的变量一律不传给 server。
pub const WINDOWS_CORE_ENV_VARS: &[&str] = &[
    // 路径解析
    "PATH",
    "PATHEXT",
    // 系统根
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "COMSPEC",
    // 用户上下文
    "USERNAME",
    "USERDOMAIN",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    // 程序目录
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    // 应用数据与缓存
    "LOCALAPPDATA",
    "APPDATA",
    // 临时目录
    "TEMP",
    "TMP",
    "TMPDIR",
    // node / 常见运行时
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// stdout 读线程向 mcp_client 回传的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioEvent {
    /// 一行 NDJSON（已去掉行尾 CR/LF，按 UTF-8 解码）。
    Line(String),
    /// 协议级错误：单行超限或非 UTF-8。**必须断开该连接**，不得继续追加缓冲。
    ProtocolError(String),
}

/// stderr 有界环：只保留最近 `capacity` 字节，旧内容按字符边界丢弃。
#[derive(Debug)]
struct StderrTail {
    text: String,
    capacity: usize,
    dropped_bytes: u64,
}

impl StderrTail {
    fn new(capacity: usize) -> Self {
        Self { text: String::new(), capacity, dropped_bytes: 0 }
    }

    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() <= self.capacity {
            return;
        }
        let mut cut = self.text.len() - self.capacity;
        while cut < self.text.len() && !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        self.text.drain(..cut);
        self.dropped_bytes += cut as u64;
    }
}

/// 已启动的受限 stdio 子进程（MCP server）。
pub struct RestrictedStdioChild {
    process: OwnedHandle,
    _job: JobGuard,
    stdin: Option<std::fs::File>,
    stderr_tail: Arc<Mutex<StderrTail>>,
    pid: u32,
}

unsafe impl Send for RestrictedStdioChild {}

impl RestrictedStdioChild {
    /// 子进程 PID（审计/诊断用；不要用它做定位，句柄才是身份）。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 取走 stdin 写端（JSON-RPC 请求从这里写）。只能取一次。
    pub fn take_stdin(&mut self) -> Option<std::fs::File> {
        self.stdin.take()
    }

    /// 阻塞等待退出（经 spawn_blocking 在阻塞线程池运行），返回退出码。
    pub async fn wait(&mut self) -> io::Result<i32> {
        let process = self
            .process
            .try_clone()
            .map_err(|e| io::Error::other(format!("复制进程句柄失败: {e}")))?;
        tokio::task::spawn_blocking(move || unsafe {
            WaitForSingleObject(HANDLE(process.as_raw_handle()), INFINITE);
            let mut code: u32 = 0;
            let _ = GetExitCodeProcess(HANDLE(process.as_raw_handle()), &mut code);
            Ok(code as i32)
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    /// 终止子进程（超时/取消/协议错误时调用；Job 会连带收掉整棵树）。
    pub fn kill(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(
                HANDLE(self.process.as_raw_handle()),
                1,
            );
        }
    }

    /// 取 stderr 尾部快照（文本, 已丢弃字节数）。读线程退出与否都不阻塞调用方。
    pub fn stderr_tail(&self) -> (String, u64) {
        match self.stderr_tail.lock() {
            Ok(guard) => (guard.text.clone(), guard.dropped_bytes),
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                (guard.text.clone(), guard.dropped_bytes)
            }
        }
    }
}

/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` / `JOB_LIST` / `SECURITY_CAPABILITIES` 的持有者。
/// 属性值指向自身 Vec / 结构体，必须活到 CreateProcess 返回。
struct AttrList {
    buffer: Vec<u8>,
    handle_list: Vec<HANDLE>,
    job_list: Vec<HANDLE>,
    /// `SECURITY_CAPABILITIES.Capabilities` 指向这里：Vec 必须在结构体存活期间不被移动。
    capabilities: Vec<SID_AND_ATTRIBUTES>,
    security_capabilities: Option<SECURITY_CAPABILITIES>,
}

impl AttrList {
    fn new(count: u32) -> Result<Self, String> {
        let mut size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, count, None, &mut size);
        }
        if size == 0 {
            return Err(format!(
                "InitializeProcThreadAttributeList(测量) 失败: {}",
                io::Error::last_os_error()
            ));
        }
        let mut buffer = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe {
            InitializeProcThreadAttributeList(Some(list), count, None, &mut size)
                .map_err(|e| format!("InitializeProcThreadAttributeList 失败: {e}"))?;
        }
        Ok(Self {
            buffer,
            handle_list: Vec::new(),
            job_list: Vec::new(),
            capabilities: Vec::new(),
            security_capabilities: None,
        })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_mut_ptr().cast())
    }

    fn set_handle_list(&mut self, handles: Vec<HANDLE>) -> Result<(), String> {
        self.handle_list = handles;
        let list = self.as_mut_ptr();
        let value = self.handle_list.as_ptr().cast();
        let size = std::mem::size_of_val(self.handle_list.as_slice());
        unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(value),
                size,
                None,
                None,
            )
        }
        .map_err(|e| format!("UpdateProcThreadAttribute(HANDLE_LIST) 失败: {e}"))
    }

    fn set_job(&mut self, job: HANDLE) -> Result<(), String> {
        self.job_list = vec![job];
        let list = self.as_mut_ptr();
        let value = self.job_list.as_ptr().cast();
        let size = std::mem::size_of_val(self.job_list.as_slice());
        unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(value),
                size,
                None,
                None,
            )
        }
        .map_err(|e| format!("UpdateProcThreadAttribute(JOB_LIST) 失败: {e}"))
    }

    /// 挂 AppContainer 身份（`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`）。
    ///
    /// 这是官方「Launch an AppContainer」样例的路径：容器 SID 与能力 SID 一起交给
    /// 进程创建属性，由内核在建进程时打标。**传空能力列表 = 无 `internetClient`**，
    /// 出站网络即被容器边界默认拒绝（v1 默认断网就靠这一点，不是靠额外代码拦）。
    fn set_security_capabilities(
        &mut self,
        app_container_sid: PSID,
        capabilities: &[PSID],
    ) -> Result<(), String> {
        const SE_GROUP_ENABLED: u32 = 0x0000_0004;
        self.capabilities = capabilities
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: *sid,
                Attributes: SE_GROUP_ENABLED,
            })
            .collect();
        self.security_capabilities = Some(SECURITY_CAPABILITIES {
            AppContainerSid: app_container_sid,
            Capabilities: if self.capabilities.is_empty() {
                std::ptr::null_mut()
            } else {
                self.capabilities.as_mut_ptr()
            },
            CapabilityCount: self.capabilities.len() as u32,
            Reserved: 0,
        });
        let list = self.as_mut_ptr();
        // 属性值必须指向稳定地址：结构体存在字段里，Vec 已提前填好，这里只取地址。
        let value = (self
            .security_capabilities
            .as_ref()
            .expect("刚写入的 SECURITY_CAPABILITIES")
            as *const SECURITY_CAPABILITIES)
            .cast::<core::ffi::c_void>();
        unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                Some(value),
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
        }
        .map_err(|e| format!("UpdateProcThreadAttribute(SECURITY_CAPABILITIES) 失败: {e}"))
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}

/// 创建一对可继承的匿名管道（read, write）。
unsafe fn create_inheritable_pipe_pair() -> Result<(HandleGuard, HandleGuard), String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read_end = HANDLE::default();
    let mut write_end = HANDLE::default();
    CreatePipe(&mut read_end, &mut write_end, Some(&sa), 0)
        .map_err(|e| format!("CreatePipe 失败: {e}"))?;
    Ok((HandleGuard::new(read_end), HandleGuard::new(write_end)))
}

/// 组装 server 环境：默认白名单从父进程复制 + 配置里的显式键值覆盖。
fn build_environment(extra_env: &BTreeMap<String, String>) -> Result<Vec<(String, OsString)>, String> {
    let parent: std::collections::HashMap<String, (String, OsString)> = std::env::vars_os()
        .map(|(key, value)| {
            let name = key.to_string_lossy().to_string();
            (name.to_ascii_uppercase(), (name, value))
        })
        .collect();

    let mut merged: BTreeMap<String, (String, OsString)> = BTreeMap::new();
    for name in WINDOWS_CORE_ENV_VARS {
        if let Some((original, value)) = parent.get(&name.to_ascii_uppercase()) {
            merged.insert(name.to_ascii_uppercase(), (original.clone(), value.clone()));
        }
    }

    // 可诊断性：node 与系统 DLL 加载的硬前提缺失时直接报出变量名，不让它变成
    // CreateProcess 的一个笼统错误码。
    if !merged.contains_key("SYSTEMROOT") && !merged.contains_key("WINDIR") {
        return Err(
            "环境白名单缺少 SystemRoot/windir：node 等运行时无法加载系统 DLL。\
             请检查启动 SHANIU 的环境，或在 server 的 env 里显式配置"
                .into(),
        );
    }
    if !merged.contains_key("PATH") {
        return Err(
            "环境白名单缺少 PATH：无法解析 server 可执行文件。\
             请检查启动 SHANIU 的环境，或在 server 的 env 里显式配置"
                .into(),
        );
    }

    for (key, value) in extra_env {
        if key.trim().is_empty() {
            return Err("MCP server 的 env 含空键名".into());
        }
        merged.insert(key.to_ascii_uppercase(), (key.clone(), OsString::from(value)));
    }

    // BTreeMap 按大写键排序，正好满足 Windows 环境块的建议顺序。
    Ok(merged.into_values().collect())
}

/// 构造 `KEY=VALUE\0...\0` 的 UTF-16 环境块。
fn build_environment_block(env: &[(String, OsString)]) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in env {
        block.extend(key.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.to_string_lossy().encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

fn is_script_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "cmd" | "bat" | "ps1" | "vbs" | "js"
            )
        })
}

/// 解析可执行文件为绝对路径。C-R5：不做 shell 解释，`.cmd/.bat` 明确拒绝。
fn resolve_program(
    program: &str,
    env: &[(String, OsString)],
    cwd: &Path,
) -> Result<PathBuf, String> {
    let program = program.trim();
    if program.is_empty() {
        return Err("server 的 command 为空：无法启动".into());
    }
    if !cwd.is_dir() {
        return Err(format!("server 的 cwd「{}」不是可访问目录", cwd.display()));
    }
    if is_script_extension(Path::new(program)) {
        return Err(format!(
            "不支持脚本型可执行文件「{program}」（C-R5：不做 shell 解释）。\
             请改为真实可执行文件（如 node 的绝对路径），并把脚本路径放进 args"
        ));
    }

    let path_var = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let pathext_var = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATHEXT"))
        .map(|(_, value)| value.to_string_lossy().to_string())
        .unwrap_or_else(|| ".COM;.EXE".into());

    let has_extension = Path::new(program).extension().is_some();
    let mut extensions: Vec<String> = if has_extension {
        vec![String::new()]
    } else {
        pathext_var
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
            .map(str::to_string)
            .collect()
    };
    if !has_extension && !extensions.iter().any(|ext| ext.eq_ignore_ascii_case(".EXE")) {
        extensions.push(".EXE".into());
    }

    let has_sep = program.contains('\\') || program.contains('/');
    let base = if Path::new(program).is_absolute() {
        PathBuf::from(program)
    } else if has_sep {
        cwd.join(program)
    } else {
        PathBuf::from(program)
    };
    let file_name = base
        .file_name()
        .ok_or_else(|| format!("server 的 command「{program}」不是有效文件名"))?
        .to_os_string();

    let search_dirs: Vec<PathBuf> = if Path::new(program).is_absolute() || has_sep {
        vec![base
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf())]
    } else {
        let mut dirs: Vec<PathBuf> = path_var
            .split(';')
            .map(|dir| dir.trim().trim_matches('"'))
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .collect();
        // 当前目录排在最后：只在 PATH 中找不到时才回退（与 which 行为一致）。
        dirs.push(cwd.to_path_buf());
        dirs
    };

    for dir in &search_dirs {
        for ext in &extensions {
            let mut name = file_name.clone();
            if !ext.is_empty() {
                name.push(ext);
            }
            let candidate = dir.join(&name);
            if !candidate.is_file() {
                continue;
            }
            if is_script_extension(&candidate) {
                return Err(format!(
                    "「{program}」解析到脚本「{}」，v1 不支持（C-R5：不做 shell 解释）。\
                     请配置真实可执行文件路径",
                    candidate.display()
                ));
            }
            let canonical = std::fs::canonicalize(&candidate).unwrap_or(candidate);
            // CreateProcessAsUserW 的 lpApplicationName 不接受 `\\?\` 扩展前缀
            // （传进去会得到 ERROR_INVALID_PARAMETER），这里剥掉。
            let canonical = match canonical.to_string_lossy().strip_prefix(r"\\?\") {
                Some(stripped) => PathBuf::from(stripped),
                None => canonical,
            };
            return Ok(canonical);
        }
    }

    let preview = search_dirs
        .iter()
        .take(6)
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "找不到可执行文件「{program}」：已搜索 {} 个目录（{}）。\
         请检查 PATH 白名单或改用绝对路径",
        search_dirs.len(),
        preview
    ))
}

/// 按 `CommandLineToArgvW` 规则转义一个 argv 项（C-R5：逐项传递，绝不拼 shell 字符串）。
fn append_quoted_arg(command_line: &mut String, arg: &str) {
    let needs_quotes = arg.is_empty() || arg.chars().any(|ch| matches!(ch, ' ' | '\t' | '"'));
    if !needs_quotes {
        command_line.push_str(arg);
        return;
    }
    command_line.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    command_line.push('\\');
                }
                command_line.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    command_line.push('\\');
                }
                command_line.push(ch);
                backslashes = 0;
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        command_line.push('\\');
    }
    command_line.push('"');
}

fn build_command_line(program: &Path, args: &[&str]) -> String {
    let mut command_line = String::new();
    append_quoted_arg(&mut command_line, &program.to_string_lossy());
    for arg in args {
        command_line.push(' ');
        append_quoted_arg(&mut command_line, arg);
    }
    command_line
}

/// 读一行，硬上限 `max` 字节。超限返回 Err（调用方转成 ProtocolError 并断开）。
fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<String>, String> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| format!("读取 server stdout 失败: {e}"))?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len() + take > max {
            return Err(format!(
                "server stdout 单行超过 {max} 字节上限（已累计 {} 字节）：按 MCP 协议错误断开",
                line.len() + take
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|e| format!("server stdout 不是 UTF-8（MCP 要求 NDJSON）: {e}"))
}

/// stdout 读线程：逐行送入 tx；超限/非 UTF-8 → ProtocolError 后立即停止读取（断开）。
fn spawn_stdout_reader(read: std::fs::File, tx: mpsc::Sender<StdioEvent>, max_line_bytes: usize) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(read);
        loop {
            match read_bounded_line(&mut reader, max_line_bytes) {
                Ok(None) => break,
                Ok(Some(line)) => {
                    if tx.blocking_send(StdioEvent::Line(line)).is_err() {
                        break;
                    }
                }
                Err(reason) => {
                    // 记录后立即断开：绝不继续向缓冲追加巨长行。
                    tracing::warn!(%reason, "MCP server stdout 协议错误，断开读取");
                    let _ = tx.blocking_send(StdioEvent::ProtocolError(reason));
                    break;
                }
            }
        }
    });
}

/// stderr 读线程：写入有界环，**不落盘**，退出也不阻塞主流程。
fn spawn_stderr_reader(mut read: std::fs::File, tail: Arc<Mutex<StderrTail>>) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match read.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let text = String::from_utf8_lossy(&buffer[..count]);
                    if let Ok(mut guard) = tail.lock() {
                        guard.push(&text);
                    }
                }
            }
        }
    });
}

/// 启动身份：决定用哪几层隔离（沙箱机制与身份一一对应）。
#[derive(Clone, Copy)]
enum StdioIdentity<'a> {
    /// 删除全部特权的受限令牌 + Job + 句柄白名单（默认）。
    RestrictedToken,
    /// 逐服务器显式豁免（X10）：仅 Job + 句柄白名单。
    Unsandboxed,
    /// AppContainer + 受限令牌 + Job + 句柄白名单（SH-064 5a）。
    AppContainer { sid: PSID, capabilities: &'a [PSID] },
}

struct StdioSpawnSpec<'a> {
    program: &'a str,
    args: &'a [&'a str],
    cwd: &'a Path,
    env: &'a BTreeMap<String, String>,
    identity: StdioIdentity<'a>,
    max_line_bytes: usize,
}

fn spawn_stdio(spec: &StdioSpawnSpec<'_>, tx: mpsc::Sender<StdioEvent>) -> Result<RestrictedStdioChild, String> {
    let env = build_environment(spec.env)?;
    let resolved = resolve_program(spec.program, &env, spec.cwd)?;
    let command_line = build_command_line(&resolved, spec.args);
    let mut command_line_wide = wide(&command_line);
    let application_wide = wide(&resolved);
    let cwd_wide = wide(spec.cwd.as_os_str());
    let environment_block = build_environment_block(&env);
    let mode = match spec.identity {
        StdioIdentity::RestrictedToken => "受限令牌 + Job",
        StdioIdentity::Unsandboxed => "未沙箱（显式豁免）",
        StdioIdentity::AppContainer { .. } => "AppContainer + 受限令牌 + Job",
    };

    unsafe {
        let token = match spec.identity {
            StdioIdentity::Unsandboxed => None,
            _ => Some(create_restricted_token()?),
        };

        // Job：KILL_ON_JOB_CLOSE，随句柄关闭收掉整棵树。
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("CreateJobObject 失败: {e}"))?;
        let job_guard = JobGuard(HandleGuard::new(job));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job_guard.handle(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject 失败: {e}"))?;

        // 三根管道：stdin(父写子读) / stdout(子写父读) / stderr(子写父读)。
        let (stdin_read, stdin_write) = create_inheritable_pipe_pair()?;
        let (stdout_read, stdout_write) = create_inheritable_pipe_pair()?;
        let (stderr_read, stderr_write) = create_inheritable_pipe_pair()?;
        for parent_end in [stdin_write.handle(), stdout_read.handle(), stderr_read.handle()] {
            SetHandleInformation(parent_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                .map_err(|e| format!("SetHandleInformation(父端不可继承) 失败: {e}"))?;
        }

        // 白名单只含三条子端句柄；未列出的可继承句柄不会进入子进程。
        let attributes_count = if matches!(spec.identity, StdioIdentity::AppContainer { .. }) {
            3
        } else {
            2
        };
        let mut attributes = AttrList::new(attributes_count)?;
        attributes.set_handle_list(vec![
            stdin_read.handle(),
            stdout_write.handle(),
            stderr_write.handle(),
        ])?;
        attributes.set_job(job_guard.handle())?;
        if let StdioIdentity::AppContainer { sid, capabilities } = spec.identity {
            attributes.set_security_capabilities(sid, capabilities)?;
        }

        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin_read.handle();
        startup.StartupInfo.hStdOutput = stdout_write.handle();
        startup.StartupInfo.hStdError = stderr_write.handle();
        startup.lpAttributeList = attributes.as_mut_ptr();

        // 环境块是 UTF-16：必须配 CREATE_UNICODE_ENVIRONMENT，否则系统按 ANSI
        // 解析这个指针，CreateProcess 直接返回 ERROR_INVALID_PARAMETER。
        let flags = PROCESS_CREATION_FLAGS(
            CREATE_NO_WINDOW.0 | EXTENDED_STARTUPINFO_PRESENT.0 | CREATE_UNICODE_ENVIRONMENT.0,
        );
        let mut process_info = PROCESS_INFORMATION::default();
        let environment_ptr = environment_block.as_ptr().cast();
        let created = match token.as_ref() {
            Some(token) => CreateProcessAsUserW(
                Some(token.handle()),
                PCWSTR(application_wide.as_ptr()),
                Some(PWSTR(command_line_wide.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(environment_ptr),
                PCWSTR(cwd_wide.as_ptr()),
                &startup.StartupInfo,
                &mut process_info,
            )
            .map_err(|e| {
                format!(
                    "CreateProcessAsUserW 失败（{mode}，program={}，cwd={}）: {e}",
                    resolved.display(),
                    spec.cwd.display()
                )
            }),
            None => CreateProcessW(
                PCWSTR(application_wide.as_ptr()),
                Some(PWSTR(command_line_wide.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(environment_ptr),
                PCWSTR(cwd_wide.as_ptr()),
                &startup.StartupInfo,
                &mut process_info,
            )
            .map_err(|e| {
                format!(
                    "CreateProcessW 失败（{mode}，program={}，cwd={}）: {e}",
                    resolved.display(),
                    spec.cwd.display()
                )
            }),
        };
        created?;

        let _ = CloseHandle(process_info.hThread);
        let raw_process: RawHandle = process_info.hProcess.0.cast();
        let process = OwnedHandle::from_raw_handle(raw_process);
        let pid = process_info.dwProcessId;

        // 父侧关闭子端；子端在进程创建后由内核持有一份，不会因这里 drop 而断管。
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);

        let stdin = Some(stdin_write.into_file());
        let stderr_tail = Arc::new(Mutex::new(StderrTail::new(STDERR_TAIL_CAPACITY)));
        spawn_stdout_reader(stdout_read.into_file(), tx, spec.max_line_bytes);
        spawn_stderr_reader(stderr_read.into_file(), Arc::clone(&stderr_tail));

        Ok(RestrictedStdioChild {
            process,
            _job: job_guard,
            stdin,
            stderr_tail,
            pid,
        })
    }
}

/// 以「删除全部特权的受限令牌 + Job Object + 句柄白名单」启动 MCP stdio server。
pub fn spawn_restricted_stdio(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<RestrictedStdioChild, String> {
    spawn_stdio(
        &StdioSpawnSpec {
            program,
            args,
            cwd,
            env,
            identity: StdioIdentity::RestrictedToken,
            max_line_bytes: MAX_MCP_STDIO_LINE_BYTES,
        },
        tx,
    )
}

/// 以「AppContainer + 删除全部特权的受限令牌 + Job Object + 句柄白名单」启动 MCP stdio server。
///
/// `capabilities` 为空 = 不授予 `internetClient`，出站网络由容器边界默认拒绝；
/// 只在会话级网络授权批准时传 `internet_client_capability()`。
/// 目录访问由 `acl` 按容器 SID 授权，未授权的路径在容器内不可读写。
pub fn spawn_appcontainer_stdio(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    app_container_sid: PSID,
    capabilities: &[PSID],
    tx: mpsc::Sender<StdioEvent>,
) -> Result<RestrictedStdioChild, String> {
    spawn_stdio(
        &StdioSpawnSpec {
            program,
            args,
            cwd,
            env,
            identity: StdioIdentity::AppContainer {
                sid: app_container_sid,
                capabilities,
            },
            max_line_bytes: MAX_MCP_STDIO_LINE_BYTES,
        },
        tx,
    )
}

/// 逐服务器显式豁免沙箱的启动路径（X10：用户主动选择、记录在案、界面可见）。
/// 仍走句柄白名单 + Job（防泄漏/防孤儿），只是不创建受限令牌。
pub fn spawn_unsandboxed_stdio(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<RestrictedStdioChild, String> {
    spawn_stdio(
        &StdioSpawnSpec {
            program,
            args,
            cwd,
            env,
            identity: StdioIdentity::Unsandboxed,
            max_line_bytes: MAX_MCP_STDIO_LINE_BYTES,
        },
        tx,
    )
}

#[cfg(test)]
fn spawn_restricted_stdio_with_max_line(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    tx: mpsc::Sender<StdioEvent>,
    max_line_bytes: usize,
) -> Result<RestrictedStdioChild, String> {
    spawn_stdio(
        &StdioSpawnSpec {
            program,
            args,
            cwd,
            env,
            identity: StdioIdentity::RestrictedToken,
            max_line_bytes,
        },
        tx,
    )
}

#[cfg(test)]
mod stdio_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    /// 子进程重入后用到的完整测试名（libtest `--exact`）。
    const PROBE_TEST: &str = "sandbox::windows_impl::stdio_tests::handle_probe_child";

    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-stdio-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 收集事件直到通道断开或超时（不用 blocking_recv，避免测试卡死）。
    fn drain(
        rx: &mut mpsc::Receiver<StdioEvent>,
        max_wait: Duration,
    ) -> (Vec<String>, Vec<String>) {
        let deadline = Instant::now() + max_wait;
        let mut lines = Vec::new();
        let mut errors = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(StdioEvent::Line(line)) => lines.push(line),
                Ok(StdioEvent::ProtocolError(error)) => errors.push(error),
                Err(mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        (lines, errors)
    }

    fn process_alive(pid: u32) -> bool {
        unsafe {
            match OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            ) {
                Ok(handle) => {
                    let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
                    let _ = CloseHandle(handle);
                    alive
                }
                Err(_) => false,
            }
        }
    }

    // ------------------------------ 纯单元 ------------------------------

    #[test]
    fn stderr_tail_keeps_only_recent_bytes() {
        let mut tail = StderrTail::new(16);
        tail.push("0123456789");
        tail.push("abcdefghij");
        assert_eq!(tail.text, "456789abcdefghij");
        assert_eq!(tail.dropped_bytes, 4);
        // 再推入超过容量的内容：只保留尾部，且丢弃计数继续累加
        tail.push("XYZ");
        assert_eq!(tail.text, "789abcdefghijXYZ");
        assert_eq!(tail.dropped_bytes, 7);
    }

    #[test]
    fn quoted_args_follow_command_line_rules() {
        let mut line = String::new();
        append_quoted_arg(&mut line, "plain");
        append_quoted_arg(&mut line, "a b");
        append_quoted_arg(&mut line, "C:\\dir with space\\");
        append_quoted_arg(&mut line, "say \"hi\"");
        assert_eq!(
            line,
            "plain\"a b\"\"C:\\dir with space\\\\\"\"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn script_program_is_rejected_with_actionable_error() {
        let dir = temp_dir();
        let (tx, _rx) = mpsc::channel(4);
        let err = match spawn_restricted_stdio("npx.cmd", &[], &dir, &BTreeMap::new(), tx) {
            Err(error) => error,
            Ok(_) => panic!("脚本型可执行文件应被拒绝"),
        };
        assert!(err.contains("C-R5"), "错误应说明拒绝依据: {err}");
        assert!(err.contains("npx.cmd"), "错误应包含原始命令: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_program_error_lists_searched_dirs() {
        let dir = temp_dir();
        let (tx, _rx) = mpsc::channel(4);
        let err = match spawn_restricted_stdio(
            "shaniu-no-such-exe-xyz",
            &[],
            &dir,
            &BTreeMap::new(),
            tx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("不存在的可执行文件应启动失败"),
        };
        assert!(err.contains("shaniu-no-such-exe-xyz"), "{err}");
        assert!(err.contains("已搜索"), "错误应说明搜索范围: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------ 真机 ------------------------------

    #[test]
    fn env_whitelist_blocks_parent_sentinel() {
        let sentinel = "SHANIU_MCP_SENTINEL_0420";
        unsafe { std::env::set_var(sentinel, "leak-me") };
        let dir = temp_dir();
        let script = format!("echo [%{sentinel}%]");
        let args = ["/C", script.as_str()];
        let (tx, mut rx) = mpsc::channel(64);
        let child = spawn_restricted_stdio("cmd", &args, &dir, &BTreeMap::new(), tx).unwrap();
        let (lines, errors) = drain(&mut rx, Duration::from_secs(15));
        unsafe { std::env::remove_var(sentinel) };
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!lines.is_empty(), "没有收到子进程输出");
        assert!(
            lines.iter().all(|line| !line.contains("leak-me")),
            "父进程哨兵变量泄漏进 server 环境: {lines:?}"
        );
        drop(child);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_whitelist_keeps_systemroot_for_runtimes() {
        let dir = temp_dir();
        let args = ["/C", "echo [%SystemRoot%]"];
        let (tx, mut rx) = mpsc::channel(64);
        let child = spawn_restricted_stdio("cmd", &args, &dir, &BTreeMap::new(), tx).unwrap();
        let (lines, errors) = drain(&mut rx, Duration::from_secs(15));
        assert!(errors.is_empty(), "{errors:?}");
        let line = lines
            .iter()
            .find(|line| line.starts_with('['))
            .unwrap_or_else(|| panic!("没有收到 SystemRoot 输出: {lines:?}"));
        assert_ne!(line.trim(), "[%SystemRoot%]", "SystemRoot 没有进入白名单");
        assert!(line.trim().len() > 2, "SystemRoot 为空: {line:?}");
        drop(child);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn argv_with_spaces_is_not_split() {
        let dir = temp_dir();
        let bat = dir.join("probe.bat");
        std::fs::write(&bat, "@echo off\r\necho [%~1]\r\n").unwrap();
        let script = bat.display().to_string();
        let args = ["/C", script.as_str(), "a b"];
        let (tx, mut rx) = mpsc::channel(64);
        let child = spawn_restricted_stdio("cmd", &args, &dir, &BTreeMap::new(), tx).unwrap();
        let (lines, errors) = drain(&mut rx, Duration::from_secs(15));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            lines.iter().any(|line| line.trim() == "[a b]"),
            "含空格的 argv 被拆开了: {lines:?}"
        );
        drop(child);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsandboxed_stdio_runs_and_reports_output() {
        let dir = temp_dir();
        let args = ["/C", "echo unsandboxed-ok"];
        let (tx, mut rx) = mpsc::channel(64);
        let child =
            spawn_unsandboxed_stdio("cmd", &args, &dir, &BTreeMap::new(), tx).unwrap();
        let (lines, errors) = drain(&mut rx, Duration::from_secs(15));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            lines.iter().any(|line| line.contains("unsandboxed-ok")),
            "显式豁免路径没有读到输出: {lines:?}"
        );
        drop(child);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_list_blocks_unlisted_inheritable_handle() {
        // 父进程刻意留一个可继承句柄：没有 HANDLE_LIST 时它会被子进程继承。
        let (probe_read, probe_write) = unsafe { create_inheritable_pipe_pair().unwrap() };
        let raw = probe_write.handle().0 as usize;
        let mut env = BTreeMap::new();
        env.insert("SHANIU_STDIO_PROBE_HANDLE".to_string(), raw.to_string());

        let exe = std::env::current_exe().unwrap();
        let args = ["--exact", PROBE_TEST, "--nocapture"];
        let (tx, mut rx) = mpsc::channel(256);
        let child = spawn_restricted_stdio(
            exe.to_str().expect("测试可执行文件路径应为 UTF-8"),
            &args,
            &std::env::temp_dir(),
            &env,
            tx,
        )
        .unwrap();

        let (lines, errors) = drain(&mut rx, Duration::from_secs(60));
        assert!(errors.is_empty(), "{errors:?}");
        let probe = lines
            .iter()
            .find(|line| line.contains("PROBE:"))
            .unwrap_or_else(|| panic!("子进程没有输出探针结果: {lines:?}"));
        assert!(
            probe.contains("BLOCKED"),
            "白名单之外的可继承句柄泄漏进子进程: {probe}（全部输出 {lines:?}）"
        );
        drop(child);
        drop(probe_read);
        drop(probe_write);
    }

    #[test]
    fn line_over_limit_reports_protocol_error_and_stops() {
        let dir = temp_dir();
        let long_line = "0123456789012345678901234567890123456789";
        let script = format!("echo {long_line}");
        let args = ["/C", script.as_str()];
        let (tx, mut rx) = mpsc::channel(64);
        let child = spawn_restricted_stdio_with_max_line(
            "cmd",
            &args,
            &dir,
            &BTreeMap::new(),
            tx,
            16,
        )
        .unwrap();
        let (lines, errors) = drain(&mut rx, Duration::from_secs(15));
        assert!(
            errors.iter().any(|error| error.contains("16")),
            "超过单行上限必须报协议错误: lines={lines:?} errors={errors:?}"
        );
        child.kill();
        drop(child);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_probe_child() {
        // 仅当作为探针子进程被启动时执行；正常测试运行时变量不存在，直接返回。
        let Ok(raw) = std::env::var("SHANIU_STDIO_PROBE_HANDLE") else {
            return;
        };
        let value = raw.parse::<usize>().unwrap_or(0);
        let mut duplicate = HANDLE::default();
        let leaked = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(value as *mut core::ffi::c_void),
                GetCurrentProcess(),
                &mut duplicate,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .is_ok();
        if leaked {
            unsafe {
                let _ = CloseHandle(duplicate);
            }
        }
        println!("PROBE:{}", if leaked { "LEAKED" } else { "BLOCKED" });
    }

    // ------------------------------ AppContainer stdio（SH-064 5a） ------------------------------

    /// 容器 + 已按容器 SID 授权的工作目录。
    ///
    /// `cmd` 的内建命令会校验当前目录是否可访问（否则报「当前目录无效。」），
    /// 所以 cwd 必须先有容器 SID 的 ACE——这本身也是 5b「按 (path,SID) 授权」的输入。
    fn container_fixture(
        tag: &str,
    ) -> (
        crate::sandbox::appcontainer::AppContainer,
        PathBuf,
        crate::sandbox::acl::AclManager,
    ) {
        container_fixture_with(tag, crate::sandbox::acl::Access::Write)
    }

    fn container_fixture_with(
        tag: &str,
        access: crate::sandbox::acl::Access,
    ) -> (
        crate::sandbox::appcontainer::AppContainer,
        PathBuf,
        crate::sandbox::acl::AclManager,
    ) {
        let name = format!("Shaniu.Mcp.Test.{}.{}", std::process::id(), tag);
        let container = crate::sandbox::appcontainer::AppContainer::ensure_named(&name)
            .expect("测试用 AppContainer profile 应可创建");
        let dir = temp_dir();
        let mut acl = crate::sandbox::acl::AclManager::new(dir.join(".acl-backup"));
        acl.grant(&dir, container.sid(), access)
            .expect("应能给测试目录授予容器 SID 的 ACE");
        (container, dir, acl)
    }

    fn container_teardown(
        container: crate::sandbox::appcontainer::AppContainer,
        dir: PathBuf,
        mut acl: crate::sandbox::acl::AclManager,
    ) {
        if let Err(error) = acl.restore_all() {
            eprintln!("清理测试目录 DACL 失败: {error}");
        }
        if let Err(error) = container.delete() {
            eprintln!("清理测试 profile 失败: {error}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn appcontainer_stdio_round_trips_stdin() {
        use std::io::Write as _;
        let (container, dir, acl) = container_fixture("stdin");
        let (tx, mut rx) = mpsc::channel(64);
        let mut child = spawn_appcontainer_stdio(
            "cmd",
            &["/C", "sort"],
            &dir,
            &BTreeMap::new(),
            container.sid(),
            &[],
            tx,
        )
        .expect("容器内应能启动 sort");
        let mut stdin = child.take_stdin().expect("应有 stdin 写端");
        stdin.write_all(b"beta\nalpha\n").unwrap();
        stdin.flush().unwrap();
        // 关写端 → sort 收到 EOF 才输出。
        drop(stdin);
        let (lines, errors) = drain(&mut rx, Duration::from_secs(30));
        let (stderr, _) = child.stderr_tail();
        drop(child);
        container_teardown(container, dir, acl);
        assert!(errors.is_empty(), "{errors:?}");
        let body: Vec<&str> = lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(body, vec!["alpha", "beta"], "stdin 往返结果不符: {lines:?}；stderr={stderr}");
    }

    #[test]
    fn appcontainer_job_close_kills_child() {
        let (container, dir, acl) = container_fixture("job");
        let args = ["/C", "ping -n 60 127.0.0.1 >nul"];
        let (tx, _rx) = mpsc::channel(16);
        let child =
            spawn_appcontainer_stdio("cmd", &args, &dir, &BTreeMap::new(), container.sid(), &[], tx)
                .expect("容器内应能启动 cmd");
        let pid = child.pid();
        assert!(process_alive(pid), "子进程一开始就不在运行: {pid}");
        drop(child); // Job 句柄关闭 → KILL_ON_JOB_CLOSE 收树
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && process_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let alive = process_alive(pid);
        container_teardown(container, dir, acl);
        assert!(!alive, "Job 关闭后容器内子进程 {pid} 仍在运行（孤儿）");
    }

    /// 只读档：能列能读，写入必须失败（Q2 默认姿态）。
    #[test]
    fn appcontainer_read_only_grant_allows_read_but_denies_write() {
        let (container, dir, acl) = container_fixture_with(
            "readonly",
            crate::sandbox::acl::Access::Read,
        );
        std::fs::write(dir.join("probe.txt"), b"hello-leaf").unwrap();

        let reading = run_container_cmd(container.sid(), &dir, &["/C", "type probe.txt"]);
        let writing = run_container_cmd(container.sid(), &dir, &["/C", "echo nope> denied.txt"]);
        println!("[只读] 读文件 {reading:?}");
        println!("[只读] 写文件 {writing:?}");

        assert_eq!(reading.2, 0, "只读档应能读: {reading:?}");
        assert!(reading.0.contains("hello-leaf"));
        assert_ne!(writing.2, 0, "只读档不应能写: {writing:?}");
        assert!(!dir.join("denied.txt").exists(), "只读档下容器写入了文件");

        container_teardown(container, dir, acl);
    }

    /// 在容器里跑一条 `cmd` 命令，返回（stdout, stderr, 退出码）。
    fn run_container_cmd(sid: PSID, cwd: &Path, args: &[&str]) -> (String, String, i32) {
        let (tx, mut rx) = mpsc::channel(64);
        let mut child =
            match spawn_appcontainer_stdio("cmd", args, cwd, &BTreeMap::new(), sid, &[], tx) {
                Ok(child) => child,
                // 启动失败（例如 cwd 连打开都不允许）：直接用错误文本当 stderr。
                Err(error) => return (String::new(), error, -2),
            };
        let (lines, errors) = drain(&mut rx, Duration::from_secs(30));
        let (stderr, _) = child.stderr_tail();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 tokio runtime");
        let code = runtime.block_on(child.wait()).unwrap_or(-3);
        (lines.join("\n"), format!("{stderr}{}", errors.join("\n")), code)
    }

    /// 5b 决定性验证：**只**给叶子目录加 ACE → 容器内能列、能读、能写 → 撤销后立刻不能。
    ///
    /// 列表用 `for %f in (*)` 而不是 `dir`：`dir` 在 AppContainer 内依赖卷信息，
    /// 即使 ACL 正确也报「拒绝访问。」（实测 `dir C:\Windows` 同样失败），
    /// 生产路径已有宿主侧列举兜底（见 unified_exec 的 dir_listing）。
    #[test]
    fn appcontainer_leaf_grant_allows_list_read_write_then_revoke_denies() {
        let (container, dir, mut acl) = container_fixture("leafio");
        std::fs::write(dir.join("probe.txt"), b"hello-leaf").unwrap();

        let listing = run_container_cmd(container.sid(), &dir, &["/C", "for %f in (*) do @echo %f"]);
        let reading = run_container_cmd(container.sid(), &dir, &["/C", "type probe.txt"]);
        let writing = run_container_cmd(container.sid(), &dir, &["/C", "echo written> out.txt"]);
        let written = std::fs::read_to_string(dir.join("out.txt")).ok();
        println!("[授权后] 列目录 {listing:?}");
        println!("[授权后] 读文件 {reading:?}");
        println!("[授权后] 写文件 {writing:?} → 宿主读到 {written:?}");

        assert_eq!(listing.2, 0, "容器内列目录失败: {listing:?}");
        assert!(
            listing.0.contains("probe.txt"),
            "容器内列目录看不到文件: {listing:?}"
        );
        assert_eq!(reading.2, 0, "容器内读文件失败: {reading:?}");
        assert!(reading.0.contains("hello-leaf"), "读取内容不符: {reading:?}");
        assert_eq!(writing.2, 0, "容器内写文件失败: {writing:?}");
        assert_eq!(written.as_deref(), Some("written\r\n"), "宿主看不到容器写入的内容");

        // 撤销 (路径, SID) → 立刻不能读、不能写、不能列。
        assert!(acl.revoke(&dir, container.sid()).expect("撤销应成功"));
        let reading_after = run_container_cmd(container.sid(), &dir, &["/C", "type probe.txt"]);
        let writing_after = run_container_cmd(container.sid(), &dir, &["/C", "echo nope> denied.txt"]);
        let listing_after = run_container_cmd(container.sid(), &dir, &["/C", "for %f in (*) do @echo %f"]);
        println!("[撤销后] 读文件 {reading_after:?}");
        println!("[撤销后] 写文件 {writing_after:?}");
        println!("[撤销后] 列目录 {listing_after:?}");

        assert_ne!(reading_after.2, 0, "撤销后容器内仍能读: {reading_after:?}");
        assert!(
            !reading_after.0.contains("hello-leaf"),
            "撤销后仍读到内容: {reading_after:?}"
        );
        assert_ne!(writing_after.2, 0, "撤销后容器内仍能写: {writing_after:?}");
        assert!(!dir.join("denied.txt").exists(), "撤销后容器仍写入了文件");
        // `for` 会吞掉枚举错误：撤销后退出码仍可能是 0，但目录对容器变成「空」。
        assert!(
            !listing_after.0.contains("probe.txt"),
            "撤销后容器内仍能列目录: {listing_after:?}"
        );

        container_teardown(container, dir, acl);
    }

    // 句柄白名单与 argv 逐项转义的容器版验证延到 5d：
    //  · 该代码路径与受限令牌路径共用（`set_handle_list`），已由既有用例覆盖。
    //  · argv 转义是 `build_command_line` 的纯函数，已由 `quoted_args_follow_command_line_rules`
    //    锁定；容器路径复用它，没有第二套拼装逻辑。
    // 真机（5d）补容器内越界读写、断网/放行与 DACL 精确撤销。

    /// 真机：父进程关闭 Job 句柄后，server 派生的孙进程也必须消失。
    #[ignore = "真机用例：需要 PowerShell；验证 Job 关闭即杀树"]
    #[test]
    fn job_close_kills_process_tree() {
        let dir = temp_dir();
        let script = "$p = Start-Process -FilePath cmd -ArgumentList '/C','ping -n 60 127.0.0.1 >nul' -PassThru -WindowStyle Hidden; Write-Output $p.Id; Start-Sleep -Seconds 60";
        let args = ["-NoProfile", "-NonInteractive", "-Command", script];
        let (tx, mut rx) = mpsc::channel(64);
        let child = spawn_restricted_stdio("powershell", &args, &dir, &BTreeMap::new(), tx).unwrap();
        // 只等第一行 PID（PowerShell 随后还要 sleep 60s，不能等通道断开）。
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut grandchild: Option<u32> = None;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(StdioEvent::Line(line)) => {
                    if let Ok(pid) = line.trim().parse() {
                        grandchild = Some(pid);
                        break;
                    }
                }
                Ok(StdioEvent::ProtocolError(error)) => panic!("协议错误: {error}"),
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        let grandchild = grandchild.unwrap_or_else(|| panic!("没有拿到孙进程 PID"));
        assert!(process_alive(grandchild), "孙进程一开始就不在运行: {grandchild}");

        // 关闭 Job 句柄 → KILL_ON_JOB_CLOSE 收掉整棵树。
        drop(child);
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !process_alive(grandchild),
            "Job 关闭后孙进程 {grandchild} 仍在运行（孤儿进程）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}


