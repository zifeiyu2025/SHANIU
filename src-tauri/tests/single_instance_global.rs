//! 全局单实例 + 二次启动唤回的**真进程**验证（2026-11-11）。
//!
//! 两层单实例里，本文件只验第一层（整个登录会话唯一 UI 实例）与"再启动一次 = 把已有
//! 窗口叫到前台"。数据目录层见 `single_instance_process.rs`。
//!
//! 不 mock：父测试进程自 re-exec 出两个独立进程。`hold` 进程拿全局锁、建一个**真的
//! Win32 窗口**并隐藏/最小化、注册产品的激活监听；`attempt` 进程走产品
//! `handle_second_launch` 的同一段逻辑。
//!
//! 反向对照（撤掉修复必须变红）：
//! - 去掉全局锁：`attempt` 会拿到锁并退出 7，第一条断言立刻失败；
//! - 去掉"叫到前台"（不 SetEvent 或不恢复窗口）：`attempt` 仍退 0，但激活标记不出现
//!   /窗口仍不可见，标记断言失败；
//! - 数据目录不同时不说明：`attempt` 退出码不是 3、提示文件里也找不到两边路径。

use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use shaniu_lib::config::single_instance::{
    self, GlobalAcquire, SecondLaunchOutcome, ACTIVATE_EVENT_NAME,
};
use shaniu_lib::test_support::TestTempDir;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, IsIconic, ShowWindow, SW_HIDE, SW_SHOWMINIMIZED, WINDOW_EX_STYLE,
    WS_OVERLAPPEDWINDOW,
};

const CHILD_ENV: &str = "SHANIU_GLOBAL_PROBE";
const MUTEX_ENV: &str = "SHANIU_GLOBAL_MUTEX";
const EVENT_ENV: &str = "SHANIU_GLOBAL_EVENT";
const ROOT_ENV: &str = "SHANIU_GLOBAL_ROOT";
const INFO_ENV: &str = "SHANIU_GLOBAL_INFO";
const DIR_ENV: &str = "SHANIU_GLOBAL_DIR";
const WINDOW_ENV: &str = "SHANIU_GLOBAL_WINDOW";
const HOLD_MS_ENV: &str = "SHANIU_GLOBAL_HOLD_MS";
const PROBE_TEST: &str = "global_single_instance_probe";

const ACTIVATED_FILE: &str = "activated.txt";
const MESSAGE_FILE: &str = "message.txt";
const READY_FILE: &str = "holder-ready";

/// 子进程角色。父进程正常跑测试时本函数直接返回（不产生副作用）。
#[test]
fn global_single_instance_probe() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var(ROOT_ENV).expect("缺少数据根"));
    let info = PathBuf::from(std::env::var(INFO_ENV).expect("缺少运行信息路径"));
    let dir = PathBuf::from(std::env::var(DIR_ENV).expect("缺少协作目录"));
    let mutex = std::env::var(MUTEX_ENV).expect("缺少互斥体名");
    let event = std::env::var(EVENT_ENV).expect("缺少事件名");
    match mode.as_str() {
        "hold" => hold(&root, &info, &dir, &mutex, &event),
        "attempt" => attempt(&root, &info, &dir, &mutex, &event),
        other => {
            eprintln!("未知子进程模式: {other}");
            std::process::exit(11);
        }
    }
}

