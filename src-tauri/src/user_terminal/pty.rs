//! 用户终端的 ConPTY 原语（SH-071）。
//!
//! 与 `sandbox/windows_impl.rs` 的持久终端**刻意分家**：那条路径属于 agent 的
//! 执行面，带令牌/AppContainer/审批语义；用户终端是用户自己的窗口，必须裸进程
//! 起——不建受限令牌、不挂 AppContainer、不经审批。两份代码都只做 Win32 绑定，
//! 共享的只有内核对象用法（伪控制台 + Job），刻意不抽公共层，避免以后有人把
//! 沙箱档位顺手接进用户终端。
//!
//! 唯一的"约束"是 Job 的 `KILL_ON_JOB_CLOSE`：它不限制用户能做什么，只保证
//! 应用退出/崩溃时整棵子进程树被内核回收，不留孤儿。

use std::ffi::OsStr;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(test)]
use windows::Win32::System::JobObjects::{
    JobObjectBasicAccountingInformation, QueryInformationJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, WaitForSingleObject,
};

/// 内核句柄 RAII 包装：句柄无线程亲和性，跨线程关闭是常规用法（同 `OwnedHandle`）。
struct HandleGuard(HANDLE);

unsafe impl Send for HandleGuard {}
unsafe impl Sync for HandleGuard {}

impl HandleGuard {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn handle(&self) -> HANDLE {
        self.0
    }

    fn into_file(self) -> std::fs::File {
        let raw: RawHandle = self.0 .0.cast();
        std::mem::forget(self);
        unsafe { std::fs::File::from_raw_handle(raw) }
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Job 句柄守卫：`KILL_ON_JOB_CLOSE` 让整棵树在句柄关闭（应用退出/崩溃）时被回收。
pub struct JobGuard(HandleGuard);

impl JobGuard {
    fn handle(&self) -> HANDLE {
        self.0.handle()
    }
}

/// 伪控制台守卫：失败路径上自动关闭，成功路径 `take` 后交给 `Child`。
struct PseudoConsoleGuard(Option<HPCON>);

impl PseudoConsoleGuard {
    fn new(hpc: HPCON) -> Self {
        Self(Some(hpc))
    }

    fn take(mut self) -> HPCON {
        self.0.take().expect("伪控制台刚由 new 写入")
    }
}

impl Drop for PseudoConsoleGuard {
    fn drop(&mut self) {
        if let Some(hpc) = self.0.take() {
            unsafe { ClosePseudoConsole(hpc) };
        }
    }
}

/// 已启动的用户终端会话。
pub struct Child {
    process: OwnedHandle,
    /// 伪控制台句柄存成整数（`HPCON` 就是 `isize`）：包进 `Mutex` 后仍满足 `Sync`；
    /// 所有权归本结构体，只在 `close` 里还原成句柄关闭。
    hpc: Mutex<Option<isize>>,
    /// 只为 `Drop` 活着：句柄关闭时内核回收整棵进程树（含应用崩溃）。
    _job: JobGuard,
}

impl Child {
    /// 作业内活动进程数：0 = 整树已退出；用于「关闭后不留孤儿」的断言。
    #[cfg(test)]
    pub fn active_processes(&self) -> u32 {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let result = unsafe {
            QueryInformationJobObject(
                Some(self._job.handle()),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut core::ffi::c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        };
        if result.is_err() {
            return 0;
        }
        info.ActiveProcesses
    }

    /// 复制一份进程句柄给读线程做退出码探测（句柄本身无线程亲和性）。
    pub fn clone_process(&self) -> io::Result<OwnedHandle> {
        self.process.try_clone()
    }

    /// 调整伪控制台窗口尺寸。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let handle = self
            .hpc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|raw| HPCON(*raw))
            .ok_or_else(|| String::from("终端已关闭"))?;
        unsafe {
            windows::Win32::System::Console::ResizePseudoConsole(
                handle,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
        }
        .map_err(|error| format!("ResizePseudoConsole 失败: {error}"))
    }

    /// 终止 shell 再关伪控制台：顺序反了会让 `ClosePseudoConsole` 长时间等待。
    pub fn close(&self) {
        use std::os::windows::io::AsRawHandle as _;
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(
                HANDLE(self.process.as_raw_handle()),
                1,
            );
            let raw = self
                .hpc
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(raw) = raw {
                ClosePseudoConsole(HPCON(raw));
            }
        }
    }
}

/// 读线程用：把进程句柄的退出码读出来（仍在运行返回 None）。
pub fn exit_code_of(process: &OwnedHandle) -> Option<u32> {
    use std::os::windows::io::AsRawHandle as _;
    const STILL_ACTIVE: u32 = 259;
    let mut code = 0u32;
    unsafe {
        windows::Win32::System::Threading::GetExitCodeProcess(
            HANDLE(process.as_raw_handle()),
            &mut code,
        )
    }
    .ok()?;
    (code != STILL_ACTIVE).then_some(code)
}

/// 阻塞直到进程退出。
///
/// 用户自己 `exit` 时输出管道不保证 EOF（伪控制台主机可能继续持有写端），
/// 读线程会一直阻塞在 `read` 上；判断"进程没了"必须看进程句柄。
pub fn wait_until_exit(process: &OwnedHandle) {
    use std::os::windows::io::AsRawHandle as _;
    unsafe {
        WaitForSingleObject(HANDLE(process.as_raw_handle()), INFINITE);
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.close();
    }
}

/// 父侧端点：`reader` 读终端输出，`writer` 写终端输入。
pub struct Spawn {
    pub child: Child,
    pub reader: std::fs::File,
    pub writer: std::fs::File,
}

/// 启动一个裸进程伪控制台会话（无令牌、无容器、无审批）。
///
/// - `program`：可执行文件路径，支持含空格的绝对路径；
/// - `args`：按 Windows 命令行引用规则逐个转义；
/// - `cwd`：起始目录，调用方负责校验存在性；
/// - `cols` / `rows`：初始窗口尺寸。
pub fn spawn(
    program: &str,
    args: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
) -> Result<Spawn, String> {
    let command_line = build_command_line(program, args);
    let application_wide = wide(OsStr::new(program));
    let mut command_line_wide = wide(OsStr::new(&command_line));
    let cwd_wide = wide(cwd.as_os_str());

    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("CreateJobObject 失败: {e}"))?;
        let job_guard = JobGuard(HandleGuard::new(job));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job_guard.handle(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject 失败: {e}"))?;

        // 伪控制台两对端点都不继承：子进程的 stdio 由伪控制台注入，
        // `bInheritHandles = FALSE` 是 node-pty `conpty.cc` 的硬要求。
        let (pty_input_read, pty_input_write) = create_pipe_pair(false)?;
        let (pty_output_read, pty_output_write) = create_pipe_pair(false)?;

        let size = COORD {
            X: cols.max(1) as i16,
            Y: rows.max(1) as i16,
        };
        let console = PseudoConsoleGuard::new(
            CreatePseudoConsole(size, pty_input_read.handle(), pty_output_write.handle(), 0)
                .map_err(|e| format!("CreatePseudoConsole 失败: {e}"))?,
        );

        // 托管端点必须活到 CreateProcess 返回之后：提前关闭会让客户端在
        // DLL 初始化阶段退出（0xC0000142）。
        let mut attributes = AttrList::new(2)?;
        attributes.set_job(job_guard.handle())?;
        attributes.set_pseudoconsole(console.0.expect("伪控制台刚创建，尚未移交"))?;

        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes.as_mut_ptr();
        startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = HANDLE::default();
        startup.StartupInfo.hStdOutput = HANDLE::default();
        startup.StartupInfo.hStdError = HANDLE::default();

        // 无环境块：用户 shell 继承宿主环境（PATH/用户变量都要原样可见）。
        let flags = PROCESS_CREATION_FLAGS(EXTENDED_STARTUPINFO_PRESENT.0);
        let mut process_info = PROCESS_INFORMATION::default();
        CreateProcessW(
            PCWSTR(application_wide.as_ptr()),
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            None,
            PCWSTR(cwd_wide.as_ptr()),
            &startup.StartupInfo,
            &mut process_info,
        )
        .map_err(|e| {
            format!(
                "CreateProcessW(用户终端) 失败（program={program}，cwd={}）: {e}",
                cwd.display()
            )
        })?;

        let _ = CloseHandle(process_info.hThread);
        let process = OwnedHandle::from_raw_handle(process_info.hProcess.0.cast());

        Ok(Spawn {
            child: Child {
                process,
                hpc: Mutex::new(Some(console.take().0)),
                _job: job_guard,
            },
            reader: pty_output_read.into_file(),
            writer: pty_input_write.into_file(),
        })
    }
}

