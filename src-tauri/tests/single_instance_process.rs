//! 同 `data_root` 单实例的**真进程**验证（2026-11-11）。
//!
//! 不 mock 互斥体：用测试二进制自 re-exec，让两个独立进程真的去抢同一把命名互斥体。
//! 角色由环境变量分派——`hold` 进程持有互斥体并写就绪标记，`attempt` 进程尝试获取。
//!
//! 反向对照（撤掉修复必须变红）：去掉产品里的互斥体后，第二个 `attempt` 会成功退出 0，
//! 本用例的 `Some(7)` 断言立刻失败。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shaniu_lib::test_support::TestTempDir;

const CHILD_ENV: &str = "SHANIU_SINGLE_INSTANCE_PROBE";
const ROOT_ENV: &str = "SHANIU_SINGLE_INSTANCE_ROOT";
const HOLD_MS_ENV: &str = "SHANIU_SINGLE_INSTANCE_HOLD_MS";
/// 子进程里只跑这一条用例（`--exact` 过滤），避免它再去派生孙进程。
const PROBE_TEST: &str = "single_instance_process_probe";

/// 子进程角色。父进程正常跑测试时本函数直接返回（不产生副作用）。
#[test]
fn single_instance_process_probe() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var(ROOT_ENV).expect("子进程缺少数据目录"));
    match mode.as_str() {
        "hold" => {
            let _lock = match shaniu_lib::config::single_instance::acquire(&root) {
                Ok(lock) => lock,
                Err(message) => {
                    println!("acquire-failed: {message}");
                    std::process::exit(10);
                }
            };
            std::fs::write(root.join("holder-ready"), b"ok").expect("写就绪标记");
            let hold = std::env::var(HOLD_MS_ENV)
                .ok()
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(3_000);
            std::thread::sleep(Duration::from_millis(hold));
            // 绕过 Drop：真实退出路径就是进程消失，句柄由 OS 关闭。
            std::process::exit(0);
        }
        "attempt" => match shaniu_lib::config::single_instance::acquire(&root) {
            Ok(_lock) => {
                println!("acquired");
                std::process::exit(0);
            }
            Err(message) => {
                println!("occupied: {message}");
                std::process::exit(7);
            }
        },
        other => {
            eprintln!("未知子进程模式: {other}");
            std::process::exit(11);
        }
    }
}

fn spawn_child(exe: &Path, root: &Path, mode: &str, hold_ms: u64) -> Command {
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            PROBE_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, mode)
        .env(ROOT_ENV, root)
        .env(HOLD_MS_ENV, hold_ms.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_for(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "等待 {} 超时（{}ms）——持有锁的子进程没有起来",
            path.display(),
            timeout.as_millis()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 同一 `data_root` 的第二个进程必须被拒；不同 `data_root` 必须放行（按目录区分，
/// 不是全局单实例）。
#[test]
fn second_process_on_same_data_root_is_refused_but_other_root_is_not() {
    // 子进程角色只走上面的 probe，不在子进程里再派生孙进程。
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let exe = std::env::current_exe().expect("取测试二进制路径");
    let root = TestTempDir::new("single-instance-proc");
    let other = TestTempDir::new("single-instance-proc-other");

    let mut holder = spawn_child(&exe, &root, "hold", 3_000)
        .spawn()
        .expect("启动持锁子进程");
    wait_for(&root.join("holder-ready"), Duration::from_secs(10));

    let second = spawn_child(&exe, &root, "attempt", 0)
        .output()
        .expect("启动第二个子进程");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert_eq!(
        second.status.code(),
        Some(7),
        "同一数据目录的第二个进程必须失败退出（实际 code={:?}，stdout={stdout}，stderr={}）",
        second.status.code(),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        stdout.contains("occupied") && stdout.contains("已被另一个 SHANIU 实例占用"),
        "拒绝理由必须可读且点名占用：{stdout}"
    );

    let independent = spawn_child(&exe, &other, "attempt", 0)
        .output()
        .expect("启动不同数据目录的子进程");
    assert_eq!(
        independent.status.code(),
        Some(0),
        "不同 data_root 的副本必须能同时运行（stdout={}）",
        String::from_utf8_lossy(&independent.stdout)
    );

    let _ = holder.kill();
    let _ = holder.wait();
}