/// 首实例：拿全局锁 → 建真窗口并隐藏/最小化 → 写运行信息 → 等激活事件。
fn hold(root: &Path, info: &Path, dir: &Path, mutex: &str, event: &str) {
    let _lock = match single_instance::acquire_global_named(mutex) {
        Ok(GlobalAcquire::Acquired(lock)) => lock,
        Ok(GlobalAcquire::AlreadyRunning) => {
            eprintln!("首实例角色拿到 AlreadyRunning，说明本机已有同名实例");
            std::process::exit(10);
        }
        Err(error) => {
            eprintln!("首实例获取全局锁失败: {error}");
            std::process::exit(11);
        }
    };
    if let Err(error) = single_instance::write_running_info(info, &single_instance::RunningInstance::new(root)) {
        eprintln!("写运行信息失败: {error}");
        std::process::exit(12);
    }

    let hwnd = create_probe_window();
    let minimized = std::env::var(WINDOW_ENV).as_deref() == Ok("minimized");
    unsafe {
        if minimized {
            let _ = ShowWindow(hwnd, SW_SHOWMINIMIZED);
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }

    let raw = hwnd.0 as isize;
    let marker = dir.join(ACTIVATED_FILE);
    if let Err(error) = single_instance::spawn_activation_listener(event, move || {
        let hwnd = HWND(raw as *mut c_void);
        let visible = single_instance::restore_window(raw as *mut c_void);
        let line = format!(
            "activated visible={visible} iconic={}\n",
            unsafe { IsIconic(hwnd).as_bool() }
        );
        // 追加而不是覆盖：不同数据目录的二次启动不该产生第二行。
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&marker)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }) {
        eprintln!("注册激活监听失败: {error}");
        std::process::exit(13);
    }

    std::fs::write(dir.join(READY_FILE), b"ok").expect("写就绪标记");
    let hold_ms = std::env::var(HOLD_MS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(8_000);
    // 窗口归本线程所有：必须泵消息，否则另一线程的 ShowWindow 会等本线程处理
    // WM_SHOWWINDOW 而卡住（这正是"叫不回来"的一种真实形态）。
    pump_messages_for(Duration::from_millis(hold_ms));
    std::process::exit(0);
}

fn pump_messages_for(duration: Duration) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 第二实例：走产品的二次启动收口，按结果给不同退出码（测试据此断言）。
fn attempt(root: &Path, info: &Path, dir: &Path, mutex: &str, event: &str) {
    match single_instance::acquire_global_named(mutex) {
        // 全局层存在时不可能走到这里；走到 = 逆向对照命中（第二个实例被放行）。
        Ok(GlobalAcquire::Acquired(_)) => {
            eprintln!("第二个实例拿到了全局锁（全局层缺失）");
            std::process::exit(7);
        }
        Ok(GlobalAcquire::AlreadyRunning) => {}
        Err(error) => {
            eprintln!("检查全局锁失败: {error}");
            std::process::exit(11);
        }
    }
    let message_path = dir.join(MESSAGE_FILE);
    let outcome = single_instance::handle_second_launch(root, info, event, |message| {
        std::fs::write(&message_path, message).expect("写提示内容");
    });
    let code = match outcome {
        SecondLaunchOutcome::Activated => 0,
        SecondLaunchOutcome::DifferentDataRoot => 3,
        SecondLaunchOutcome::CannotActivate => 4,
    };
    println!("outcome={outcome:?}");
    std::process::exit(code);
}

fn create_probe_window() -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("SHANIU 单实例测试窗口"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            200,
            100,
            None,
            None,
            None,
            None,
        )
        .expect("创建测试窗口失败")
    }
}

