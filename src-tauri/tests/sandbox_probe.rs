//! 健身函数 FF-3（docs/01 §5）：沙箱隔离探针。
//!
//! 用真实子进程验证 workspace-write 的**行为边界**，而不是断言配置值：
//! 1) 工作区内写入应成功——证明沙箱本身可用（否则"越界被拒"可能是沙箱根本没跑起来）；
//! 2) 工作区外写入应失败——FF-3 的核心不变量。
//!
//! 第 2 条当前标记 `#[ignore]`：受限令牌（剥离特权）保留用户 SID，不做路径级裁决；
//! 越界写封锁依赖 deny-ACE（SH-022），尚未实现。该用例是 SH-019 spike 的度量工具，
//! 实现 SH-022 后去掉 ignore 即为 FF-3 的回归门禁。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicUsize, Ordering};

use shaniu_lib::config::SandboxMode;
use shaniu_lib::sandbox::{prepare_spawn, PreparedSpawn};
use tokio::sync::mpsc;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("shaniu-probe-{tag}-{}-{n}", id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 在 workspace-write 沙箱内执行 `cmd /C <line>`，返回退出码。
async fn run_sandboxed(cwd: &Path, line: &str) -> i32 {
    let (tx, mut rx) = mpsc::channel(256);
    // 排干读线程，避免管道写满阻塞子进程
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let spawn = prepare_spawn(SandboxMode::WorkspaceWrite, "cmd", &["/C", line], cwd, tx)
        .expect("workspace-write 沙箱应可创建（fail-closed 之外的正常路径）");
    match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得退化为无沙箱执行（FF-4）"),
    }
}

#[tokio::test]
async fn sandbox_can_write_inside_workspace() {
    let ws = unique_dir("in");
    let target = ws.join("probe.txt");
    let code = run_sandboxed(&ws, &format!("echo inside> \"{}\"", target.display())).await;

    assert_eq!(code, 0, "工作区内写入的退出码应为 0");
    assert!(target.is_file(), "工作区内写入应真实落盘（证明沙箱可运行）");
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn readonly_mode_refuses_to_spawn() {
    let ws = unique_dir("ro");
    let (tx, _rx) = mpsc::channel(4);
    let err = prepare_spawn(SandboxMode::ReadOnly, "cmd", &["/C", "echo x"], &ws, tx)
        .err()
        .expect("read-only 必须拒绝命令执行");
    assert!(
        matches!(err, shaniu_lib::sandbox::SandboxError::PolicyDenied(_)),
        "read-only 拒绝应归为策略拒绝，而非创建失败：{err}"
    );
    std::fs::remove_dir_all(&ws).ok();
}

/// FF-3 核心不变量：工作区外写入必须失败。///
/// 预期当前失败（deny-ACE 未实现），故 ignore；SH-022 落地后移除 ignore 即成为回归门禁。
#[tokio::test]
#[ignore = "SH-022 deny-ACE 未实现：受限令牌不含路径级写封锁"]
async fn sandbox_denies_write_outside_workspace() {
    let ws = unique_dir("ws");
    let outside = unique_dir("outside");
    let target = outside.join("escaped.txt");

    let code = run_sandboxed(&ws, &format!("echo escaped> \"{}\"", target.display())).await;

    // 无论 cmd 返回码如何，关键断言是：越界文件不得出现
    assert!(
        !target.exists(),
        "工作区外写入成功（cmd 退出码 {code}）：沙箱未提供路径级隔离"
    );
    std::fs::remove_dir_all(&ws).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// 输出**逐行完整性**探针（FF-3 沙箱路径的数据保真一侧）。
///
/// 回归锁：受限沙箱的管道读线程曾把 `BufReader` 建在读循环内部——每轮新建会读走一整块
/// （默认 8KB）却只交出一行，缓冲区里的剩余行随 drop 一起丢弃。症状是"命令输出莫名缺行"：
/// `dir /b` 有 5 个文件，模型只收到第一行，于是反复重跑同一条命令也看不到完整清单。
/// 这类丢失不会让任何一退出码变红，只有把"每一行都必须到齐"写成断言才拦得住。
#[tokio::test]
async fn sandbox_streams_every_output_line() {
    let names = ["alpha.txt", "bravo.txt", "charlie.txt", "delta.txt", "echofiv.txt"];
    let ws = unique_dir("lines");
    for name in names {
        std::fs::write(ws.join(name), "x").unwrap();
    }

    let (tx, mut rx) = mpsc::channel(256);
    let collector = tokio::spawn(async move {
        let mut lines = Vec::new();
        while let Some((_, line)) = rx.recv().await {
            lines.push(line);
        }
        lines
    });
    let spawn = prepare_spawn(SandboxMode::WorkspaceWrite, "cmd", &["/C", "dir /b"], &ws, tx)
        .expect("workspace-write 沙箱应可创建");
    let code = match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得退化为无沙箱执行"),
    };
    assert_eq!(code, 0, "沙箱内 dir /b 应成功");

    let lines = collector.await.expect("读线程不应 panic");
    for name in names {
        assert!(
            lines.iter().any(|l| l == name),
            "输出行 {name} 丢失（沙箱管道只回传了部分行）。实际收到：{lines:?}"
        );
    }
    std::fs::remove_dir_all(&ws).ok();
}
