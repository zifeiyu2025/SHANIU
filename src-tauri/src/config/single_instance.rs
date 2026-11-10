//! 单实例保护：两层，职责不同（2026-11-11）。
//!
//! **第一层 = 全局（本登录会话）唯一 UI 实例**（`GLOBAL_MUTEX_NAME`，固定名、不按
//! 数据目录派生）。第二次启动不是报错，而是把已运行的窗口叫到前台；指向不同
//! `data_root` 时明确告知"当前运行的是哪个数据目录"并给出退出的指引。
//!
//! **第二层 = 同 `<data_root>` 只允许一个写者**（`mutex_name`，按规范化路径 hash）。
//! 真源 `<data_root>/rollouts/*.jsonl` 是逐行 append + flush 的：两个进程同时写同一
//! 档案会把两条历史交错进同一个文件，产出的坏行即便被 `rollout::read` 跳过，历史本身
//! 也已经串了。
//!
//! 第二层在全局层存在之后仍然保留，理由要说准：`Local\` 命名空间是**按登录会话隔离**
//! 的，所以快速用户切换 / 远程桌面登录时全局层看不到另一个会话——但第二层同样看不到，
//! 它并不是"跨会话兜底"。它真正兜住的是**旁路入口**：不走 `run()` 的进程（测试、诊断
//! 工具、只带旧版逻辑的二进制）与全局层失效/改名后的写入冲突。跨会话真要互斥需要
//! `Global\` 命名空间（普通用户无创建特权）或独占锁文件，登记为后续项，不在这里假装。
//!
//! 用 Windows 命名互斥体而不是文件锁：互斥体随最后一个句柄关闭自动消失，进程被强杀
//! 也不会留下"谁也不认领"的锁文件；且创建/判定是一次原子调用，没有"先查后建"的竞态。
//! 数据目录层的名字里放 hash 而不是明文：互斥体名在同机进程间可见，不泄露本地路径。

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, IsWindowVisible, SetForegroundWindow, ShowWindow,
    ASFW_ANY, SW_RESTORE,
};

/// 全局层互斥体：固定应用标识，**不按数据目录派生**（整机/本登录会话只允许一个 UI）。
pub const GLOBAL_MUTEX_NAME: &str = "Local\\SHANIU-app";

/// 二次启动唤醒事件：命名自动复位事件，首实例等待、第二实例 SetEvent。
pub const ACTIVATE_EVENT_NAME: &str = "Local\\SHANIU-activate";

/// 首实例把"我正在用哪个数据目录"写在这里，供二次启动判断是否同一位置。
pub const RUNNING_INFO_FILE: &str = "running.json";
pub const RUNNING_INFO_VERSION: u32 = 1;

/// 持有期间即"本进程独占该 data_root"；`Drop` 关闭句柄，互斥体最后一个句柄消失即释放。
pub struct InstanceLock {
    handle: HANDLE,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 尝试独占 `data_root`。失败返回给用户看的说明（调用方负责弹窗并退出）。
pub fn acquire(data_root: &Path) -> Result<InstanceLock, String> {
    let name = HSTRING::from(mutex_name(data_root));
    let handle = unsafe { CreateMutexW(None, false, &name) }
        .map_err(|e| format!("创建数据目录互斥体失败: {e}"))?;
    // 必须紧接着读，`CreateMutexW` 返回成功同时也可能是"已存在"（此时我们只是拿到
    // 同一个对象的句柄，没有独占权）。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(format!(
            "这个数据目录已被另一个 SHANIU 实例占用：{}",
            data_root.display()
        ));
    }
    Ok(InstanceLock { handle })
}

/// 全局层锁：持有期间本进程是这台机器（本登录会话）唯一的 UI 实例。
pub struct GlobalLock {
    handle: HANDLE,
}