fn spawn_child(
    exe: &Path,
    mode: &str,
    mutex: &str,
    event: &str,
    window: &str,
    coords: &TestTempDir,
) -> Command {
    let mut command = Command::new(exe);
    command
        .args(["--exact", PROBE_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, mode)
        .env(MUTEX_ENV, mutex)
        .env(EVENT_ENV, event)
        .env(ROOT_ENV, coords.join("root"))
        .env(INFO_ENV, coords.join("running.json"))
        .env(DIR_ENV, coords.path())
        .env(WINDOW_ENV, window)
        .env(HOLD_MS_ENV, "8000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "等待 {} 超时（{}ms）",
            path.display(),
            timeout.as_millis()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_lines(path: &Path, wanted: usize, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.lines().count() >= wanted {
            return text;
        }
        assert!(
            start.elapsed() < timeout,
            "等待 {} 出现 {wanted} 行超时，当前内容：{text}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn unique_names(tag: &str) -> (String, String) {
    let pid = std::process::id();
    (
        format!("Local\\SHANIU-test-global-{tag}-{pid}"),
        format!("Local\\SHANIU-test-activate-{tag}-{pid}"),
    )
}

/// 隐藏（托盘态）窗口：二次启动必须把它拉回可见；不同数据目录则只说明、不激活。
#[test]
fn second_launch_restores_hidden_window_and_explains_other_data_root() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let exe = std::env::current_exe().expect("取测试二进制路径");
    let coords = TestTempDir::new("global-singleton");
    let (mutex, event) = unique_names("hidden");
    let other_root = TestTempDir::new("global-singleton-other");

    let holder = spawn_child(&exe, "hold", &mutex, &event, "hidden", &coords)
        .spawn()
        .expect("启动首实例子进程");
    wait_for_file(&coords.join(READY_FILE), Duration::from_secs(10));

    // ① 同一数据目录：安静激活，不弹"已在运行"。
    let mut second = spawn_child(&exe, "attempt", &mutex, &event, "hidden", &coords);
    second.env(ROOT_ENV, coords.join("root"));
    let output = second.output().expect("启动二次启动子进程");
    assert_eq!(
        output.status.code(),
        Some(0),
        "同一数据目录的二次启动必须干净退出 0（stdout={}，stderr={}）",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let activated = wait_for_lines(&coords.join(ACTIVATED_FILE), 1, Duration::from_secs(10));
    assert!(
        activated.contains("visible=true") && activated.contains("iconic=false"),
        "隐藏窗口必须被恢复并显示：{activated}"
    );
    assert!(
        !coords.join(MESSAGE_FILE).exists(),
        "同一数据目录不该弹任何提示（用户只是又点了一次图标）"
    );

    // ② 不同数据目录：必须说明"当前运行的是哪个"，且不激活（避免以为新目录已生效）。
    let mut third = spawn_child(&exe, "attempt", &mutex, &event, "hidden", &coords);
    third.env(ROOT_ENV, other_root.path());
    let output = third.output().expect("启动不同数据目录的二次启动子进程");
    assert_eq!(
        output.status.code(),
        Some(3),
        "不同数据目录必须走说明分支（stdout={}，stderr={}）",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let message = std::fs::read_to_string(coords.join(MESSAGE_FILE)).expect("必须给出说明");
    let holder_root = coords.join("root").display().to_string();
    let mine = other_root.path().display().to_string();
    for needle in [holder_root.as_str(), mine.as_str(), "请先退出正在运行的 SHANIU"] {
        assert!(message.contains(needle), "说明里缺少「{needle}」：{message}");
    }
    let after = std::fs::read_to_string(coords.join(ACTIVATED_FILE)).unwrap_or_default();
    assert_eq!(
        after.lines().count(),
        1,
        "不同数据目录不该激活首实例窗口：{after}"
    );

    kill(holder);
}

/// 最小化窗口：二次启动同样要恢复（SW_RESTORE 覆盖最小化与隐藏两种状态）。
#[test]
fn second_launch_restores_minimized_window() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let exe = std::env::current_exe().expect("取测试二进制路径");
    let coords = TestTempDir::new("global-singleton-min");
    let (mutex, event) = unique_names("minimized");

    let holder = spawn_child(&exe, "hold", &mutex, &event, "minimized", &coords)
        .spawn()
        .expect("启动首实例子进程");
    wait_for_file(&coords.join(READY_FILE), Duration::from_secs(10));

    let output = spawn_child(&exe, "attempt", &mutex, &event, "minimized", &coords)
        .output()
        .expect("启动二次启动子进程");
    assert_eq!(
        output.status.code(),
        Some(0),
        "二次启动必须干净退出 0（stderr={}）",
        String::from_utf8_lossy(&output.stderr)
    );
    let activated = wait_for_lines(&coords.join(ACTIVATED_FILE), 1, Duration::from_secs(10));
    assert!(
        activated.contains("iconic=false"),
        "最小化窗口必须被恢复（iconic 仍为 true 说明只退出、没叫回）：{activated}"
    );

    kill(holder);
}

/// 产品入口用的是固定应用标识，不随数据目录变化。
#[test]
fn global_mutex_name_is_fixed_and_independent_of_data_root() {
    assert_eq!(single_instance::GLOBAL_MUTEX_NAME, "Local\\SHANIU-app");
    assert_ne!(
        single_instance::GLOBAL_MUTEX_NAME,
        single_instance::mutex_name(Path::new("D:/SHANIU/data")),
        "全局层不能按数据目录派生，否则两个副本又会同时起来"
    );
    assert_eq!(ACTIVATE_EVENT_NAME, "Local\\SHANIU-activate");
}
