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

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;

use tokio::sync::mpsc;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    CreateRestrictedToken, GetTokenInformation, LUID_AND_ATTRIBUTES, SECURITY_ATTRIBUTES,
    SE_PRIVILEGE_REMOVED, TOKEN_ACCESS_MASK, TOKEN_PRIVILEGES, TokenPrivileges,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
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

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
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

/// std::thread 逐行读管道 → blocking_send 桥接 tokio 通道（codex read_handle_loop 同思路）。
fn spawn_pipe_reader(read: std::fs::File, channel: u8, tx: mpsc::Sender<ExecLine>) {
    std::thread::spawn(move || {
        let mut file = read;
        let mut line = String::new();
        loop {
            line.clear();
            {
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(&mut file);
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if tx.blocking_send((channel, trimmed)).is_err() {
                break;
            }
        }
    });
}
