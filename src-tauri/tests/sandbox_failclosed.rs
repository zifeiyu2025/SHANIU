//! 健身函数 FF-4（docs/01 §5）：沙箱创建失败时，命令执行数 = 0。
//!
//! 度量方式是**副作用探针**，不是返回值：每种失败形态都给一条"只要真跑起来就会在工作区落一个
//! marker 文件"的命令，然后检查 marker 不存在。只断言 `Err` 证明不了两件事——命令可能已被启动
//! 后才报错，也可能被上层悄悄降级成无沙箱执行；这两态都恰好是 FF-4 要拦的失效形态。
//!
//! 三条不变量：
//! 1) 创建失败 → 返回 Err 且 marker 不存在（执行数 = 0）；
//! 2) 任何失败路径都不得返回 `Ok(Plain(_))`——那等于"沙箱没建起来，照跑不误"；
//! 3) 对照用例：同一条命令在正常 workspace-write 下 marker **必须**出现，
//!    否则 marker 机制本身在空转，前两条就是假绿（FF-2 那次已经踩过）。
//!
//! 失败形态刻意覆盖 token/Job 创建**之后**、进程真正起来**之前**的晚阶段失败（坏 cwd、
//! 坏镜像文件），因为那才是"已经分配了沙箱资源却没建成进程"的最危险窗口。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicUsize, Ordering};

use shaniu_lib::config::SandboxMode;
use shaniu_lib::sandbox::{prepare_spawn, PreparedSpawn, SandboxError};
use tokio::sync::mpsc;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("shaniu-failclosed-{tag}-{}-{n}", id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 「跑起来就必然落盘」的命令行：cmd 会把 marker 写进给定目录。
fn marker_line(marker: &Path) -> String {
    format!("echo executed> \"{}\"", marker.display())
}

/// 调用 prepare_spawn 并强制分类：返回 Err 文本 = 预期内的创建/策略失败；
/// 任何 Ok 分支都是 FF-4 违例，直接 panic 并说明是哪一种降级。
async fn expect_no_execution(program: &str, args: &[&str], cwd: &Path) -> String {
    let (tx, mut rx) = mpsc::channel(16);
    // 正常路径下读线程会往 tx 回传；这里预期不产生任何 stdio
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    match prepare_spawn(SandboxMode::WorkspaceWrite, program, args, cwd, tx) {
        Err(e) => e.to_string(),
        Ok(PreparedSpawn::Plain(_)) => panic!(
            "FF-4 违例：沙箱创建失败却降级为无沙箱执行（Plain）——命令将在不受限状态下运行"
        ),
        Ok(PreparedSpawn::Restricted(mut child)) => {
            let code = child.wait().await.unwrap_or(-1);
            panic!("FF-4 违例：预期创建失败，子进程却已启动（退出码 {code}）")
        }
    }
}

#[tokio::test]
async fn control_normal_sandbox_executes_and_writes_marker() {
    // 假绿探测器：这条不过，下面几条"marker 不存在"全部无意义。
    let ws = unique_dir("control");
    let marker = ws.join("marker.txt");
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let spawn = prepare_spawn(
        SandboxMode::WorkspaceWrite,
        "cmd",
        &["/C", &marker_line(&marker)],
        &ws,
        tx,
    )
    .expect("正常 workspace-write 应可创建沙箱");
    match spawn {
        PreparedSpawn::Restricted(mut child) => {
            assert_eq!(child.wait().await.unwrap(), 0, "沙箱内正常命令退出码应为 0")
        }
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得走无沙箱 Plain 路径"),
    }
    assert!(
        marker.is_file(),
        "对照失败：命令确实执行了却没落盘，marker 探针不成立"
    );
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn nonexistent_program_executes_nothing() {
    let ws = unique_dir("noprog");
    let marker = ws.join("marker.txt");
    let ghost = ws.join("shaniu-does-not-exist.exe");

    let err = expect_no_execution(
        &ghost.display().to_string(),
        &["/C", &marker_line(&marker)],
        &ws,
    )
    .await;

    assert!(
        err.contains("沙箱创建失败"),
        "程序不存在应归为创建失败：{err}"
    );
    assert!(!marker.exists(), "创建失败时命令执行数必须为 0，marker 却出现了");
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn invalid_cwd_executes_nothing_after_token_and_job_created() {
    // cmd 与命令行都合法，只有 cwd 不存在：受限令牌、Job、管道都已建好，
    // 失败发生在 CreateProcessAsUserW——资源已分配、进程未起来的晚阶段窗口。
    let ws = unique_dir("badcwd");
    let marker = std::env::temp_dir().join(format!(
        "shaniu-failclosed-badcwd-marker-{}-{}.txt",
        id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let missing_cwd = ws.join("no-such-subdir");

    let err = expect_no_execution(
        "cmd",
        &["/C", &marker_line(&marker)],
        &missing_cwd,
    )
    .await;

    assert!(err.contains("沙箱创建失败"), "坏 cwd 应归为创建失败：{err}");
    assert!(!marker.exists(), "坏 cwd 下命令执行数必须为 0");
    std::fs::remove_file(&marker).ok();
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn non_executable_image_executes_nothing() {
    let ws = unique_dir("badimg");
    let marker = ws.join("marker.txt");
    let not_an_exe = ws.join("not-an-exe.txt");
    std::fs::write(&not_an_exe, b"just text, not a PE image").unwrap();

    let err = expect_no_execution(
        &not_an_exe.display().to_string(),
        &["/C", &marker_line(&marker)],
        &ws,
    )
    .await;

    assert!(err.contains("沙箱创建失败"), "非可执行镜像应归为创建失败：{err}");
    assert!(!marker.exists(), "非可执行镜像下命令执行数必须为 0");
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn readonly_policy_denies_without_executing() {
    // read-only 属"策略拒绝"而非"创建失败"，同样计入 FF-4 的执行数口径。
    let ws = unique_dir("ro");
    let marker = ws.join("marker.txt");
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let err = prepare_spawn(
        SandboxMode::ReadOnly,
        "cmd",
        &["/C", &marker_line(&marker)],
        &ws,
        tx,
    )
    .err()
    .expect("read-only 必须拒绝命令执行");

    assert!(
        matches!(err, SandboxError::PolicyDenied(_)),
        "read-only 应归为策略拒绝：{err}"
    );
    assert!(!marker.exists(), "策略拒绝时命令执行数必须为 0");
    std::fs::remove_dir_all(&ws).ok();
}
