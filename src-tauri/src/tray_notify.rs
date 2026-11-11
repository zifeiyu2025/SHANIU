//! 关窗提示的系统层出口（A0-2）。
//!
//! 关闭窗口 = 隐藏到托盘，**应用内的一切（含 toast）在 `window.hide()` 之后都不可见**。
//! 所以「程序还在跑」这句话必须由系统层发出：在**已有托盘图标**上挂一个 Shell 气泡
//! （`Shell_NotifyIconW` + `NIF_INFO`），Win10/11 会把它渲染成系统通知。
//!
//! 为什么自创：Tauri 2.12.5 的托盘 API 只有 icon/menu/tooltip/title/visible/rect，
//! 没有 balloon；底层 `tray-icon` 也只用了 `NIM_ADD / NIM_MODIFY(NIF_ICON|NIF_TIP|
//! NIF_STATE) / NIM_DELETE`。而 `tauri-plugin-notification` 走 WinRT toast，对**未安装的
//! 便携 exe** 需要 Start Menu 快捷方式 + AUMID 才能稳定投递，SHANIU 的主产物正是单个
//! 便携 exe。挂在既有托盘图标上的气泡没有这个前提，也不会多出第二个图标。
//!
//! **屏蔽与退回**：Win11 可以按应用或全局关掉通知，也可以开专注助手；那时气泡不显示，
//! 而 `Shell_NotifyIconW` 仍返回 TRUE——我们**无法探测是否被屏蔽**。退回链路是托盘
//! tooltip（常驻、悬停可见）、右键菜单「显示 / 退出」、再次双击 exe 经单实例信号唤回。
//! 这三层都不能替代气泡，只是保证「不是什么都没发生」；报告与注释都按降级记。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIF_REALTIME, NIIF_INFO, NIIF_NOSOUND, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId,
};

/// `tray-icon` 创建托盘消息窗口时固定使用的窗口类名
/// （`tray-icon/src/platform_impl/windows/mod.rs`）。这是我们对上游的唯一形状依赖，
/// 改名时这里会找不到窗口 → 返回 `Err` 并记 warn，不会静默发错对象。
const TRAY_WINDOW_CLASS: &str = "tray_icon_app";

/// `tray-icon` 的 uID 从 1 开始由进程内计数器分配，本应用只建一个托盘图标，正常命中 1。
/// 逐个试只是容错，避免把「计数器细节」写成硬编码。
const UID_CANDIDATES: std::ops::RangeInclusive<u32> = 1..=4;

/// 在已有托盘图标上显示一条气泡。失败返回原因（调用方记 warn，tooltip 兜底）。
pub fn show_balloon(title: &str, body: &str) -> Result<(), String> {
    let Some(hwnd) = find_tray_window() else {
        return Err(format!(
            "找不到托盘窗口（类名 {TRAY_WINDOW_CLASS}，进程 {}）：气泡发不出去，只剩 tooltip",
            unsafe { GetCurrentProcessId() }
        ));
    };
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        // NIF_REALTIME：立刻显示，不排在上一条后面（用户此刻就需要看到）。
        uFlags: NIF_INFO | NIF_REALTIME,
        dwInfoFlags: NIIF_INFO | NIIF_NOSOUND,
        ..Default::default()
    };
    write_utf16(&mut data.szInfoTitle, title);
    write_utf16(&mut data.szInfo, body);
    for uid in UID_CANDIDATES {
        data.uID = uid;
        // 对不存在的 (hwnd, uID) 组合 Shell 返回 FALSE，所以逐个试不会误发给别人。
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            return Ok(());
        }
    }
    Err("Shell_NotifyIconW(NIM_MODIFY, NIF_INFO) 在候选 uID 上全部被拒绝".to_string())
}