impl Drop for GlobalLock {
    fn drop(&mut self) {
        // 正常退出时抹掉"正在运行"记录，避免下次启动读到过期的数据目录。
        // 进程被强杀时记录会留下，但那时互斥体也已释放，下一次启动会直接覆盖它。
        let info_path = running_info_path();
        if read_running_info(&info_path).is_some_and(|info| info.pid == std::process::id()) {
            let _ = std::fs::remove_file(info_path);
        }
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 全局层结果：拿到锁，还是"已经有一个实例在运行"（后者不是错误，是要把窗口叫回来）。
pub enum GlobalAcquire {
    Acquired(GlobalLock),
    AlreadyRunning,
}

/// 获取全局层锁（产品入口，固定名）。
pub fn acquire_global() -> Result<GlobalAcquire, String> {
    acquire_global_named(GLOBAL_MUTEX_NAME)
}

/// 按指定名字获取全局层锁。名字参数只为测试隔离（真进程用例不能和本机正在跑的
/// SHANIU 抢同一把锁），产品路径一律走 [`GLOBAL_MUTEX_NAME`]。
pub fn acquire_global_named(name: &str) -> Result<GlobalAcquire, String> {
    let name = HSTRING::from(name);
    let handle = unsafe { CreateMutexW(None, false, &name) }
        .map_err(|e| format!("创建全局单实例互斥体失败: {e}"))?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(GlobalAcquire::AlreadyRunning);
    }
    Ok(GlobalAcquire::Acquired(GlobalLock { handle }))
}

/// 首实例的运行信息：二次启动据此判断"是不是同一个数据目录"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningInstance {
    pub version: u32,
    pub pid: u32,
    pub data_root: String,
    pub started_at_ms: u64,
}

impl RunningInstance {
    pub fn new(data_root: &Path) -> Self {
        Self {
            version: RUNNING_INFO_VERSION,
            pid: std::process::id(),
            data_root: data_root.display().to_string(),
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

/// 运行信息文件：与数据根记录同目录（`<用户目录>\SHANIU\data-root\running.json`）。
/// 和记录一样放用户目录而不是 exe 旁边——exe 旁边可能只读。
pub fn running_info_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(super::APP_DIR_NAME)
        .join(super::DATA_ROOT_RECORD_DIR)
        .join(RUNNING_INFO_FILE)
}

pub fn write_running_info(path: &Path, info: &RunningInstance) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建运行信息目录失败 {}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(info).map_err(|e| format!("序列化运行信息失败: {e}"))?;
    let tmp = super::temp_sibling(path);
    if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写运行信息失败 {}: {e}", path.display()));
    }
    Ok(())
}

pub fn read_running_info(path: &Path) -> Option<RunningInstance> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<RunningInstance>(&raw)
        .ok()
        .filter(|info| info.version == RUNNING_INFO_VERSION)
}

/// 数据目录不同时的说明——必须点名"当前运行的是哪个"，并给出可执行的下一步。
/// 返回 `None` = 同一目录（或读不到运行信息），安静地把窗口叫回来即可。
pub fn conflict_message(running: Option<&RunningInstance>, mine: &Path) -> Option<String> {
    let running = running?;
    if super::same_path(Path::new(&running.data_root), mine) {
        return None;
    }
    Some(format!(
        "已有一个 SHANIU 正在运行，但它用的是另一个数据目录。\n\n\
         当前运行的是：\n{}\n\n\
         这次启动要用的是：\n{}\n\n\
         同时打开会各自写各自的会话历史。要使用这次的数据目录，\
         请先退出正在运行的 SHANIU 再重新启动。",
        running.data_root,
        mine.display()
    ))
}

/// 二次启动的处理结果（调用方据此决定退出码/日志，测试据此断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondLaunchOutcome {
    /// 已通知首实例把窗口叫到前台，本进程可以退了。
    Activated,
    /// 数据目录不同：已向用户说明，并未激活（避免误以为新目录已生效）。
    DifferentDataRoot,
    /// 首实例在，但唤醒事件打不开（旧版本/初始化窗口）：已说明，请用户手动退出。
    CannotActivate,
}

/// 二次启动的收口：同一数据目录 → 请求首实例激活窗口；不同 → 说明后退出；都不再继续启动。
///
/// `event_name` 正常传 [`ACTIVATE_EVENT_NAME`]；测试传独立名字以免唤醒本机真实实例。
pub fn handle_second_launch(
    mine: &Path,
    info_path: &Path,
    event_name: &str,
    show_message: impl Fn(&str),
) -> SecondLaunchOutcome {
    let running = read_running_info(info_path);
    if let Some(message) = conflict_message(running.as_ref(), mine) {
        show_message(&message);
        return SecondLaunchOutcome::DifferentDataRoot;
    }
    if request_activation(event_name) {
        return SecondLaunchOutcome::Activated;
    }
    show_message(&format!(
        "检测到一个正在运行的 SHANIU，但没能把它的窗口叫出来。\n\n\
         当前运行的数据目录：{}\n\n\
         请先在任务栏/托盘里找到它（或结束该进程）再重新启动。",
        running
            .as_ref()
            .map(|info| info.data_root.clone())
            .unwrap_or_else(|| "（未能读取）".into())
    ));
    SecondLaunchOutcome::CannotActivate
}