/// 创建一对不可继承的匿名管道。
unsafe fn create_pipe_pair(inheritable: bool) -> Result<(HandleGuard, HandleGuard), String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: inheritable.into(),
    };
    let mut read_end = HANDLE::default();
    let mut write_end = HANDLE::default();
    CreatePipe(&mut read_end, &mut write_end, Some(&sa), 0)
        .map_err(|e| format!("CreatePipe 失败: {e}"))?;
    if !inheritable {
        SetHandleInformation(read_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
            .map_err(|e| format!("SetHandleInformation(read) 失败: {e}"))?;
        SetHandleInformation(write_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
            .map_err(|e| format!("SetHandleInformation(write) 失败: {e}"))?;
    }
    Ok((HandleGuard::new(read_end), HandleGuard::new(write_end)))
}

/// 进程线程属性表：Job 与伪控制台两项。
struct AttrList {
    buffer: Vec<u8>,
    job_list: Vec<HANDLE>,
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
            job_list: Vec::new(),
        })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_mut_ptr().cast())
    }

    fn set_job(&mut self, job: HANDLE) -> Result<(), String> {
        self.job_list = vec![job];
        let list = self.as_mut_ptr();
        let value = self.job_list.as_ptr().cast();
        let size = size_of::<HANDLE>();
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

    /// 属性值就是 HPCON 本身（不是指向 HPCON 的指针）——传 `&hpc` 会 0xC0000142。
    fn set_pseudoconsole(&mut self, hpc: HPCON) -> Result<(), String> {
        let list = self.as_mut_ptr();
        unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const core::ffi::c_void),
                size_of::<HPCON>(),
                None,
                None,
            )
        }
        .map_err(|e| format!("UpdateProcThreadAttribute(PSEUDOCONSOLE) 失败: {e}"))
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}

/// 逐参转义（MSVCRT 规则）：空格/制表/引号触发加引号，反斜杠按后随字符加倍。
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn build_command_line(program: &str, args: &[String]) -> String {
    let mut line = quote_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}