/// 「要不要弹、弹什么」的唯一裁决点。
///
/// 只在两种情况弹：有任务在跑（最需要知道「我没退出」的时刻），或本进程首次收托盘
/// （位置说明只说一次）。其余情况一律不弹——天天关窗天天弹，用户会把这个提示关掉，
/// 然后回到零反馈。之后想看，走托盘菜单「后台运行说明」。
pub fn balloon_payload(count: usize, first: bool) -> Option<(&'static str, String)> {
    if count > 0 {
        Some((RUNNING_TITLE, running_body(count)))
    } else if first {
        Some((FIRST_TITLE, FIRST_BODY.to_string()))
    } else {
        None
    }
}

/// 托盘菜单主动查看时的文案：不受「首次 / 有任务」门控，随时可读。
pub fn menu_hint_payload(count: usize) -> (&'static str, String) {
    if count > 0 {
        (RUNNING_TITLE, running_body(count))
    } else {
        (IDLE_TITLE, IDLE_BODY.to_string())
    }
}

const RUNNING_TITLE: &str = "SHANIU 仍在后台运行";
const FIRST_TITLE: &str = "SHANIU 已收进系统托盘";
const IDLE_TITLE: &str = "SHANIU 正在后台运行";
const FIRST_BODY: &str =
    "窗口已隐藏，程序仍在后台运行。双击托盘图标回到界面，右键菜单可退出。";
const IDLE_BODY: &str =
    "窗口已隐藏，程序仍在托盘里。双击托盘图标回到界面，右键菜单可退出。";

fn running_body(count: usize) -> String {
    format!("还有 {count} 项任务在跑；双击托盘图标回到界面，右键菜单可退出。")
}

/// 把字符串写进 Win32 的定长 UTF-16 缓冲：超出截断，余位清零（保证 NUL 结尾）。
fn write_utf16(dst: &mut [u16], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().collect();
    let len = encoded.len().min(dst.len().saturating_sub(1));
    dst[..len].copy_from_slice(&encoded[..len]);
    for slot in &mut dst[len..] {
        *slot = 0;
    }
}

/// 找到本进程托盘图标的消息窗口。
fn find_tray_window() -> Option<HWND> {
    struct Search {
        pid: u32,
        found: Option<HWND>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // LPARAM 携带的是同步调用期间的栈上指针，EnumWindows 返回前一直有效。
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut class = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == TRAY_WINDOW_CLASS {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid == search.pid {
                search.found = Some(hwnd);
                return BOOL::from(false); // 找到即停止枚举
            }
        }
        BOOL::from(true)
    }

    let mut search = Search {
        pid: unsafe { GetCurrentProcessId() },
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

#[cfg(test)]
mod tests {
    use super::{balloon_payload, find_tray_window, menu_hint_payload, write_utf16};

    /// 反向对照：把 `first` 分支去掉，`idle_first_notifies` 必红；把 `count > 0` 分支
    /// 去掉，`running_notifies` 必红。
    #[test]
    fn gate_matches_task_state() {
        let (_, running) = balloon_payload(2, false).expect("有任务时关窗必须提示");
        assert!(running.contains('2'), "提示要带数量，实际：{running}");

        let (_, first) = balloon_payload(0, true).expect("首次收托盘应当提示");
        assert!(first.contains("托盘"), "首次提示要说明图标在托盘，实际：{first}");

        assert!(
            balloon_payload(0, false).is_none(),
            "既没任务也不是首次时不许弹——每次都弹会被用户关掉"
        );
    }

    #[test]
    fn menu_hint_always_answers() {
        assert!(menu_hint_payload(3).1.contains('3'));
        assert!(menu_hint_payload(0).1.contains("托盘"));
    }

    #[test]
    fn utf16_buffer_is_truncated_and_terminated() {
        let mut buffer = [1u16; 8];
        write_utf16(&mut buffer, "abcdefghij");
        assert_eq!(&buffer[..7], "abcdefg".encode_utf16().collect::<Vec<_>>().as_slice());
        assert_eq!(buffer[7], 0, "必须留 NUL 结尾");
    }

    /// 单测进程没有托盘图标：找不到窗口必须是明确的 `None`，而不是瞎猜一个 hwnd。
    #[test]
    fn no_tray_window_in_unit_test_process() {
        assert!(find_tray_window().is_none());
    }
}