/// 请求首实例把窗口叫到前台：打开发送端句柄 → 交出前台权 → 置事件。
pub fn request_activation(event_name: &str) -> bool {
    let name = HSTRING::from(event_name);
    let Ok(handle) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) }) else {
        return false;
    };
    unsafe {
        // 二次启动进程此刻持有前台权，显式交出去，首实例的 SetForegroundWindow 才不会被
        // 系统的前台锁拦下（否则窗口只闪一下任务栏按钮）。
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let ok = SetEvent(handle).is_ok();
        let _ = CloseHandle(handle);
        ok
    }
}

/// 首实例注册激活监听：后台线程等命名事件，收到就调用 `on_activate`。
/// 线程随进程退出而结束（句柄由 OS 回收），不需要显式停止。
pub fn spawn_activation_listener(
    event_name: &str,
    mut on_activate: impl FnMut() + Send + 'static,
) -> Result<(), String> {
    let name = HSTRING::from(event_name);
    let handle = unsafe { CreateEventW(None, false, false, &name) }
        .map_err(|e| format!("创建激活事件失败: {e}"))?;
    let raw = handle.0 as isize;
    let spawned = std::thread::Builder::new()
        .name("shaniu-activate".into())
        .spawn(move || {
            let handle = HANDLE(raw as *mut c_void);
            // 自动复位事件：每次 SetEvent 唤醒一次；等待失败（句柄失效）就退出。
            while let WAIT_OBJECT_0 = unsafe { WaitForSingleObject(handle, INFINITE) } {
                on_activate();
            }
            unsafe {
                let _ = CloseHandle(handle);
            }
        });
    if let Err(e) = spawned {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(format!("启动激活监听线程失败: {e}"));
    }
    Ok(())
}

/// 把窗口从隐藏/最小化恢复到前台，返回恢复后是否可见。
///
/// 传裸指针而不是 `HWND`：tauri 自带的 `windows` 版本可能与本 crate 不同，裸指针
/// 两边都能转。`SW_RESTORE` 同时覆盖"最小化"与"隐藏（托盘态）"两种状态。
pub fn restore_window(hwnd: *mut c_void) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let hwnd = HWND(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        IsWindowVisible(hwnd).as_bool()
    }
}

/// 互斥体名：`Local\SHANIU-data-<规范化路径的 FNV-1a 64 位 hash>`。
///
/// `Local\` 而非 `Global\`：不需要额外特权，覆盖"同一用户开两份"这个真实场景；
/// 不同登录会话共用同一 data_root 本身就不是受支持的用法（append/SQLite 都撑不住）。
pub fn mutex_name(data_root: &Path) -> String {
    let normalized = normalize(data_root);
    let lowered = normalized.to_string_lossy().to_lowercase();
    // 按 UTF-16 取字节：Windows 路径语义以 UTF-16 为准，代理对也按同一形态入 hash。
    let mut bytes = Vec::with_capacity(lowered.len() * 2);
    for unit in lowered.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    format!("Local\\SHANIU-data-{:016x}", fnv1a_64(&bytes))
}

/// FNV-1a 64：六行的稳定 hash，替代不保证跨版本稳定的 `DefaultHasher`。
///
/// 放在本模块而不是 `config`：互斥体名与数据根记录都依赖它，但单实例这一批要能
/// 独立提交/回退，因此由最早落地的模块持有，后者复用。
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 规范化：先建目录（幂等，run() 随后也会建）再 canonicalize。
///
/// 必须先建：`canonicalize` 对不存在的路径会失败，而首次启动/新副本的 `data\`
/// 往往还不存在；两侧实例若一个走 canonicalize、另一个走词法兜底，算出的 hash
/// 不一致，锁就形同虚设。
fn normalize(data_root: &Path) -> PathBuf {
    let _ = std::fs::create_dir_all(data_root);
    let canonical = std::fs::canonicalize(data_root)
        .or_else(|_| std::path::absolute(data_root))
        .unwrap_or_else(|_| data_root.to_path_buf());
    let trimmed = canonical
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_string();
    PathBuf::from(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    /// 同目录的不同写法必须命中同一把锁；不同目录必须不同（否则多副本被误拦）。
    #[test]
    fn mutex_name_is_per_data_root_and_spelling_insensitive() {
        let dir = TestTempDir::new("instance-name");
        let plain = mutex_name(&dir);
        let trailing = mutex_name(Path::new(&format!("{}\\", dir.display())));
        let upper = mutex_name(Path::new(&dir.display().to_string().to_uppercase()));
        assert_eq!(plain, trailing, "尾分隔符不该改变锁名");
        assert_eq!(plain, upper, "盘符/路径大小写不该改变锁名");

        let other = TestTempDir::new("instance-name-other");
        assert_ne!(plain, mutex_name(&other), "不同 data_root 必须是不同的锁");
    }

    #[test]
    fn acquire_succeeds_and_release_allows_reacquire() {
        let dir = TestTempDir::new("instance-acquire");
        let first = acquire(&dir).expect("首次获取应成功");
        // 同进程再获取一次：命名互斥体是跨句柄的，第二个句柄同样看到"已存在"。
        assert!(acquire(&dir).is_err(), "占用期间不得再次获取");
        drop(first);
        drop(acquire(&dir).expect("释放后应能重新获取"));
    }

    /// 全局层必须同进同出：第二次获取是 `AlreadyRunning`（不是 Err），释放后能重新获取。
    #[test]
    fn global_lock_is_exclusive_and_released_on_drop() {
        let name = format!("Local\\SHANIU-test-global-unit-{}", std::process::id());
        let first = acquire_global_named(&name).expect("首次获取");
        assert!(matches!(first, GlobalAcquire::Acquired(_)));
        assert!(
            matches!(
                acquire_global_named(&name).expect("第二次获取"),
                GlobalAcquire::AlreadyRunning
            ),
            "已有一个实例时必须报告 AlreadyRunning，而不是当成错误或再给一把锁"
        );
        drop(first);
        assert!(matches!(
            acquire_global_named(&name).expect("释放后获取"),
            GlobalAcquire::Acquired(_)
        ));
    }

    /// 运行信息往返 + 不同数据目录必须点名两边路径；同一路径的大小写/尾分隔符
    /// 变体不能被当成"不同"（否则每次双击图标都会弹一次提示）。
    #[test]
    fn running_info_round_trips_and_different_root_is_reported() {
        let dir = TestTempDir::new("running-info");
        let path = dir.join("running.json");
        assert!(read_running_info(&path).is_none(), "文件不存在时不该 panic");

        let root = dir.join("data");
        let info = RunningInstance::new(&root);
        write_running_info(&path, &info).unwrap();
        assert_eq!(read_running_info(&path), Some(info.clone()));

        let same_spelling = PathBuf::from(format!(
            "{}\\",
            root.display().to_string().to_uppercase()
        ));
        assert!(
            conflict_message(Some(&info), &same_spelling).is_none(),
            "同一目录的不同写法应该安静激活"
        );

        let other = dir.join("other");
        let message = conflict_message(Some(&info), &other).expect("不同目录必须说明");
        for needle in [
            root.display().to_string(),
            other.display().to_string(),
            "请先退出正在运行的 SHANIU".to_string(),
        ] {
            assert!(message.contains(&needle), "说明缺少「{needle}」：{message}");
        }
    }

    #[test]
    fn restore_window_rejects_null_handle() {
        assert!(!restore_window(std::ptr::null_mut()));
    }

    /// 事件通道本身：监听线程收到 SetEvent 必须回调。
    #[test]
    fn activation_listener_wakes_on_request() {
        let name = format!("Local\\SHANIU-test-activate-unit-{}", std::process::id());
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_activation_listener(&name, move || {
            let _ = tx.send(());
        })
        .expect("注册监听");
        assert!(request_activation(&name), "事件已创建，请求激活必须成功");
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("监听线程必须被唤醒");
    }
}
